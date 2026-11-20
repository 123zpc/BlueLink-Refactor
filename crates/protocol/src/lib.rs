//! 协议 v2 实现（详细设计第一章）：长度前缀帧编解码、控制消息、文件分片。
//! 帧格式：`u32 小端 payload 长度 | 1 字节类型 | payload`，payload 上限 16 MiB。

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单帧 payload 上限（16 MiB）。
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
/// 文件分片大小（256 KiB）。
pub const CHUNK_SIZE: usize = 256 * 1024;

/// 分片/图片帧的压缩算法标志（帧内 1 字节）：
/// 0=无压缩，1=旧版 zlib（v2.5.2 起不再发送，收到即判旧版并干净失败），
/// 2=zstd（v2.5.2 起）。其余取值一律按未知算法处理。
pub const COMP_NONE: u8 = 0;
pub const COMP_ZLIB_LEGACY: u8 = 1;
pub const COMP_ZSTD: u8 = 2;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("帧长度超限：{0}")]
    FrameTooLarge(u32),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 解析失败：{0}")]
    Json(#[from] serde_json::Error),
    #[error("分片帧格式错误")]
    BadChunk,
}

/// 控制类消息（JSON 承载），与详细设计 1.2 的帧类型表一一对应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    Hello {
        device_id: String,
        device_name: String,
        protocol: u32,
        code: String,
    },
    HelloAck {
        device_id: String,
        device_name: String,
        accepted: bool,
        reason: Option<String>,
    },
    ChatText {
        msg_id: String,
        text: String,
        reply_to: Option<String>,
        ts: i64,
    },
    Delivered {
        msg_id: String,
    },
    Typing {
        typing: bool,
    },
    FileOffer {
        transfer_id: String,
        msg_id: String,
        name: String,
        size: u64,
        sha256: String,
        mime: Option<String>,
        /// 图片组归属（v2.6.0）：Some 时表示本文件是某图片组的一员。
        /// 老版本（≤v2.5.2）反序列化时按 serde 默认规则忽略这个未知
        /// 字段，照常把每张图当独立文件消息接收——即天然逐张降级；
        /// 新版本据此把成员收进同一条组消息而不是逐张建消息。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<GroupRef>,
    },
    FileAccept {
        transfer_id: String,
        received: Vec<(u64, u64)>,
    },
    FileDone {
        transfer_id: String,
    },
    FileVerify {
        transfer_id: String,
        ok: bool,
    },
    FileCancel {
        transfer_id: String,
    },
    AiProxyRequest {
        request_id: String,
        messages: Vec<serde_json::Value>,
        model: Option<String>,
    },
    AiProxyChunk {
        request_id: String,
        delta: String,
    },
    AiProxyDone {
        request_id: String,
        error: Option<String>,
    },
    /// 剪贴板同步开关状态通告：连接建立后双方互发，开关被切换时再发。
    /// 只有双方都开启才真正同步内容。
    ClipboardState {
        enabled: bool,
    },
    /// 剪贴板文字内容（hash 为内容 SHA-256 十六进制，接收方据此抑制回环）。
    ClipboardText {
        hash: String,
        text: String,
    },
    Ping,
    Pong,
}

impl Control {
    /// 控制消息对应的帧类型码。
    pub fn type_code(&self) -> u8 {
        match self {
            Control::Hello { .. } => 0x01,
            Control::HelloAck { .. } => 0x02,
            Control::ChatText { .. } => 0x03,
            Control::Delivered { .. } => 0x04,
            Control::Typing { .. } => 0x05,
            Control::FileOffer { .. } => 0x10,
            Control::FileAccept { .. } => 0x11,
            Control::FileDone { .. } => 0x13,
            Control::FileVerify { .. } => 0x14,
            Control::FileCancel { .. } => 0x15,
            Control::AiProxyRequest { .. } => 0x20,
            Control::AiProxyChunk { .. } => 0x21,
            Control::AiProxyDone { .. } => 0x22,
            Control::ClipboardState { .. } => 0x06,
            Control::ClipboardText { .. } => 0x07,
            Control::Ping => 0x30,
            Control::Pong => 0x31,
        }
    }
}

/// 图片组归属（随 FileOffer 承载）：组消息 ID + 本成员在组内的
/// 顺序下标（0 起）+ 组内总数。接收方据此把成员按序收进组清单。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupRef {
    pub group_id: String,
    pub index: u32,
    pub total: u32,
}

/// 文件分片帧（类型码 0x12，二进制承载）。
#[derive(Debug, Clone, PartialEq)]
pub struct FileChunk {
    pub transfer_id: String,
    pub offset: u64,
    /// 压缩算法标志（COMP_* 常量）。
    pub compression: u8,
    pub data: Vec<u8>,
}

/// 剪贴板图片帧（类型码 0x1A，二进制承载）：RGBA 位图，可选压缩
/// （压缩/解压由服务层负责，协议层只搬字节，与文件分片同口径）。
/// hash 为未压缩 RGBA 内容（含宽高）的 SHA-256，接收方据此抑制回环。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipboardImage {
    pub hash: [u8; 32],
    pub width: u32,
    pub height: u32,
    /// 压缩算法标志（COMP_* 常量）。
    pub compression: u8,
    pub data: Vec<u8>,
}

/// 一帧的解码结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Control(Control),
    Chunk(FileChunk),
    ClipboardImage(ClipboardImage),
}

/// 把一帧编码为字节（长度前缀 + 类型 + payload）。
pub fn encode(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    let (code, payload) = match frame {
        Frame::Control(c) => (c.type_code(), serde_json::to_vec(c)?),
        Frame::Chunk(ch) => {
            let mut p = Vec::with_capacity(16 + 8 + 1 + ch.data.len());
            // transfer_id 以 16 字节 UUID 形式承载；非 UUID 时退化为哈希前 16 字节
            p.extend_from_slice(&id_to_16(&ch.transfer_id));
            p.extend_from_slice(&ch.offset.to_le_bytes());
            p.push(ch.compression);
            p.extend_from_slice(&ch.data);
            (0x12u8, p)
        }
        Frame::ClipboardImage(img) => {
            let mut p = Vec::with_capacity(32 + 4 + 4 + 1 + img.data.len());
            p.extend_from_slice(&img.hash);
            p.extend_from_slice(&img.width.to_le_bytes());
            p.extend_from_slice(&img.height.to_le_bytes());
            p.push(img.compression);
            p.extend_from_slice(&img.data);
            (0x1Au8, p)
        }
    };
    if payload.len() as u64 + 1 > MAX_PAYLOAD as u64 {
        return Err(ProtocolError::FrameTooLarge(payload.len() as u32));
    }
    let mut out = Vec::with_capacity(4 + 1 + payload.len());
    out.extend_from_slice(&((payload.len() as u32 + 1).to_le_bytes()));
    out.push(code);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn id_to_16(id: &str) -> [u8; 16] {
    if let Ok(u) = uuid_parse(id) {
        return u;
    }
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(id.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&d[..16]);
    out
}

fn uuid_parse(s: &str) -> Result<[u8; 16], ()> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(());
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(out)
}

fn id_from_16(b: &[u8]) -> String {
    // 与 id_to_16 对应：按 UUID 文本格式还原（本项目 transfer_id 均为 UUID）
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// 从异步流读取一帧；流正常关闭时返回 None。
pub async fn read_frame(
    r: &mut (dyn AsyncRead + Unpin + Send),
) -> Result<Option<Frame>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf);
    if len == 0 || len > MAX_PAYLOAD {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    let code = payload[0];
    let body = &payload[1..];
    if code == 0x12 {
        if body.len() < 25 {
            return Err(ProtocolError::BadChunk);
        }
        let chunk = FileChunk {
            transfer_id: id_from_16(&body[..16]),
            offset: u64::from_le_bytes(
                body[16..24]
                    .try_into()
                    .map_err(|_| ProtocolError::BadChunk)?,
            ),
            compression: body[24],
            data: body[25..].to_vec(),
        };
        return Ok(Some(Frame::Chunk(chunk)));
    }
    if code == 0x1A {
        if body.len() < 41 {
            return Err(ProtocolError::BadChunk);
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&body[..32]);
        let image = ClipboardImage {
            hash,
            width: u32::from_le_bytes(
                body[32..36]
                    .try_into()
                    .map_err(|_| ProtocolError::BadChunk)?,
            ),
            height: u32::from_le_bytes(
                body[36..40]
                    .try_into()
                    .map_err(|_| ProtocolError::BadChunk)?,
            ),
            compression: body[40],
            data: body[41..].to_vec(),
        };
        return Ok(Some(Frame::ClipboardImage(image)));
    }
    let control: Control = serde_json::from_slice(body)?;
    Ok(Some(Frame::Control(control)))
}

/// 向异步流写出一帧。
pub async fn write_frame(
    w: &mut (dyn AsyncWrite + Unpin + Send),
    frame: &Frame,
) -> Result<(), ProtocolError> {
    let bytes = encode(frame)?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 控制帧往返一致() {
        let frames = vec![
            Frame::Control(Control::Hello {
                device_id: "d1".into(),
                device_name: "甲".into(),
                protocol: 2,
                code: "123456".into(),
            }),
            Frame::Control(Control::ChatText {
                msg_id: "m1".into(),
                text: "你好".into(),
                reply_to: None,
                ts: 42,
            }),
            Frame::Control(Control::FileAccept {
                transfer_id: "t".into(),
                received: vec![(0, 100), (300, 400)],
            }),
            Frame::Control(Control::Ping),
        ];
        let mut buf: Vec<u8> = Vec::new();
        for f in &frames {
            buf.extend_from_slice(&encode(f).unwrap());
        }
        let mut cursor = std::io::Cursor::new(buf);
        for f in &frames {
            let got = read_frame(&mut cursor).await.unwrap().unwrap();
            assert_eq!(&got, f);
        }
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 剪贴板帧往返一致() {
        let frames = vec![
            Frame::Control(Control::ClipboardState { enabled: true }),
            Frame::Control(Control::ClipboardText {
                hash: "ab12".into(),
                text: "同步的文字".into(),
            }),
            Frame::ClipboardImage(ClipboardImage {
                hash: [9u8; 32],
                width: 3,
                height: 2,
                compression: COMP_ZSTD,
                data: vec![1, 2, 3, 4, 5],
            }),
        ];
        let mut buf: Vec<u8> = Vec::new();
        for f in &frames {
            buf.extend_from_slice(&encode(f).unwrap());
        }
        let mut cursor = std::io::Cursor::new(buf);
        for f in &frames {
            let got = read_frame(&mut cursor).await.unwrap().unwrap();
            assert_eq!(&got, f);
        }
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 分片帧往返一致() {
        let id = bluelink_domain::new_id();
        let f = Frame::Chunk(FileChunk {
            transfer_id: id.clone(),
            offset: 262144,
            compression: COMP_ZSTD,
            data: vec![7u8; 1000],
        });
        let bytes = encode(&f).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(got, f);
    }

    #[tokio::test]
    async fn 图片组要约往返一致() {
        let f = Frame::Control(Control::FileOffer {
            transfer_id: "t1".into(),
            msg_id: "member-1".into(),
            name: "a.jpg".into(),
            size: 123,
            sha256: "ab".into(),
            mime: None,
            group: Some(GroupRef {
                group_id: "g1".into(),
                index: 2,
                total: 5,
            }),
        });
        let bytes = encode(&f).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(got, f);
        // 无 group 时不写出该字段（与老版本帧逐字节同构）
        let plain = Frame::Control(Control::FileOffer {
            transfer_id: "t1".into(),
            msg_id: "m".into(),
            name: "a.jpg".into(),
            size: 1,
            sha256: "ab".into(),
            mime: None,
            group: None,
        });
        let s = String::from_utf8(encode(&plain).unwrap()).unwrap();
        assert!(!s.contains("group"), "无组时不应出现 group 字段：{s}");
    }

    /// 降级锚点：用「老版本形态」的枚举（FileOffer 没有 group 字段，
    /// 其余与 v2.5.2 相同）解析新版本带 group 的帧 JSON——serde 默认
    /// 忽略未知字段，解析必须成功且字段不丢。老版本真机行为以
    /// v2.5.2 源码 worktree 双端实测为准（见 QA），此测只锁线格式。
    #[test]
    fn 老形态枚举能解析带组字段的要约() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(tag = "t", rename_all = "snake_case")]
        enum OldControl {
            FileOffer {
                transfer_id: String,
                msg_id: String,
                name: String,
                size: u64,
                sha256: String,
                mime: Option<String>,
            },
        }
        let new = Control::FileOffer {
            transfer_id: "t1".into(),
            msg_id: "member-1".into(),
            name: "a.jpg".into(),
            size: 123,
            sha256: "ab".into(),
            mime: None,
            group: Some(GroupRef {
                group_id: "g1".into(),
                index: 0,
                total: 4,
            }),
        };
        let json = serde_json::to_vec(&new).unwrap();
        let old: OldControl = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            old,
            OldControl::FileOffer {
                transfer_id: "t1".into(),
                msg_id: "member-1".into(),
                name: "a.jpg".into(),
                size: 123,
                sha256: "ab".into(),
                mime: None,
            }
        );
    }
}
