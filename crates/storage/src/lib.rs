//! 存储层：SQLite（rusqlite 内置打包），实现详细设计第二章的表结构。
//! 对外只暴露领域类型，不泄漏 SQL 细节。

use bluelink_domain::*;
use rusqlite::{params, Connection};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("数据库错误：{0}")]
    Sql(#[from] rusqlite::Error),
    #[error("序列化错误：{0}")]
    Json(#[from] serde_json::Error),
}

/// 建表语句（与详细设计一致）。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS devices(device_id TEXT PRIMARY KEY, name TEXT, code TEXT, last_seen INTEGER, created_at INTEGER);
CREATE TABLE IF NOT EXISTS conversations(id TEXT PRIMARY KEY, kind TEXT, peer_device_id TEXT, title TEXT,
    last_read_id TEXT, draft TEXT DEFAULT '', created_at INTEGER);
CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY, conversation_id TEXT, sender TEXT, kind TEXT, body TEXT,
    file_id TEXT, reply_to TEXT, status TEXT, created_at INTEGER);
CREATE INDEX IF NOT EXISTS idx_msg_conv ON messages(conversation_id, created_at);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(body, content='messages', content_rowid='rowid');
CREATE TABLE IF NOT EXISTS tags(id INTEGER PRIMARY KEY, name TEXT UNIQUE COLLATE NOCASE, color INTEGER, created_at INTEGER);
CREATE TABLE IF NOT EXISTS message_tags(message_id TEXT, tag_id INTEGER, PRIMARY KEY(message_id, tag_id));
CREATE TABLE IF NOT EXISTS message_marks(message_id TEXT, kind TEXT, created_at INTEGER, PRIMARY KEY(message_id, kind));
CREATE TABLE IF NOT EXISTS files(id TEXT PRIMARY KEY, name TEXT, size INTEGER, sha256 TEXT, path TEXT, mime TEXT);
CREATE TABLE IF NOT EXISTS transfers(id TEXT PRIMARY KEY, file_id TEXT, direction TEXT, peer_id TEXT, total INTEGER,
    ranges TEXT, state TEXT, updated_at INTEGER);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS reading_positions(conversation_id TEXT PRIMARY KEY, message_id TEXT, updated_at INTEGER);
CREATE TABLE IF NOT EXISTS categories(color INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS message_category(message_id TEXT PRIMARY KEY, color INTEGER NOT NULL, note TEXT);
INSERT OR IGNORE INTO categories(color,name) VALUES (0,'灰色'),(1,'红色'),(2,'橙色'),(3,'黄色'),(4,'绿色'),(5,'青色'),(6,'蓝色'),(7,'紫色'),(8,'粉色');
"#;

/// 存储句柄（内部互斥，单连接模型足够桌面端使用）。
pub struct Storage {
    conn: Mutex<Connection>,
}

fn kind_str(k: ConversationKind) -> &'static str {
    match k {
        ConversationKind::Peer => "peer",
        ConversationKind::Ai => "ai",
    }
}
fn sender_str(s: Sender) -> &'static str {
    match s {
        Sender::Me => "me",
        Sender::Peer => "peer",
        Sender::Ai => "ai",
        Sender::System => "system",
    }
}
fn msg_kind_str(k: MessageKind) -> &'static str {
    match k {
        MessageKind::Text => "text",
        MessageKind::File => "file",
        MessageKind::System => "system",
        MessageKind::ImageGroup => "imagegroup",
    }
}
fn status_str(s: MessageStatus) -> &'static str {
    match s {
        MessageStatus::Sending => "sending",
        MessageStatus::Sent => "sent",
        MessageStatus::Delivered => "delivered",
        MessageStatus::Received => "received",
        MessageStatus::Failed => "failed",
    }
}

impl Storage {
    /// 打开（或创建）数据库并执行建表。path 为 ":memory:" 时用内存库（测试用）。
    pub fn open(path: &str) -> Result<Self, StorageError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let s = Self {
            conn: Mutex::new(conn),
        };
        s.migrate_legacy_tags()?;
        Ok(s)
    }

    /// 一次性迁移（v2.2）：把旧的自由标签按颜色并入彩签分类——
    /// 每个颜色的分类名取该颜色下用得最多的旧标签名；一条消息有多个旧标签时，
    /// 取创建时间最早的那个标签的颜色为其分类（颜色相同自然合并）。
    /// 以 settings 标记防重复执行；老表数据保留不删（旧命令仍可读）。
    pub fn migrate_legacy_tags(&self) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let marker: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='cat_migrated'",
                [],
                |r| r.get(0),
            )
            .ok();
        if marker.as_deref() == Some("1") {
            return Ok(());
        }
        let tx = conn.transaction()?;
        // 分类名：每个颜色取消息数最多的旧标签名
        for color in 0..=8i64 {
            let name: Option<String> = tx
                .query_row(
                    "SELECT t.name FROM tags t JOIN message_tags mt ON mt.tag_id=t.id
                     WHERE t.color=?1 GROUP BY t.id ORDER BY COUNT(*) DESC, t.created_at ASC LIMIT 1",
                    params![color],
                    |r| r.get(0),
                )
                .ok();
            if let Some(name) = name {
                tx.execute(
                    "UPDATE categories SET name=?1 WHERE color=?2",
                    params![name, color],
                )?;
            }
        }
        // 归属：每条消息取其最早创建的旧标签的颜色
        let mut stmt = tx.prepare(
            "SELECT mt.message_id, t.color FROM message_tags mt JOIN tags t ON t.id=mt.tag_id
             ORDER BY mt.message_id, t.created_at ASC, t.id ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut seen = std::collections::HashSet::new();
        let mut winners: Vec<(String, i64)> = Vec::new();
        for row in rows {
            let (mid, color) = row?;
            if seen.insert(mid.clone()) {
                winners.push((mid, color));
            }
        }
        drop(stmt);
        for (mid, color) in winners {
            tx.execute(
                "INSERT OR IGNORE INTO message_category(message_id,color,note) VALUES(?1,?2,NULL)",
                params![mid, color],
            )?;
        }
        tx.execute(
            "INSERT INTO settings(key,value) VALUES('cat_migrated','1')
             ON CONFLICT(key) DO UPDATE SET value='1'",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("存储锁中毒")
    }

    // ---------- 设备 ----------
    pub fn upsert_device(&self, d: &Device) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO devices(device_id,name,code,last_seen,created_at) VALUES(?1,?2,?3,?4,?4)
             ON CONFLICT(device_id) DO UPDATE SET name=?2, code=?3, last_seen=?4",
            params![d.device_id, d.name, d.code, d.last_seen],
        )?;
        Ok(())
    }

    // ---------- 会话 ----------
    pub fn create_conversation(&self, c: &Conversation) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO conversations(id,kind,peer_device_id,title,last_read_id,draft,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![c.id, kind_str(c.kind), c.peer_device_id, c.title, c.last_read_id, c.draft, c.created_at],
        )?;
        Ok(())
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,kind,peer_device_id,title,last_read_id,draft,created_at FROM conversations ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| {
            Ok(Conversation {
                id: r.get(0)?,
                kind: if r.get::<_, String>(1)? == "ai" {
                    ConversationKind::Ai
                } else {
                    ConversationKind::Peer
                },
                peer_device_id: r.get(2)?,
                title: r.get(3)?,
                last_read_id: r.get(4)?,
                draft: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// ㉕ 会话改名（AI 会话自定义命名）：只改标题，其余字段不动。
    pub fn rename_conversation(&self, id: &str, title: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE conversations SET title=?2 WHERE id=?1",
            params![id, title],
        )?;
        Ok(())
    }

    pub fn save_draft(&self, conversation_id: &str, draft: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE conversations SET draft=?2 WHERE id=?1",
            params![conversation_id, draft],
        )?;
        Ok(())
    }

    // ---------- 消息 ----------
    /// 插入消息，返回本机落库序号（rowid）——显示顺序的唯一依据，调用方
    /// 应回填到 Message.seq 再发给前端（实时消息也要有真 seq 才能参与分页）。
    pub fn insert_message(&self, m: &Message) -> Result<i64, StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO messages(id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![m.id, m.conversation_id, sender_str(m.sender), msg_kind_str(m.kind), m.body, m.file_id, m.reply_to, status_str(m.status), m.created_at],
        )?;
        let rowid: i64 = tx.last_insert_rowid();
        if m.kind == MessageKind::Text {
            tx.execute(
                "INSERT INTO messages_fts(rowid, body) VALUES(?1, ?2)",
                params![rowid, m.body],
            )?;
        }
        tx.commit()?;
        Ok(rowid)
    }

    /// 删除一条消息（供 AI 重新生成替换旧回复用），连带清理全文索引与标记。
    /// 删除整个会话及其全部消息（AI 会话删除用）：消息的 FTS、彩签、
    /// 收藏/置顶附属行一并清掉，不留孤儿。
    pub fn delete_conversation(&self, id: &str) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // FTS 清理一条语句完成（评审修正）：旧版先把全部 rowid 拉回
        // 内存再逐条 DELETE，会话越大越慢。
        tx.execute(
            "DELETE FROM messages_fts WHERE rowid IN (SELECT rowid FROM messages WHERE conversation_id=?1)",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM message_tags WHERE message_id IN (SELECT id FROM messages WHERE conversation_id=?1)",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM message_marks WHERE message_id IN (SELECT id FROM messages WHERE conversation_id=?1)",
            params![id],
        )?;
        tx.execute("DELETE FROM messages WHERE conversation_id=?1", params![id])?;
        tx.execute("DELETE FROM conversations WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_message(&self, id: &str) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        if let Ok(rowid) =
            tx.query_row("SELECT rowid FROM messages WHERE id=?1", params![id], |r| {
                r.get::<_, i64>(0)
            })
        {
            tx.execute("DELETE FROM messages_fts WHERE rowid=?1", params![rowid])?;
        }
        tx.execute("DELETE FROM message_tags WHERE message_id=?1", params![id])?;
        tx.execute("DELETE FROM message_marks WHERE message_id=?1", params![id])?;
        tx.execute("DELETE FROM messages WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_message_status(
        &self,
        id: &str,
        status: MessageStatus,
    ) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE messages SET status=?2 WHERE id=?1",
            params![id, status_str(status)],
        )?;
        Ok(())
    }

    /// 只更新消息正文（图片组接收端逐张追加清单用）：不动 rowid，
    /// 消息的落库序号（显示顺序）保持首次插入时的位置。
    pub fn update_message_body(&self, id: &str, body: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute("UPDATE messages SET body=?2 WHERE id=?1", params![id, body])?;
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT rowid,id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at FROM messages WHERE id=?1")?;
        let mut rows = stmt.query_map(params![id], map_message)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// 按入账序（rowid）正序取会话消息；before 为分页游标（取 seq 小于它的
    /// 消息），配合 limit 取最近一页后在内存中翻正。排序只认本机落库序号，
    /// 永不按发送方墙钟 created_at——双机时钟差下墙钟序必然乱（v2.5.1 定案）。
    pub fn list_messages(
        &self,
        conversation_id: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rowid,id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at FROM messages
             WHERE conversation_id=?1 AND (?2 IS NULL OR rowid<?2) ORDER BY rowid DESC LIMIT ?3")?;
        let rows = stmt.query_map(params![conversation_id, before, limit], map_message)?;
        let mut v: Vec<Message> = rows.collect::<Result<_, _>>()?;
        v.reverse();
        Ok(v)
    }

    /// 子串搜索，返回命中消息（按时间倒序，上限 200）。
    /// 不用 FTS5：默认 unicode61 分词器把整段中文当一个词条，
    /// 搜「复验」匹配不到「重建后复验消息」；本地消息量小，LIKE 子串匹配更符合聊天搜索预期。
    pub fn search_messages(&self, query: &str) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        // 转义 LIKE 通配符，用户输入按字面子串匹配
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut stmt = conn.prepare(
            "SELECT rowid,id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at
             FROM messages
             WHERE body LIKE ?1 ESCAPE '\\' ORDER BY created_at DESC, rowid DESC LIMIT 200",
        )?;
        let rows = stmt.query_map(params![pattern], map_message)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---------- 标签 ----------
    /// 整体替换一条消息的标签集合；标签名按需求规范化（去首尾空格，空名忽略）。
    pub fn set_message_tags(&self, message_id: &str, names: &[String]) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM message_tags WHERE message_id=?1",
            params![message_id],
        )?;
        for raw in names {
            let name = raw.trim();
            if name.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO tags(name,color,created_at) VALUES(?1,0,?2)",
                params![name, now_ms()],
            )?;
            let tag_id: i64 = tx.query_row(
                "SELECT id FROM tags WHERE name=?1 COLLATE NOCASE",
                params![name],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO message_tags(message_id,tag_id) VALUES(?1,?2)",
                params![message_id, tag_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_message_tags(&self, message_id: &str) -> Result<Vec<String>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.name FROM tags t JOIN message_tags mt ON mt.tag_id=t.id WHERE mt.message_id=?1 ORDER BY t.name")?;
        let rows = stmt.query_map(params![message_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 全部标签及计数（标签总览页）。
    pub fn list_tags(&self) -> Result<Vec<Tag>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT t.id,t.name,t.color,COUNT(mt.message_id) FROM tags t LEFT JOIN message_tags mt ON mt.tag_id=t.id
             GROUP BY t.id ORDER BY COUNT(mt.message_id) DESC, t.name")?;
        let rows = stmt.query_map([], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
                count: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 设置标签颜色（调色板序号，0 为默认）。标签不存在时静默无操作。
    pub fn set_tag_color(&self, name: &str, color: i64) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tags SET color=?1 WHERE name=?2 COLLATE NOCASE",
            params![color, name],
        )?;
        Ok(())
    }

    /// 某标签下的全部消息（按时间正序，供抽屉列表与定位）。
    pub fn list_messages_by_tag(&self, tag_name: &str) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.rowid,m.id,m.conversation_id,m.sender,m.kind,m.body,m.file_id,m.reply_to,m.status,m.created_at
             FROM messages m JOIN message_tags mt ON mt.message_id=m.id JOIN tags t ON t.id=mt.tag_id
             WHERE t.name=?1 COLLATE NOCASE ORDER BY m.created_at, m.rowid")?;
        let rows = stmt.query_map(params![tag_name], map_message)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---------- 彩签分类（v2.2 新模型：颜色即分类） ----------
    /// 全部 9 个分类及各自消息数（按颜色序号排列，空分类也在内）。
    pub fn list_categories(&self) -> Result<Vec<Category>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.color, c.name, COUNT(mc.message_id) FROM categories c
             LEFT JOIN message_category mc ON mc.color=c.color
             GROUP BY c.color ORDER BY c.color",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Category {
                color: r.get(0)?,
                name: r.get(1)?,
                count: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 给分类改名（改名后全部分类成员同步显示新名，因为名字只存一份）。
    pub fn rename_category(&self, color: i64, name: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE categories SET name=?1 WHERE color=?2",
            params![name.trim(), color],
        )?;
        Ok(())
    }

    /// 设置一条消息的彩签分类；None 表示清除。备注为空串时按无备注存。
    pub fn set_message_category(
        &self,
        message_id: &str,
        category: Option<(i64, Option<String>)>,
    ) -> Result<(), StorageError> {
        let conn = self.lock();
        match category {
            Some((color, note)) => {
                let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
                conn.execute(
                    "INSERT INTO message_category(message_id,color,note) VALUES(?1,?2,?3)
                     ON CONFLICT(message_id) DO UPDATE SET color=?2, note=?3",
                    params![message_id, color, note],
                )?;
            }
            None => {
                conn.execute(
                    "DELETE FROM message_category WHERE message_id=?1",
                    params![message_id],
                )?;
            }
        }
        Ok(())
    }

    /// 一条消息的彩签归属（无则 None）。
    pub fn get_message_category(
        &self,
        message_id: &str,
    ) -> Result<Option<MessageCategory>, StorageError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT color, note FROM message_category WHERE message_id=?1")?;
        let mut rows = stmt.query_map(params![message_id], |r| {
            Ok(MessageCategory {
                color: r.get(0)?,
                note: r.get(1)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// 某分类下的全部消息及备注（时间正序，供悬停列表与总览抽屉）。
    pub fn list_messages_by_category(
        &self,
        color: i64,
    ) -> Result<Vec<CategorizedMessage>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.rowid,m.id,m.conversation_id,m.sender,m.kind,m.body,m.file_id,m.reply_to,m.status,m.created_at, mc.note
             FROM messages m JOIN message_category mc ON mc.message_id=m.id
             WHERE mc.color=?1 ORDER BY m.created_at, m.rowid",
        )?;
        let rows = stmt.query_map(params![color], |r| {
            Ok(CategorizedMessage {
                message: map_message(r)?,
                note: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---------- 收藏/置顶 ----------
    /// 切换标记状态，返回切换后是否已标记。
    pub fn toggle_mark(&self, message_id: &str, kind: MarkKind) -> Result<bool, StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_marks WHERE message_id=?1 AND kind=?2)",
            params![message_id, kind.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            tx.execute(
                "DELETE FROM message_marks WHERE message_id=?1 AND kind=?2",
                params![message_id, kind.as_str()],
            )?;
        } else {
            tx.execute(
                "INSERT INTO message_marks(message_id,kind,created_at) VALUES(?1,?2,?3)",
                params![message_id, kind.as_str(), now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(!exists)
    }

    pub fn list_marked(&self, kind: MarkKind) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.rowid,m.id,m.conversation_id,m.sender,m.kind,m.body,m.file_id,m.reply_to,m.status,m.created_at
             FROM messages m JOIN message_marks mk ON mk.message_id=m.id WHERE mk.kind=?1 ORDER BY m.created_at, m.rowid")?;
        let rows = stmt.query_map(params![kind.as_str()], map_message)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---------- 文件与传输 ----------
    pub fn save_file(&self, f: &FileMeta) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO files(id,name,size,sha256,path,mime) VALUES(?1,?2,?3,?4,?5,?6)",
            params![f.id, f.name, f.size as i64, f.sha256, f.path, f.mime],
        )?;
        Ok(())
    }

    /// 按文件 ID 取元信息。
    pub fn get_file(&self, file_id: &str) -> Result<Option<FileMeta>, StorageError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id,name,size,sha256,path,mime FROM files WHERE id=?1")?;
        let mut rows = stmt.query_map(params![file_id], |r| {
            Ok(FileMeta {
                id: r.get(0)?,
                name: r.get(1)?,
                size: r.get::<_, i64>(2)? as u64,
                sha256: r.get(3)?,
                path: r.get(4)?,
                mime: r.get(5)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// 回写文件落地路径（接收完成时 .part 转正后调用）。
    pub fn update_file_path(&self, file_id: &str, path: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE files SET path=?1 WHERE id=?2",
            params![path, file_id],
        )?;
        Ok(())
    }

    /// 消息统计（设置页存储区）：条数与正文总字节数。
    pub fn message_stats(&self) -> Result<(i64, i64), StorageError> {
        let conn = self.lock();
        let row = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(body)),0) FROM messages",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(row)
    }

    /// 清空全部聊天记录（消息、全文索引、彩签归属、收藏/置顶一并清，会话与设置保留）。
    pub fn clear_all_messages(&self) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM message_category", [])?;
        tx.execute("DELETE FROM message_tags", [])?;
        tx.execute("DELETE FROM message_marks", [])?;
        tx.execute("DELETE FROM messages", [])?;
        // 外部内容 FTS：用 rebuild 按已清空的内容表重建索引（结果为空索引）
        tx.execute(
            "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    // 参数与 transfers 表字段一一对应，拆参数结构体反而更绕，这里豁免参数个数检查
    #[allow(clippy::too_many_arguments)]
    pub fn save_transfer(
        &self,
        id: &str,
        file_id: &str,
        direction: &str,
        peer_id: &str,
        total: u64,
        ranges: &Ranges,
        state: &str,
    ) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO transfers(id,file_id,direction,peer_id,total,ranges,state,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![id, file_id, direction, peer_id, total as i64, serde_json::to_string(ranges)?, state, now_ms()])?;
        Ok(())
    }

    /// ⑳ 接收完成的三笔回写合并成一个事务：消息状态、文件落地路径、
    /// 传输收尾。此前三笔各走一次自动提交（各带一次落盘同步），在本机
    /// 磁盘慢时合计约 250 ms，全算在「收完→发出校验确认」的等待里；
    /// 合并后只剩一次提交。
    #[allow(clippy::too_many_arguments)]
    pub fn finish_received_file(
        &self,
        msg_id: &str,
        file_id: &str,
        final_path: &str,
        transfer_id: &str,
        peer_id: &str,
        total: u64,
        ranges: &Ranges,
    ) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE messages SET status=?2 WHERE id=?1",
            params![msg_id, status_str(MessageStatus::Delivered)],
        )?;
        tx.execute(
            "UPDATE files SET path=?1 WHERE id=?2",
            params![final_path, file_id],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO transfers(id,file_id,direction,peer_id,total,ranges,state,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![transfer_id, file_id, "in", peer_id, total as i64, serde_json::to_string(ranges)?, "done", now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_transfer_ranges(&self, id: &str) -> Result<Option<Ranges>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT ranges FROM transfers WHERE id=?1")?;
        let mut rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(s) => Ok(Some(serde_json::from_str(&s?)?)),
            None => Ok(None),
        }
    }

    // ---------- 设置 ----------
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT value FROM settings WHERE key=?1")?;
        let mut rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(v) => Ok(Some(v?)),
            None => Ok(None),
        }
    }
}

fn map_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        seq: r.get(0)?,
        id: r.get(1)?,
        conversation_id: r.get(2)?,
        sender: match r.get::<_, String>(3)?.as_str() {
            "me" => Sender::Me,
            "ai" => Sender::Ai,
            "system" => Sender::System,
            _ => Sender::Peer,
        },
        kind: match r.get::<_, String>(4)?.as_str() {
            "file" => MessageKind::File,
            "system" => MessageKind::System,
            "imagegroup" => MessageKind::ImageGroup,
            _ => MessageKind::Text,
        },
        body: r.get(5)?,
        file_id: r.get(6)?,
        reply_to: r.get(7)?,
        status: match r.get::<_, String>(8)?.as_str() {
            "sending" => MessageStatus::Sending,
            "sent" => MessageStatus::Sent,
            "delivered" => MessageStatus::Delivered,
            "failed" => MessageStatus::Failed,
            _ => MessageStatus::Received,
        },
        created_at: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 准备() -> (Storage, Conversation) {
        let s = Storage::open(":memory:").unwrap();
        let c = Conversation {
            id: "c1".into(),
            kind: ConversationKind::Peer,
            peer_device_id: Some("d2".into()),
            title: "对端".into(),
            draft: String::new(),
            last_read_id: None,
            created_at: 1,
        };
        s.create_conversation(&c).unwrap();
        (s, c)
    }

    fn 消息(id: &str, conv: &str, body: &str, ts: i64) -> Message {
        Message {
            id: id.into(),
            conversation_id: conv.into(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: body.into(),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sent,
            created_at: ts,
            seq: 0,
        }
    }

    /// 性能基线（v1.1.0 实测用）：1 万条消息下分页/搜索/按标签列取的耗时，
    /// 对照需求 N 类预算（定位与搜索 ≤500ms）。断言用宽松上限防 CI 抖动，实际数值以打印为准。
    #[test]
    fn 万条消息性能基线() {
        use std::time::Instant;
        let (s, c) = 准备();
        for i in 0..10_000 {
            let body = if i % 100 == 0 {
                format!("第{i}条 关键事项 记录")
            } else {
                format!("第{i}条 普通聊天内容")
            };
            s.insert_message(&消息(
                &format!("p{i}"),
                &c.id,
                &body,
                1_700_000_000_000 + i as i64,
            ))
            .unwrap();
            if i % 100 == 0 {
                s.set_message_tags(&format!("p{i}"), &["基线".to_string()])
                    .unwrap();
            }
        }
        let t0 = Instant::now();
        let page = s.list_messages(&c.id, None, 200).unwrap();
        let d_page = t0.elapsed();
        assert_eq!(page.len(), 200);

        let t0 = Instant::now();
        let hits = s.search_messages("关键事项").unwrap();
        let d_search = t0.elapsed();
        assert_eq!(hits.len(), 100);

        let t0 = Instant::now();
        let tagged = s.list_messages_by_tag("基线").unwrap();
        let d_tag = t0.elapsed();
        assert_eq!(tagged.len(), 100);

        eprintln!(
            "性能基线（1 万条）：分页 200 条 {:?}，全文搜索 {:?}（命中 {}），按标签列取 {:?}（{} 条）",
            d_page,
            d_search,
            hits.len(),
            d_tag,
            tagged.len()
        );
        for (name, d) in [("分页", d_page), ("搜索", d_search), ("标签", d_tag)] {
            assert!(d.as_millis() < 500, "{name} 耗时 {:?} 超过 500ms 预算", d);
        }
    }

    #[test]
    fn 消息分页与搜索() {
        let (s, c) = 准备();
        for i in 0..30 {
            s.insert_message(&消息(
                &format!("m{i}"),
                &c.id,
                &format!("第{i}条 苹果"),
                i as i64,
            ))
            .unwrap();
        }
        let page = s.list_messages(&c.id, None, 20).unwrap();
        assert_eq!(page.len(), 20);
        assert_eq!(page[0].id, "m10");
        // 分页游标 = 入账序号 seq（v2.5.1 起，不再是 created_at）
        let older = s.list_messages(&c.id, Some(page[0].seq), 20).unwrap();
        assert_eq!(older.len(), 10);
        let hits = s.search_messages("苹果").unwrap();
        assert_eq!(hits.len(), 30);
        // 中文子串必须能命中：整段无空格文本里的任意片段（FTS5 分词做不到，回归防线）
        s.insert_message(&消息("mx", &c.id, "重建后复验消息", 100))
            .unwrap();
        assert_eq!(s.search_messages("复验").unwrap().len(), 1);
        assert_eq!(s.search_messages("建后复").unwrap().len(), 1);
        assert_eq!(s.search_messages("不存在的词").unwrap().len(), 0);
    }

    #[test]
    fn 标签与标记全流程() {
        let (s, c) = 准备();
        s.insert_message(&消息("m1", &c.id, "合同终版", 1)).unwrap();
        s.insert_message(&消息("m2", &c.id, "别的内容", 2)).unwrap();
        s.set_message_tags("m1", &["工作".into(), " 重要 ".into()])
            .unwrap();
        assert_eq!(
            s.get_message_tags("m1").unwrap(),
            vec!["工作".to_string(), "重要".to_string()]
        );
        let tags = s.list_tags().unwrap();
        assert_eq!(tags.len(), 2);
        assert!(tags.iter().all(|t| t.count == 1));
        let by_tag = s.list_messages_by_tag("工作").unwrap();
        assert_eq!(by_tag.len(), 1);
        assert!(s.toggle_mark("m1", MarkKind::Favorite).unwrap());
        assert_eq!(s.list_marked(MarkKind::Favorite).unwrap().len(), 1);
        assert!(!s.toggle_mark("m1", MarkKind::Favorite).unwrap());
        assert_eq!(s.list_marked(MarkKind::Favorite).unwrap().len(), 0);
    }

    /// ⑲ 回归：置顶/取消置顶只许动 message_marks——会话列表与消息列表
    /// 的完整性、条数、顺序在置顶前后必须逐项不变（用户实报「置顶后
    /// 列表里看不到、重启不恢复」，数据层先钉死写入不污染列表字段）。
    #[test]
    fn 置顶不污染会话与消息列表() {
        let (s, c) = 准备();
        // 再建一个 AI 会话，凑齐「多会话 + 中间一条消息置顶」的场景
        let ai = Conversation {
            id: "ai-default".into(),
            kind: ConversationKind::Ai,
            peer_device_id: None,
            title: "AI".into(),
            draft: String::new(),
            last_read_id: None,
            created_at: 2,
        };
        s.create_conversation(&ai).unwrap();
        s.insert_message(&消息("m1", &c.id, "第一条", 1)).unwrap();
        s.insert_message(&消息("m2", &ai.id, "中间条", 2)).unwrap();
        s.insert_message(&消息("m3", &ai.id, "第三条", 3)).unwrap();
        let convs_before: Vec<String> = s
            .list_conversations()
            .unwrap()
            .iter()
            .map(|x| x.id.clone())
            .collect();
        assert_eq!(
            convs_before,
            vec!["c1".to_string(), "ai-default".to_string()]
        );
        // 置顶中间一条（AI 会话内）
        assert!(s.toggle_mark("m2", MarkKind::Pin).unwrap());
        let pinned = s.list_marked(MarkKind::Pin).unwrap();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].id, "m2");
        assert_eq!(pinned[0].conversation_id, "ai-default");
        // 置顶后：会话列表条数/顺序不变，两个会话的消息条数不变
        let convs_after: Vec<String> = s
            .list_conversations()
            .unwrap()
            .iter()
            .map(|x| x.id.clone())
            .collect();
        assert_eq!(convs_after, convs_before);
        assert_eq!(s.list_messages(&ai.id, None, 200).unwrap().len(), 2);
        assert_eq!(s.list_messages(&c.id, None, 200).unwrap().len(), 1);
        // 取消置顶后同样不变，且置顶集为空
        assert!(!s.toggle_mark("m2", MarkKind::Pin).unwrap());
        assert_eq!(s.list_marked(MarkKind::Pin).unwrap().len(), 0);
        assert_eq!(s.list_conversations().unwrap().len(), 2);
        assert_eq!(s.list_messages(&ai.id, None, 200).unwrap().len(), 2);
    }

    #[test]
    fn 彩签分类全流程() {
        let (s, c) = 准备();
        s.insert_message(&消息("m1", &c.id, "合同终版", 1)).unwrap();
        s.insert_message(&消息("m2", &c.id, "另一条", 2)).unwrap();
        // 初始 9 个分类都在，计数为 0
        let cats = s.list_categories().unwrap();
        assert_eq!(cats.len(), 9);
        assert!(cats.iter().all(|x| x.count == 0));
        assert_eq!(cats[1].name, "红色");
        // 归类 + 备注
        s.set_message_category("m1", Some((1, Some(" 终版合同 ".into()))))
            .unwrap();
        s.set_message_category("m2", Some((1, None))).unwrap();
        let mc = s.get_message_category("m1").unwrap().unwrap();
        assert_eq!(mc.color, 1);
        assert_eq!(mc.note.as_deref(), Some("终版合同"));
        let cats = s.list_categories().unwrap();
        assert_eq!(cats[1].count, 2);
        // 改名全类同步
        s.rename_category(1, "文件").unwrap();
        assert_eq!(s.list_categories().unwrap()[1].name, "文件");
        // 按分类列取（带备注）
        let list = s.list_messages_by_category(1).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].message.id, "m1");
        assert_eq!(list[0].note.as_deref(), Some("终版合同"));
        // 改归属：一人一类，换色即移走
        s.set_message_category("m1", Some((4, None))).unwrap();
        assert_eq!(s.list_categories().unwrap()[1].count, 1);
        assert_eq!(s.list_categories().unwrap()[4].count, 1);
        // 清除
        s.set_message_category("m1", None).unwrap();
        assert!(s.get_message_category("m1").unwrap().is_none());
        // 清空记录连带清归属
        s.clear_all_messages().unwrap();
        assert_eq!(s.message_stats().unwrap().0, 0);
        assert_eq!(s.list_categories().unwrap()[1].count, 0);
    }

    #[test]
    fn 老标签按颜色迁移() {
        let (s, c) = 准备();
        s.insert_message(&消息("m1", &c.id, "甲", 1)).unwrap();
        s.insert_message(&消息("m2", &c.id, "乙", 2)).unwrap();
        // 旧模型：自由标签 + 每签一色；m1 有两签（红的文件、用得少；蓝的其它）
        s.set_message_tags("m1", &["文件".into(), "其它".into()])
            .unwrap();
        s.set_message_tags("m2", &["文件".into()]).unwrap();
        s.set_tag_color("文件", 1).unwrap();
        s.set_tag_color("其它", 6).unwrap();
        // open 时迁移已在空库跑过，这里手动重置标记再迁一次（模拟老库首次升级打开）
        s.set_setting("cat_migrated", "0").unwrap();
        s.migrate_legacy_tags().unwrap();
        let cats = s.list_categories().unwrap();
        assert_eq!(cats[1].name, "文件"); // 红色分类名取自旧标签「文件」
        assert_eq!(cats[1].count, 2); // m1（最早标签为文件）、m2 都归红
        assert_eq!(s.get_message_category("m1").unwrap().unwrap().color, 1);
        // 迁移幂等：再跑不变
        s.set_setting("cat_migrated", "0").unwrap();
        s.migrate_legacy_tags().unwrap();
        assert_eq!(s.list_categories().unwrap()[1].count, 2);
    }

    #[test]
    fn 草稿与设置() {
        let (s, c) = 准备();
        s.save_draft(&c.id, "写了一半").unwrap();
        assert_eq!(s.list_conversations().unwrap()[0].draft, "写了一半");
        s.set_setting("download_dir", "/tmp/dl").unwrap();
        assert_eq!(
            s.get_setting("download_dir").unwrap().as_deref(),
            Some("/tmp/dl")
        );
    }

    /// 显示顺序只认本机入账序号（2026-11-20 v2.5.1）：created_at 故意倒序、
    /// 同毫秒混杂插入，list_messages 必须按入账序返回；游标分页按 seq 向前
    /// 翻，同毫秒消息一条不吞。
    #[test]
    fn 消息按入账序而非墙钟序() {
        let (s, c) = 准备();
        // 入账顺序 m1..m5，created_at 分别 5000/1000/3000/3000/2000（模拟对端
        // 时钟差与同毫秒连发）：按旧口径 ORDER BY created_at 会得到
        // m2,m5,m3/m4(并列序不稳),m1 的乱序。
        let stamps = [5000_i64, 1000, 3000, 3000, 2000];
        for (i, ts) in stamps.iter().enumerate() {
            let m = 消息(&format!("m{}", i + 1), &c.id, "x", *ts);
            s.insert_message(&m).unwrap();
        }
        let all = s.list_messages(&c.id, None, 10).unwrap();
        let ids: Vec<&str> = all.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m1", "m2", "m3", "m4", "m5"]);
        // seq 即 rowid：严格递增且与返回一致
        assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
        // 分页：先取最近 2 条，再以首条 seq 为游标向前翻——同毫秒的 m3 必须在内
        let page1 = s.list_messages(&c.id, None, 2).unwrap();
        assert_eq!(
            page1.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["m4", "m5"]
        );
        let page2 = s.list_messages(&c.id, Some(page1[0].seq), 2).unwrap();
        assert_eq!(
            page2.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["m2", "m3"]
        );
        let page3 = s.list_messages(&c.id, Some(page2[0].seq), 2).unwrap();
        assert_eq!(
            page3.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["m1"]
        );
    }
}
