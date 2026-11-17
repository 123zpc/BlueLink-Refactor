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
        Ok(Self {
            conn: Mutex::new(conn),
        })
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

    pub fn save_draft(&self, conversation_id: &str, draft: &str) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE conversations SET draft=?2 WHERE id=?1",
            params![conversation_id, draft],
        )?;
        Ok(())
    }

    // ---------- 消息 ----------
    pub fn insert_message(&self, m: &Message) -> Result<(), StorageError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO messages(id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![m.id, m.conversation_id, sender_str(m.sender), msg_kind_str(m.kind), m.body, m.file_id, m.reply_to, status_str(m.status), m.created_at],
        )?;
        if m.kind == MessageKind::Text {
            let rowid: i64 = tx.query_row(
                "SELECT rowid FROM messages WHERE id=?1",
                params![m.id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO messages_fts(rowid, body) VALUES(?1, ?2)",
                params![rowid, m.body],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 删除一条消息（供 AI 重新生成替换旧回复用），连带清理全文索引与标记。
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

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at FROM messages WHERE id=?1")?;
        let mut rows = stmt.query_map(params![id], map_message)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// 按时间正序取会话消息；before 为分页游标（取该时间之前的消息），配合 limit 取最近一页后在内存中翻正。
    pub fn list_messages(
        &self,
        conversation_id: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id,conversation_id,sender,kind,body,file_id,reply_to,status,created_at FROM messages
             WHERE conversation_id=?1 AND (?2 IS NULL OR created_at<?2) ORDER BY created_at DESC LIMIT ?3")?;
        let rows = stmt.query_map(params![conversation_id, before, limit], map_message)?;
        let mut v: Vec<Message> = rows.collect::<Result<_, _>>()?;
        v.reverse();
        Ok(v)
    }

    /// 全文搜索（FTS5），返回命中消息（按时间倒序，上限 200）。
    pub fn search_messages(&self, query: &str) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.id,m.conversation_id,m.sender,m.kind,m.body,m.file_id,m.reply_to,m.status,m.created_at
             FROM messages_fts f JOIN messages m ON m.rowid=f.rowid
             WHERE messages_fts MATCH ?1 ORDER BY m.created_at DESC LIMIT 200")?;
        let rows = stmt.query_map(params![query], map_message)?;
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

    /// 某标签下的全部消息（按时间正序，供抽屉列表与定位）。
    pub fn list_messages_by_tag(&self, tag_name: &str) -> Result<Vec<Message>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.id,m.conversation_id,m.sender,m.kind,m.body,m.file_id,m.reply_to,m.status,m.created_at
             FROM messages m JOIN message_tags mt ON mt.message_id=m.id JOIN tags t ON t.id=mt.tag_id
             WHERE t.name=?1 COLLATE NOCASE ORDER BY m.created_at")?;
        let rows = stmt.query_map(params![tag_name], map_message)?;
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
            "SELECT m.id,m.conversation_id,m.sender,m.kind,m.body,m.file_id,m.reply_to,m.status,m.created_at
             FROM messages m JOIN message_marks mk ON mk.message_id=m.id WHERE mk.kind=?1 ORDER BY m.created_at")?;
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
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        sender: match r.get::<_, String>(2)?.as_str() {
            "me" => Sender::Me,
            "ai" => Sender::Ai,
            "system" => Sender::System,
            _ => Sender::Peer,
        },
        kind: match r.get::<_, String>(3)?.as_str() {
            "file" => MessageKind::File,
            "system" => MessageKind::System,
            _ => MessageKind::Text,
        },
        body: r.get(4)?,
        file_id: r.get(5)?,
        reply_to: r.get(6)?,
        status: match r.get::<_, String>(7)?.as_str() {
            "sending" => MessageStatus::Sending,
            "sent" => MessageStatus::Sent,
            "delivered" => MessageStatus::Delivered,
            "failed" => MessageStatus::Failed,
            _ => MessageStatus::Received,
        },
        created_at: r.get(8)?,
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
        let older = s
            .list_messages(&c.id, Some(page[0].created_at), 20)
            .unwrap();
        assert_eq!(older.len(), 10);
        let hits = s.search_messages("苹果").unwrap();
        assert_eq!(hits.len(), 30);
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
}
