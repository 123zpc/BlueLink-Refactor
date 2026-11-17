//! 应用服务层：把协议、传输、存储、AI 编排成一个可用的节点（Node）。
//! 桌面端（Tauri）与演示 CLI 都只调用这一层，不直接碰底层 crate。

use bluelink_ai::{AiClient, AiConfig, ChatMessage};
use bluelink_domain::*;
use bluelink_protocol::{Control, FileChunk, Frame, CHUNK_SIZE};
use bluelink_storage::Storage;
use bluelink_transport::Conn;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, oneshot, Mutex};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("存储错误：{0}")]
    Storage(#[from] bluelink_storage::StorageError),
    #[error("传输错误：{0}")]
    Transport(#[from] bluelink_transport::TransportError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("尚未连接对方设备")]
    NotConnected,
    #[error("连接码不正确")]
    BadCode,
    #[error("对方拒绝了连接")]
    Rejected,
    #[error("AI 错误：{0}")]
    Ai(#[from] bluelink_ai::AiError),
    #[error("{0}")]
    Other(String),
}

/// 服务层事件（Tauri 层订阅后转发给前端，事件名与详细设计第三章对应）。
#[derive(Debug, Clone)]
pub enum Event {
    ConnectionChanged {
        connected: bool,
        peer_name: Option<String>,
    },
    MessageNew(Message),
    MessageStatus {
        id: String,
        status: MessageStatus,
    },
    TransferProgress {
        transfer_id: String,
        received: u64,
        total: u64,
    },
    TransferState {
        transfer_id: String,
        state: String,
    },
    AiChunk {
        conversation_id: String,
        delta: String,
    },
    AiDone {
        conversation_id: String,
        message: Message,
    },
    AiError {
        conversation_id: String,
        error: String,
    },
    PeerTyping(bool),
}

/// 已知压缩格式（这些扩展名的分片直传，不再压缩）。
const KNOWN_COMPRESSED: &[&str] = &[
    "zip", "7z", "rar", "gz", "bz2", "xz", "jpg", "jpeg", "png", "gif", "webp", "mp4", "mkv",
    "mp3", "aac", "pdf", "docx", "xlsx", "pptx",
];

/// 计算文件的 SHA-256（流式读取，不把整文件读入内存）。
pub fn sha256_file(path: &Path) -> Result<String, ServiceError> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// 由文件哈希与对端设备派生稳定的传输 ID：同一文件重发时复用，实现断点续传。
pub fn derive_transfer_id(file_sha: &str, peer_id: &str) -> String {
    let d = Sha256::digest(format!("{file_sha}:{peer_id}").as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
    )
}

/// 尝试压缩分片：已知压缩格式或压不动时返回 None（直传）。
fn try_compress(data: &[u8], file_name: &str) -> Option<Vec<u8>> {
    let ext = Path::new(file_name).extension()?.to_str()?.to_lowercase();
    if KNOWN_COMPRESSED.contains(&ext.as_str()) {
        return None;
    }
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).ok()?;
    let out = enc.finish().ok()?;
    if out.len() < data.len() {
        Some(out)
    } else {
        None
    }
}

fn decompress(data: &[u8]) -> Result<Vec<u8>, ServiceError> {
    use flate2::read::ZlibDecoder;
    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

/// 接收中的文件状态。
struct RecvState {
    name: String,
    total: u64,
    sha256: String,
    msg_id: String,
    part_path: PathBuf,
    ranges: Ranges,
    written: u64,
}

/// 一个对端会话。
struct PeerSession {
    conn: Arc<Conn>,
    peer: Device,
    conversation_id: String,
}

/// 节点：本机身份 + 存储 + 当前连接 + AI 配置。
pub struct Node {
    pub device_id: String,
    pub device_name: String,
    pub storage: Arc<Storage>,
    download_dir: PathBuf,
    peer: Mutex<Option<Arc<PeerSession>>>,
    events: broadcast::Sender<Event>,
    ai_cfg: Mutex<Option<AiConfig>>,
    accept_waiters: Mutex<HashMap<String, oneshot::Sender<Ranges>>>,
    verify_waiters: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    recv_files: Mutex<HashMap<String, RecvState>>,
    proxy_waiters: Mutex<HashMap<String, tokio::sync::mpsc::Sender<Result<String, String>>>>,
}

impl Node {
    /// 创建节点。identity_path 中持久化设备 ID，保证重启后连接码不变。
    pub fn new(data_dir: &Path, device_name: &str) -> Result<Arc<Self>, ServiceError> {
        std::fs::create_dir_all(data_dir)?;
        let download_dir = data_dir.join("downloads");
        std::fs::create_dir_all(&download_dir)?;
        let id_path = data_dir.join("device_id.txt");
        let device_id = if id_path.exists() {
            std::fs::read_to_string(&id_path)?.trim().to_string()
        } else {
            let id = new_id();
            std::fs::write(&id_path, &id)?;
            id
        };
        let storage = Arc::new(Storage::open(
            data_dir
                .join("bluelink.db")
                .to_str()
                .unwrap_or("bluelink.db"),
        )?);
        let (events, _) = broadcast::channel(512);
        Ok(Arc::new(Self {
            device_id,
            device_name: device_name.to_string(),
            storage,
            download_dir,
            peer: Mutex::new(None),
            events,
            ai_cfg: Mutex::new(None),
            accept_waiters: Mutex::new(HashMap::new()),
            verify_waiters: Mutex::new(HashMap::new()),
            recv_files: Mutex::new(HashMap::new()),
            proxy_waiters: Mutex::new(HashMap::new()),
        }))
    }

    /// 本机连接码。
    pub fn my_code(&self) -> String {
        connection_code(&self.device_id)
    }

    /// 下载目录（接收文件落地处）。
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    /// 订阅事件。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    /// 设置 AI 配置（None 表示未配置）。
    pub async fn set_ai_config(&self, cfg: Option<AiConfig>) {
        *self.ai_cfg.lock().await = cfg;
    }

    async fn session(&self) -> Result<Arc<PeerSession>, ServiceError> {
        self.peer
            .lock()
            .await
            .clone()
            .ok_or(ServiceError::NotConnected)
    }

    /// 是否已连接。
    pub async fn is_connected(&self) -> bool {
        self.peer.lock().await.is_some()
    }

    /// 在已建立的字节流上完成握手并接管连接（宿主侧 host=true 时校验对方给出的连接码）。
    pub async fn attach(
        self: &Arc<Self>,
        conn: Conn,
        host: bool,
        code_from_peer_or_target: &str,
    ) -> Result<Device, ServiceError> {
        let conn = Arc::new(conn);
        let peer = if host {
            // 宿主侧：等对方 Hello，校验连接码
            match conn.recv().await? {
                Frame::Control(Control::Hello {
                    device_id,
                    device_name,
                    code,
                    ..
                }) => {
                    if code != self.my_code() {
                        let _ = conn
                            .send(&Frame::Control(Control::HelloAck {
                                device_id: self.device_id.clone(),
                                device_name: self.device_name.clone(),
                                accepted: false,
                                reason: Some("连接码不正确".into()),
                            }))
                            .await;
                        return Err(ServiceError::BadCode);
                    }
                    conn.send(&Frame::Control(Control::HelloAck {
                        device_id: self.device_id.clone(),
                        device_name: self.device_name.clone(),
                        accepted: true,
                        reason: None,
                    }))
                    .await?;
                    Device {
                        device_id,
                        name: device_name,
                        code: String::new(),
                        last_seen: now_ms(),
                    }
                }
                _ => return Err(ServiceError::Other("握手失败：未收到 Hello".into())),
            }
        } else {
            // 发起侧：发 Hello（带目标连接码），等 Ack
            conn.send(&Frame::Control(Control::Hello {
                device_id: self.device_id.clone(),
                device_name: self.device_name.clone(),
                protocol: 2,
                code: code_from_peer_or_target.to_string(),
            }))
            .await?;
            match conn.recv().await? {
                Frame::Control(Control::HelloAck {
                    device_id,
                    device_name,
                    accepted,
                    ..
                }) => {
                    if !accepted {
                        return Err(ServiceError::Rejected);
                    }
                    Device {
                        device_id,
                        name: device_name,
                        code: String::new(),
                        last_seen: now_ms(),
                    }
                }
                _ => return Err(ServiceError::Other("握手失败：未收到 HelloAck".into())),
            }
        };

        // 落库设备与会话（会话 ID 以对端设备 ID 派生，稳定唯一）
        let mut peer_dev = peer.clone();
        peer_dev.code = connection_code(&peer.device_id);
        self.storage.upsert_device(&peer_dev)?;
        let conversation_id = format!("peer-{}", peer.device_id);
        self.storage.create_conversation(&Conversation {
            id: conversation_id.clone(),
            kind: ConversationKind::Peer,
            peer_device_id: Some(peer.device_id.clone()),
            title: peer.name.clone(),
            draft: String::new(),
            last_read_id: None,
            created_at: now_ms(),
        })?;

        let session = Arc::new(PeerSession {
            conn: conn.clone(),
            peer,
            conversation_id,
        });
        *self.peer.lock().await = Some(session.clone());
        self.emit(Event::ConnectionChanged {
            connected: true,
            peer_name: Some(session.peer.name.clone()),
        });

        // 接收循环
        let peer_out = session.peer.clone();
        let node = self.clone();
        tokio::spawn(async move { node.recv_loop(session).await });
        Ok(peer_out)
    }

    /// 断开当前连接。
    pub async fn disconnect(&self) {
        *self.peer.lock().await = None;
        self.emit(Event::ConnectionChanged {
            connected: false,
            peer_name: None,
        });
    }

    async fn recv_loop(self: Arc<Self>, session: Arc<PeerSession>) {
        while let Ok(frame) = session.conn.recv().await {
            if let Err(e) = self.handle_frame(&session, frame).await {
                eprintln!("处理帧出错：{e}");
            }
        }
        // 连接断开：清理会话并通知
        let mut guard = self.peer.lock().await;
        if let Some(cur) = &*guard {
            if Arc::ptr_eq(cur, &session) {
                *guard = None;
                self.emit(Event::ConnectionChanged {
                    connected: false,
                    peer_name: None,
                });
            }
        }
    }

    async fn handle_frame(
        self: &Arc<Self>,
        session: &Arc<PeerSession>,
        frame: Frame,
    ) -> Result<(), ServiceError> {
        match frame {
            Frame::Control(Control::Ping) => {
                session.conn.send(&Frame::Control(Control::Pong)).await?;
            }
            Frame::Control(Control::Pong)
            | Frame::Control(Control::Hello { .. })
            | Frame::Control(Control::HelloAck { .. }) => {}
            Frame::Control(Control::Typing { typing }) => {
                self.emit(Event::PeerTyping(typing));
            }
            Frame::Control(Control::ChatText {
                msg_id,
                text,
                reply_to,
                ts,
            }) => {
                let m = Message {
                    id: msg_id.clone(),
                    conversation_id: session.conversation_id.clone(),
                    sender: Sender::Peer,
                    kind: MessageKind::Text,
                    body: text,
                    file_id: None,
                    reply_to,
                    status: MessageStatus::Received,
                    created_at: ts,
                };
                self.storage.insert_message(&m)?;
                self.emit(Event::MessageNew(m));
                session
                    .conn
                    .send(&Frame::Control(Control::Delivered { msg_id }))
                    .await?;
            }
            Frame::Control(Control::Delivered { msg_id }) => {
                self.storage
                    .update_message_status(&msg_id, MessageStatus::Delivered)?;
                self.emit(Event::MessageStatus {
                    id: msg_id,
                    status: MessageStatus::Delivered,
                });
            }
            Frame::Control(Control::FileOffer {
                transfer_id,
                msg_id,
                name,
                size,
                sha256,
                mime,
            }) => {
                self.on_file_offer(session, transfer_id, msg_id, name, size, sha256, mime)
                    .await?;
            }
            Frame::Control(Control::FileAccept {
                transfer_id,
                received,
            }) => {
                if let Some(tx) = self.accept_waiters.lock().await.remove(&transfer_id) {
                    let _ = tx.send(received);
                }
            }
            Frame::Chunk(chunk) => {
                self.on_file_chunk(session, chunk).await?;
            }
            Frame::Control(Control::FileDone { transfer_id }) => {
                self.on_file_done(session, transfer_id).await?;
            }
            Frame::Control(Control::FileVerify { transfer_id, ok }) => {
                if let Some(tx) = self.verify_waiters.lock().await.remove(&transfer_id) {
                    let _ = tx.send(ok);
                }
            }
            Frame::Control(Control::FileCancel { transfer_id }) => {
                self.emit(Event::TransferState {
                    transfer_id,
                    state: "cancelled".into(),
                });
            }
            Frame::Control(Control::AiProxyRequest {
                request_id,
                messages,
                ..
            }) => {
                let node = self.clone();
                let session2 = session.clone();
                tokio::spawn(async move {
                    let _ = node.serve_ai_proxy(session2, request_id, messages).await;
                });
            }
            Frame::Control(Control::AiProxyChunk { request_id, delta }) => {
                self.handle_ai_proxy_frame(Control::AiProxyChunk { request_id, delta })
                    .await;
            }
            Frame::Control(Control::AiProxyDone { request_id, error }) => {
                self.handle_ai_proxy_frame(Control::AiProxyDone { request_id, error })
                    .await;
            }
        }
        Ok(())
    }

    // ---------- 聊天 ----------
    /// 发送文本消息，返回落库后的消息。
    pub async fn send_text(
        &self,
        text: &str,
        reply_to: Option<String>,
    ) -> Result<Message, ServiceError> {
        let session = self.session().await?;
        let m = Message {
            id: new_id(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: reply_to.clone(),
            status: MessageStatus::Sending,
            created_at: now_ms(),
        };
        self.storage.insert_message(&m)?;
        session
            .conn
            .send(&Frame::Control(Control::ChatText {
                msg_id: m.id.clone(),
                text: m.body.clone(),
                reply_to,
                ts: m.created_at,
            }))
            .await?;
        self.storage
            .update_message_status(&m.id, MessageStatus::Sent)?;
        self.emit(Event::MessageStatus {
            id: m.id.clone(),
            status: MessageStatus::Sent,
        });
        Ok(self.storage.get_message(&m.id)?.unwrap_or(m))
    }

    /// 发送正在输入状态。
    pub async fn send_typing(&self, typing: bool) {
        if let Ok(s) = self.session().await {
            let _ = s
                .conn
                .send(&Frame::Control(Control::Typing { typing }))
                .await;
        }
    }

    // ---------- 文件传输 ----------
    /// 发送文件：流式分片、断点续传、选择性压缩。返回传输 ID。
    pub async fn send_file(&self, path: &Path) -> Result<String, ServiceError> {
        let session = self.session().await?;
        let meta = std::fs::metadata(path)?;
        let size = meta.len();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| ServiceError::Other("文件名无效".into()))?;
        let sha = sha256_file(path)?;
        let transfer_id = derive_transfer_id(&sha, &session.peer.device_id);
        let file_id = new_id();
        self.storage.save_file(&FileMeta {
            id: file_id.clone(),
            name: name.clone(),
            size,
            sha256: sha.clone(),
            path: path.to_string_lossy().to_string(),
            mime: None,
        })?;
        let msg = Message {
            id: new_id(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::File,
            body: name.clone(),
            file_id: Some(file_id),
            reply_to: None,
            status: MessageStatus::Sending,
            created_at: now_ms(),
        };
        self.storage.insert_message(&msg)?;
        self.emit(Event::MessageNew(msg.clone()));

        // 等待对方的已收区间（续传）
        let (tx, rx) = oneshot::channel();
        self.accept_waiters
            .lock()
            .await
            .insert(transfer_id.clone(), tx);
        session
            .conn
            .send(&Frame::Control(Control::FileOffer {
                transfer_id: transfer_id.clone(),
                msg_id: msg.id.clone(),
                name: name.clone(),
                size,
                sha256: sha,
                mime: None,
            }))
            .await?;
        let received = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| ServiceError::Other("等待对方接收确认超时".into()))?
            .map_err(|_| ServiceError::Other("对方接收通道已关闭".into()))?;
        let missing = missing_ranges(&received, size);
        self.emit(Event::TransferProgress {
            transfer_id: transfer_id.clone(),
            received: covered_bytes(&received),
            total: size,
        });

        // 按缺失区间流式发送
        let mut file = std::fs::File::open(path)?;
        let mut sent_total = covered_bytes(&received);
        for (start, end) in missing {
            let mut offset = start;
            file.seek(SeekFrom::Start(start))?;
            while offset < end {
                let want = ((end - offset) as usize).min(CHUNK_SIZE);
                let mut buf = vec![0u8; want];
                file.read_exact(&mut buf)?;
                let (compressed, data) = match try_compress(&buf, &name) {
                    Some(c) => (true, c),
                    None => (false, buf),
                };
                session
                    .conn
                    .send(&Frame::Chunk(FileChunk {
                        transfer_id: transfer_id.clone(),
                        offset,
                        compressed,
                        data,
                    }))
                    .await?;
                offset += want as u64;
                sent_total += want as u64;
                self.emit(Event::TransferProgress {
                    transfer_id: transfer_id.clone(),
                    received: sent_total,
                    total: size,
                });
            }
        }
        // 等校验结果
        let (vtx, vrx) = oneshot::channel();
        self.verify_waiters
            .lock()
            .await
            .insert(transfer_id.clone(), vtx);
        session
            .conn
            .send(&Frame::Control(Control::FileDone {
                transfer_id: transfer_id.clone(),
            }))
            .await?;
        let ok = tokio::time::timeout(std::time::Duration::from_secs(60), vrx)
            .await
            .map_err(|_| ServiceError::Other("等待校验结果超时".into()))?
            .map_err(|_| ServiceError::Other("校验通道已关闭".into()))?;
        if ok {
            self.storage
                .update_message_status(&msg.id, MessageStatus::Delivered)?;
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "done".into(),
            });
        } else {
            self.storage
                .update_message_status(&msg.id, MessageStatus::Failed)?;
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "verify_failed".into(),
            });
        }
        Ok(transfer_id)
    }

    // 参数与 FileOffer 帧字段一一对应，豁免参数个数检查
    #[allow(clippy::too_many_arguments)]
    async fn on_file_offer(
        &self,
        session: &Arc<PeerSession>,
        transfer_id: String,
        msg_id: String,
        name: String,
        size: u64,
        sha256: String,
        _mime: Option<String>,
    ) -> Result<(), ServiceError> {
        // 恢复已有进度：传输记录 + .part 文件都在才算数
        let part_path = self.download_dir.join(format!("{transfer_id}.part"));
        let mut ranges = self
            .storage
            .load_transfer_ranges(&transfer_id)?
            .unwrap_or_default();
        if !part_path.exists() {
            ranges.clear();
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&part_path)?;
        file.set_len(size)?;
        self.recv_files.lock().await.insert(
            transfer_id.clone(),
            RecvState {
                name: name.clone(),
                total: size,
                sha256: sha256.clone(),
                msg_id: msg_id.clone(),
                part_path,
                ranges: ranges.clone(),
                written: covered_bytes(&ranges),
            },
        );
        let file_id = new_id();
        self.storage.save_file(&FileMeta {
            id: file_id.clone(),
            name: name.clone(),
            size,
            sha256,
            path: String::new(),
            mime: None,
        })?;
        let m = Message {
            id: msg_id,
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Peer,
            kind: MessageKind::File,
            body: name,
            file_id: Some(file_id),
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
        };
        self.storage.insert_message(&m)?;
        self.emit(Event::MessageNew(m));
        self.storage.save_transfer(
            &transfer_id,
            "",
            "in",
            &session.peer.device_id,
            size,
            &ranges,
            "receiving",
        )?;
        session
            .conn
            .send(&Frame::Control(Control::FileAccept {
                transfer_id,
                received: ranges,
            }))
            .await?;
        Ok(())
    }

    async fn on_file_chunk(
        &self,
        _session: &Arc<PeerSession>,
        chunk: FileChunk,
    ) -> Result<(), ServiceError> {
        let mut guard = self.recv_files.lock().await;
        let st = guard
            .get_mut(&chunk.transfer_id)
            .ok_or_else(|| ServiceError::Other("收到未知传输的分片".into()))?;
        let data = if chunk.compressed {
            decompress(&chunk.data)?
        } else {
            chunk.data
        };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&st.part_path)?;
        f.seek(SeekFrom::Start(chunk.offset))?;
        f.write_all(&data)?;
        st.ranges = merge_ranges(
            [
                st.ranges.clone(),
                vec![(chunk.offset, chunk.offset + data.len() as u64)],
            ]
            .concat(),
        );
        st.written = covered_bytes(&st.ranges);
        let (tid, written, total, ranges) = (
            chunk.transfer_id.clone(),
            st.written,
            st.total,
            st.ranges.clone(),
        );
        drop(guard);
        self.storage
            .save_transfer(&tid, "", "in", "", total, &ranges, "receiving")?;
        self.emit(Event::TransferProgress {
            transfer_id: tid,
            received: written,
            total,
        });
        Ok(())
    }

    async fn on_file_done(
        &self,
        session: &Arc<PeerSession>,
        transfer_id: String,
    ) -> Result<(), ServiceError> {
        let st = self
            .recv_files
            .lock()
            .await
            .remove(&transfer_id)
            .ok_or_else(|| ServiceError::Other("完成信号对应未知传输".into()))?;
        let actual = sha256_file(&st.part_path)?;
        let ok = actual == st.sha256;
        if ok {
            let _ = self
                .storage
                .update_message_status(&st.msg_id, MessageStatus::Delivered);
            let final_path = self.download_dir.join(&st.name);
            std::fs::rename(&st.part_path, &final_path)?;
            self.storage.save_transfer(
                &transfer_id,
                "",
                "in",
                &session.peer.device_id,
                st.total,
                &st.ranges,
                "done",
            )?;
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "done".into(),
            });
        } else {
            let _ = std::fs::remove_file(&st.part_path);
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "verify_failed".into(),
            });
        }
        session
            .conn
            .send(&Frame::Control(Control::FileVerify { transfer_id, ok }))
            .await?;
        Ok(())
    }

    // ---------- AI ----------
    /// 确保存在 AI 会话，返回会话 ID。
    pub fn ensure_ai_conversation(&self) -> Result<String, ServiceError> {
        let id = "ai-default".to_string();
        self.storage.create_conversation(&Conversation {
            id: id.clone(),
            kind: ConversationKind::Ai,
            peer_device_id: None,
            title: "AI 助手".into(),
            draft: String::new(),
            last_read_id: None,
            created_at: now_ms(),
        })?;
        Ok(id)
    }

    /// 在 AI 会话里提问：本机配置了 AI 就直连，否则经对端代理（原版核心思想）。
    pub async fn ai_send(self: &Arc<Self>, text: &str) -> Result<Message, ServiceError> {
        let conv_id = self.ensure_ai_conversation()?;
        let user_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sent,
            created_at: now_ms(),
        };
        self.storage.insert_message(&user_msg)?;
        self.emit(Event::MessageNew(user_msg));

        // 组装上下文（最近 20 条）
        let history = self.storage.list_messages(&conv_id, None, 20)?;
        let msgs: Vec<ChatMessage> = history
            .iter()
            .map(|m| ChatMessage {
                role: if m.sender == Sender::Ai {
                    "assistant".into()
                } else {
                    "user".into()
                },
                content: m.body.clone(),
            })
            .collect();

        let cfg = self.ai_cfg.lock().await.clone();
        let full = if let Some(cfg) = cfg.filter(|c| c.is_ready()) {
            let client = AiClient::new(cfg);
            let node = self.clone();
            let cid = conv_id.clone();
            client
                .chat_stream(&msgs, |delta| {
                    node.emit(Event::AiChunk {
                        conversation_id: cid.clone(),
                        delta: delta.to_string(),
                    });
                })
                .await?
        } else if self.is_connected().await {
            self.ai_via_peer(&msgs).await?
        } else {
            return Err(bluelink_ai::AiError::NotConfigured.into());
        };

        let ai_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
        };
        self.storage.insert_message(&ai_msg)?;
        self.emit(Event::AiDone {
            conversation_id: conv_id,
            message: ai_msg.clone(),
        });
        Ok(ai_msg)
    }

    /// 经对端代理提问：响应帧由 handle_ai_proxy_frame 转入等待通道。
    async fn ai_via_peer(&self, msgs: &[ChatMessage]) -> Result<String, ServiceError> {
        let session = self.session().await?;
        let request_id = new_id();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, String>>(64);
        self.proxy_waiters
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let payload: Vec<serde_json::Value> = msgs
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();
        session
            .conn
            .send(&Frame::Control(Control::AiProxyRequest {
                request_id: request_id.clone(),
                messages: payload,
                model: None,
            }))
            .await?;
        let mut full = String::new();
        while let Some(item) = rx.recv().await {
            match item {
                Ok(delta) => {
                    if delta.is_empty() {
                        break;
                    } // 空增量 = 结束信号
                    full.push_str(&delta);
                }
                Err(e) => {
                    self.proxy_waiters.lock().await.remove(&request_id);
                    return Err(ServiceError::Other(e));
                }
            }
        }
        self.proxy_waiters.lock().await.remove(&request_id);
        if full.is_empty() {
            return Err(ServiceError::Other("对方未能提供 AI 代理".into()));
        }
        Ok(full)
    }

    async fn handle_ai_proxy_frame(&self, c: Control) {
        match c {
            Control::AiProxyChunk { request_id, delta } => {
                if let Some(tx) = self.proxy_waiters.lock().await.get(&request_id) {
                    let _ = tx.send(Ok(delta)).await;
                }
            }
            Control::AiProxyDone { request_id, error } => {
                if let Some(tx) = self.proxy_waiters.lock().await.get(&request_id) {
                    let _ = tx.send(error.map(Err).unwrap_or(Ok(String::new()))).await;
                }
            }
            _ => {}
        }
    }

    /// 宿主侧：替无网对端调用本机 AI 并流式回传。
    async fn serve_ai_proxy(
        &self,
        session: Arc<PeerSession>,
        request_id: String,
        messages: Vec<serde_json::Value>,
    ) -> Result<(), ServiceError> {
        let cfg = self.ai_cfg.lock().await.clone();
        let Some(cfg) = cfg.filter(|c| c.is_ready()) else {
            session
                .conn
                .send(&Frame::Control(Control::AiProxyDone {
                    request_id,
                    error: Some("对方未配置 AI".into()),
                }))
                .await?;
            return Ok(());
        };
        let msgs: Vec<ChatMessage> = messages
            .iter()
            .map(|v| ChatMessage {
                role: v
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("user")
                    .to_string(),
                content: v
                    .get("content")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
            .collect();
        let client = AiClient::new(cfg);
        // 增量先入通道、由转发任务顺序发出，保证分片先于 Done 到达（不能逐段 spawn，会乱序）
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let conn_fwd = session.conn.clone();
        let rid_fwd = request_id.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(d) = rx.recv().await {
                let _ = conn_fwd
                    .send(&Frame::Control(Control::AiProxyChunk {
                        request_id: rid_fwd.clone(),
                        delta: d,
                    }))
                    .await;
            }
        });
        let result = client
            .chat_stream(&msgs, |delta| {
                let _ = tx.try_send(delta.to_string());
            })
            .await;
        drop(tx);
        let _ = forwarder.await;
        let error = result.err().map(|e| e.to_string());
        session
            .conn
            .send(&Frame::Control(Control::AiProxyDone { request_id, error }))
            .await?;
        Ok(())
    }
}
