# 详细设计（阶段 4 定稿）

> 出品：架构组 + 开发组 ｜ 版本：v1.0 ｜ 状态：已定稿（2026-11-17 夜间，依发起人全权授权推进）｜ 依据：REQUIREMENTS.md v0.2、ARCHITECTURE.md、DESIGN.md

## 一、协议 v2（bluelink-protocol）

### 1.1 帧格式（面向字节流的传输层通用）

```
| 4 字节 payload 长度（u32 小端） | 1 字节帧类型 | payload（长度-1 字节） |
```

- payload 上限 16 MiB；超限断开并报协议错误。
- 控制类帧 payload 为 UTF-8 JSON（serde_json）；文件分片帧 payload 为二进制结构（见 1.3）。

### 1.2 帧类型与消息

| 类型码 | 名称 | 方向 | payload |
|---|---|---|---|
| 0x01 | Hello | 发起方→接收方 | `{device_id, device_name, protocol:2, code}`（code 为对方连接码，接收方校验） |
| 0x02 | HelloAck | 接收方→发起方 | `{device_id, device_name, accepted:bool, reason?}` |
| 0x03 | ChatText | 双向 | `{msg_id, conversation_hint?, text, reply_to?, ts}` |
| 0x04 | Delivered | 双向 | `{msg_id}` 已送达回执 |
| 0x05 | Typing | 双向 | `{typing:bool}` |
| 0x10 | FileOffer | 双向 | `{transfer_id, msg_id, name, size, sha256, mime?}` |
| 0x11 | FileAccept | 双向 | `{transfer_id, received_ranges:[[start,end],...]}`（断点续传：已收区间） |
| 0x12 | FileChunk | 发送方→接收方 | 二进制：`transfer_id(16B) + offset(u64) + flags(u8, bit0=已压缩) + data` |
| 0x13 | FileDone | 发送方→接收方 | `{transfer_id}` 分片发完 |
| 0x14 | FileVerify | 接收方→发送方 | `{transfer_id, ok:bool}` 全文件 SHA-256 校验结果 |
| 0x15 | FileCancel | 双向 | `{transfer_id}` |
| 0x20 | AiProxyRequest | 无网方→有网方 | `{request_id, messages:[...], model?}` |
| 0x21 | AiProxyChunk | 有网方→无网方 | `{request_id, delta}` 流式增量 |
| 0x22 | AiProxyDone | 有网方→无网方 | `{request_id, error?}` |
| 0x30 | Ping / 0x31 Pong | 双向 | `{}` 30 秒心跳，90 秒无响应判定断开 |

### 1.3 传输规则

- 分片大小 256 KiB；发送方按 FileAccept 返回的缺失区间顺序发送；接收方每片落盘 `.part` 并更新区间记录（内存区间集 + 每 16 片或 1 秒落库）。
- 压缩：flags.bit0。仅当文件扩展名不在已知压缩格式表（zip/7z/rar/gz/jpg/png/mp4/mp3/pdf 等）且分片可压率抽样 < 0.9 时启用 flate2 压缩；文本类控制帧不压缩（体积小）。
- 连接码：6 位数字 = SHA-256(device_id) 前 8 字节转整数 mod 1,000,000，格式化为 3+3。
- 校验失败（FileVerify.ok=false）：接收方删除 .part 与区间记录，发送方收到后从零重发一次，再失败报错终止。

## 二、数据库结构（SQLite，bluelink-storage）

```sql
devices(device_id TEXT PK, name TEXT, code TEXT, last_seen INTEGER, created_at INTEGER);
conversations(id TEXT PK, kind TEXT, peer_device_id TEXT, title TEXT,
              last_read_id TEXT, draft TEXT DEFAULT '', created_at INTEGER);
messages(id TEXT PK, conversation_id TEXT, sender TEXT, kind TEXT, body TEXT,
         file_id TEXT, reply_to TEXT, status TEXT, created_at INTEGER);
INDEX idx_msg_conv ON messages(conversation_id, created_at);
VIRTUAL TABLE messages_fts USING fts5(body, content='messages', content_rowid='rowid');
tags(id INTEGER PK, name TEXT UNIQUE COLLATE NOCASE, color INTEGER, created_at INTEGER);
message_tags(message_id TEXT, tag_id INTEGER, PK(message_id, tag_id));
message_marks(message_id TEXT, kind TEXT, created_at INTEGER, PK(message_id, kind)); -- kind: favorite|pin
files(id TEXT PK, name TEXT, size INTEGER, sha256 TEXT, path TEXT, mime TEXT);
transfers(id TEXT PK, file_id TEXT, direction TEXT, peer_id TEXT, total INTEGER,
          ranges TEXT, state TEXT, updated_at INTEGER);
settings(key TEXT PK, value TEXT);
reading_positions(conversation_id TEXT PK, message_id TEXT, updated_at INTEGER);
```

- 标签/收藏/置顶统一走 MessageMark 思路：tags 独立表 + message_tags 关联；favorite/pin 走 message_marks。定位链路只认 message_id（见 ARCHITECTURE 附录）。

## 三、IPC 定稿（Tauri 命令与事件）

命令（前端→后端）：`get_identity`、`start_listening`、`stop_listening`、`connect_to{code_or_addr}`、`disconnect`、`send_text{conversation_id,text,reply_to}`、`send_file{conversation_id,path}`、`cancel_transfer{id}`、`list_conversations`、`list_messages{conversation_id,before?,limit}`、`search_messages{query}`、`set_message_tags{message_id,tags[]}`、`list_tags`、`list_messages_by_tag{tag}`、`toggle_mark{message_id,kind}`、`list_marked{kind}`、`save_draft{conversation_id,text}`、`ai_send{conversation_id,text}`、`ai_stop`、`ai_regenerate{message_id}`、`get_settings`、`set_setting{key,value}`、`test_ai_connection`。

事件（后端→前端）：`connection_state`、`message_new`、`message_status`、`transfer_progress`、`transfer_state`、`ai_chunk`、`ai_done`、`ai_error`、`peer_typing`。

## 四、模块与依赖

- crates：domain（纯类型）/ protocol（帧编解码）/ storage（rusqlite bundled）/ transport（trait + TCP + 进程内环回；BLE 后端按平台在 app 层装配）/ ai（reqwest + SSE 解析）/ services（聊天、传输引擎、标签、AI 编排）。
- 关键依赖：tokio、serde/serde_json、rusqlite(bundled)、uuid、sha2、flate2(rust_backend)、reqwest(rustls)、thiserror。版本以工作区统一声明为准。
- 错误：库内用 thiserror 定义类型化错误；services 层转成给前端的中文错误字符串（规范见 REQUIREMENTS 错误文案）。
