//! 协议 v2 实现（详细设计第一章）：长度前缀帧编解码、控制消息、文件分片。
//! 帧格式：`u32 小端 payload 长度 | 1 字节类型 | payload`，payload 上限 16 MiB。

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单帧 payload 上限（16 MiB）。
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
/// 文件分片大小（256 KiB）。
pub const CHUNK_SIZE: usize = 256 * 1024;

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
            Control::Ping => 0x30,
            Control::Pong => 0x31,
        }
    }
}

/// 文件分片帧（类型码 0x12，二进制承载）。
#[derive(Debug, Clone, PartialEq)]
pub struct FileChunk {
    pub transfer_id: String,
    pub offset: u64,
    pub compressed: bool,
    pub data: Vec<u8>,
}

/// 一帧的解码结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Control(Control),
    Chunk(FileChunk),
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
            p.push(if ch.compressed { 1 } else { 0 });
            p.extend_from_slice(&ch.data);
            (0x12u8, p)
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
            compressed: body[24] & 1 == 1,
            data: body[25..].to_vec(),
        };
        return Ok(Some(Frame::Chunk(chunk)));
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
    async fn 分片帧往返一致() {
        let id = bluelink_domain::new_id();
        let f = Frame::Chunk(FileChunk {
            transfer_id: id.clone(),
            offset: 262144,
            compressed: true,
            data: vec![7u8; 1000],
        });
        let bytes = encode(&f).unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let got = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(got, f);
    }
}
