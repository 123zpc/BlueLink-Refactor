//! 协议 v3 实现（详细设计第一章）：长度前缀帧编解码、控制消息、文件分片。
//! 帧格式：`魔数 2B "BL" | u32 小端长度 | 1 字节类型 | payload | CRC32 4B`，
//! 长度字段计类型+payload+CRC 本身，CRC32 覆盖类型+payload，payload 上限 16 MiB。
//!
//! v3 帧层加固（v2.6.2，故障隔离硬规）：魔数+帧校验让接收端在字节流
//! 失步（BLE 切片丢失/重复/污染）时只丢损坏帧、扫描魔数重新对齐到下一
//! 帧头——一帧损坏绝不许杀死整条连接。代价是与 v2.6.1 及更早版本的帧
//! 格式不兼容（对方旧帧无魔数、解码器只会丢弃并等不到完整帧，握手以
//! 超时干净失败，与 zstd 换装时的混版口径一致）。

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单帧 payload 上限（16 MiB）。
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
/// 帧头魔数（"BL"）：解码器失步时扫描它重新对齐。
pub const FRAME_MAGIC: [u8; 2] = [0x42, 0x4C];
/// 长度字段上限：类型(1) + payload(MAX) + CRC(4)。
const MAX_LEN_FIELD: usize = MAX_PAYLOAD as usize + 5;

/// CRC32（IEEE 802.3，多项式 0xEDB88320）：手写查表实现，协议层不引新依赖。
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}
static CRC_TABLE: [u32; 256] = crc32_table();

const fn crc32_tables4() -> [[u32; 256]; 4] {
    let base = crc32_table();
    let mut tables = [[0u32; 256]; 4];
    let mut i = 0;
    while i < 256 {
        tables[0][i] = base[i];
        let mut k = 1;
        while k < 4 {
            let prev = tables[k - 1][i];
            tables[k][i] = base[(prev & 0xFF) as usize] ^ (prev >> 8);
            k += 1;
        }
        i += 1;
    }
    tables
}
static CRC_TABLES4: [[u32; 256]; 4] = crc32_tables4();

/// 计算一段字节的 CRC32（IEEE，与 crc32 标准向量一致）。
/// 四字节切片查表：逐字节版在大文件帧上要多花数倍时间，帧层校验
/// 不能成为吞吐瓶颈（v2.6.2 速度回归实测后优化）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    let (chunks, remainder) = data.as_chunks::<4>();
    for ch in chunks {
        let x = c ^ u32::from_le_bytes(*ch);
        c = CRC_TABLES4[3][(x & 0xFF) as usize]
            ^ CRC_TABLES4[2][((x >> 8) & 0xFF) as usize]
            ^ CRC_TABLES4[1][((x >> 16) & 0xFF) as usize]
            ^ CRC_TABLES4[0][((x >> 24) & 0xFF) as usize];
    }
    for &b in remainder {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}
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
        /// 思考模式开关（v2.6.1）：随请求带给对端，对端组装上游请求体时用。
        /// serde default：旧版对端忽略未知字段、新版读旧帧得 None，混版不破。
        #[serde(default)]
        thinking: Option<bool>,
        /// 请求级温度（本地 API 网关透传用），同上混版兼容。
        #[serde(default)]
        temperature: Option<f32>,
        /// 请求级最大回复长度（本地 API 网关透传用）。
        #[serde(default)]
        max_tokens: Option<u32>,
        /// 请求级 top_p（本地 API 网关透传用）。
        #[serde(default)]
        top_p: Option<f32>,
    },
    AiProxyChunk {
        request_id: String,
        delta: String,
    },
    AiProxyDone {
        request_id: String,
        error: Option<String>,
        /// 思考开关是否真被上游接受（v2.6.1）：请求带了 thinking 且对端
        /// 上游支持时为 Some(true)，带了但不支持为 Some(false)，没带为 None。
        /// 请求方据此如实提示用户，不许假装生效。
        #[serde(default)]
        thinking_applied: Option<bool>,
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

/// 把一帧编码为字节（魔数 + 长度 + 类型 + payload + CRC32）。
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
    let mut out = Vec::with_capacity(2 + 4 + 1 + payload.len() + 4);
    out.extend_from_slice(&FRAME_MAGIC);
    out.extend_from_slice(&((payload.len() as u32 + 5).to_le_bytes()));
    out.push(code);
    out.extend_from_slice(&payload);
    let crc = crc32(&out[6..]);
    out.extend_from_slice(&crc.to_le_bytes());
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

/// 按类型码把 payload 解成一帧（帧头/长度/CRC 已由调用方校验过）。
fn decode_payload(code: u8, body: &[u8]) -> Result<Frame, ProtocolError> {
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
        return Ok(Frame::Chunk(chunk));
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
        return Ok(Frame::ClipboardImage(image));
    }
    let control: Control = serde_json::from_slice(body)?;
    Ok(Frame::Control(control))
}

/// 可重对齐的流式帧解码器（v2.6.2 故障隔离核心）：字节先进缓冲，
/// next_frame 逐帧取出。失步（长度离谱/CRC 不符/JSON 坏）时只丢损坏
/// 帧、扫描魔数重新对齐到下一帧头，绝不把错误抛给连接层——连接只有
/// 在真 EOF / IO 错误时才结束。计数器供上层落日志对账。
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// 消费游标：buf[..start] 已消费。旧版逐帧 drain 前段，每帧都要
    /// 搬移剩余缓冲，大帧流下是纯浪费；游标化后消费只是挪下标，
    /// push 时一次性压实（v2.6.2 速度回归优化）。
    start: usize,
    /// 成功解出的帧数。
    pub frames_ok: u64,
    /// 被丢弃的损坏帧数（长度离谱、CRC 不符、payload 解析失败）。
    pub frames_dropped: u64,
    /// 发生重新对齐的次数。
    pub resyncs: u64,
    /// 对齐过程中跳过的字节数。
    pub bytes_skipped: u64,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入新到达的字节（先压实已消费段，保持缓冲有界）。
    pub fn push(&mut self, data: &[u8]) {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// 当前缓冲里还没消费的字节数。
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    fn view(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn find_magic(&self) -> Option<usize> {
        self.view().windows(2).position(|w| w == FRAME_MAGIC)
    }

    /// 消费 n 字节。
    fn consume(&mut self, n: usize) {
        self.start += n;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
    }

    /// 尝试从缓冲中解出一帧。见类型注释：坏帧只被丢弃，绝不冒泡。
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            match self.find_magic() {
                None => {
                    // 找不到魔数：最多保留末尾 1 字节（可能与下一片拼成魔数）
                    let keep = if self.view().last() == Some(&FRAME_MAGIC[0]) {
                        1
                    } else {
                        0
                    };
                    let drop = self.buffered() - keep;
                    if drop > 0 {
                        self.bytes_skipped += drop as u64;
                        self.consume(drop);
                    }
                    return None;
                }
                Some(0) => {}
                Some(p) => {
                    self.bytes_skipped += p as u64;
                    self.resyncs += 1;
                    self.consume(p);
                }
            }
            if self.buffered() < 6 {
                return None; // 魔数+长度还没到齐
            }
            let len = u32::from_le_bytes(self.view()[2..6].try_into().unwrap()) as usize;
            if !(5..=MAX_LEN_FIELD).contains(&len) {
                // 长度离谱：这不是帧头，丢 1 字节继续扫描
                self.frames_dropped += 1;
                self.resyncs += 1;
                self.bytes_skipped += 1;
                self.consume(1);
                continue;
            }
            if self.buffered() < 6 + len {
                return None; // 整帧还没到齐
            }
            // 就地校验与解析（不整帧拷贝）：typed = 类型+payload+CRC
            let (ok, parsed) = {
                let typed = &self.view()[6..6 + len];
                let (tp, crc_bytes) = typed.split_at(typed.len() - 4);
                let crc_stored = u32::from_le_bytes(crc_bytes.try_into().unwrap());
                if crc32(tp) != crc_stored {
                    (false, None)
                } else {
                    (true, decode_payload(tp[0], &tp[1..]).ok())
                }
            };
            if !ok {
                // CRC 不符：只退 1 字节重扫——真帧头若藏在坏帧字节里，
                // 后续扫描自然会找到，不必整帧回插。每轮净丢 ≥1 字节。
                self.frames_dropped += 1;
                self.resyncs += 1;
                self.bytes_skipped += 1;
                self.consume(1);
                continue;
            }
            match parsed {
                Some(f) => {
                    self.frames_ok += 1;
                    self.consume(6 + len);
                    return Some(f);
                }
                None => {
                    // CRC 对但 payload 解析失败（如坏 JSON）：丢这一帧
                    self.frames_dropped += 1;
                    self.consume(6 + len);
                    continue;
                }
            }
        }
    }
}

/// 从异步流读取一帧；流正常关闭时返回 None。
/// 内部走 FrameDecoder：坏帧被丢弃并自动重对齐，不冒泡为错误。
pub async fn read_frame(
    r: &mut (dyn AsyncRead + Unpin + Send),
) -> Result<Option<Frame>, ProtocolError> {
    let mut dec = FrameDecoder::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if let Some(f) = dec.next_frame() {
            return Ok(Some(f));
        }
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        dec.push(&chunk[..n]);
    }
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
        // 多帧同流往返走有状态解码器（Conn 的生产路径；read_frame 无状态、
        // 一次读会吞掉后续帧的字节，不适合多帧流）
        let mut dec = FrameDecoder::new();
        dec.push(&buf);
        for f in &frames {
            assert_eq!(&dec.next_frame().unwrap(), f);
        }
        assert!(dec.next_frame().is_none());
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
        let mut dec = FrameDecoder::new();
        dec.push(&buf);
        for f in &frames {
            assert_eq!(&dec.next_frame().unwrap(), f);
        }
        assert!(dec.next_frame().is_none());
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
        let bytes = encode(&plain).unwrap();
        assert!(
            !bytes.windows(5).any(|w| w == b"group"),
            "无组时不应出现 group 字段"
        );
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

    fn ping() -> Frame {
        Frame::Control(Control::Ping)
    }

    fn chat(text: &str) -> Frame {
        Frame::Control(Control::ChatText {
            msg_id: "m".into(),
            text: text.into(),
            reply_to: None,
            ts: 1,
        })
    }

    fn drain(dec: &mut FrameDecoder) -> Vec<Frame> {
        let mut out = Vec::new();
        while let Some(f) = dec.next_frame() {
            out.push(f);
        }
        out
    }

    /// CRC32 与标准向量对齐（"123456789" 的 CRC32-IEEE 是 0xCBF43926）。
    #[test]
    fn crc32标准向量() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// 失步注入①：整帧丢失——后续帧必须照常解出，解码器不报废。
    #[test]
    fn 解码器丢帧后自恢复() {
        let mut stream = Vec::new();
        stream.extend(encode(&ping()).unwrap());
        stream.extend(encode(&chat("一")).unwrap());
        stream.extend(encode(&chat("二")).unwrap());
        // 模拟第一帧在空中整帧丢失：从第二帧头开始喂
        let first_len = encode(&ping()).unwrap().len();
        let mut dec = FrameDecoder::new();
        dec.push(&stream[first_len..]);
        let got = drain(&mut dec);
        assert_eq!(got, vec![chat("一"), chat("二")]);
        assert_eq!(dec.frames_ok, 2);
    }

    /// 失步注入②：帧中间丢字节——本帧丢弃，下一帧重新对齐解出。
    #[test]
    fn 解码器帧内丢字节后重对齐() {
        let mut stream = Vec::new();
        stream.extend(encode(&chat("受损帧")).unwrap());
        stream.extend(encode(&chat("完好帧")).unwrap());
        stream.drain(10..16); // 在第一帧 payload 中段抠掉 6 字节
        let mut dec = FrameDecoder::new();
        // 分两次喂（模拟切片边界任意切分）
        let half = stream.len() / 2;
        dec.push(&stream[..half]);
        let mut got = drain(&mut dec);
        dec.push(&stream[half..]);
        got.extend(drain(&mut dec));
        assert_eq!(got, vec![chat("完好帧")]);
        assert!(dec.frames_dropped >= 1, "损坏帧应被计数丢弃");
        assert!(dec.resyncs >= 1, "应发生重新对齐");
    }

    /// 失步注入③：重复字节/整帧重发——重复帧至多多解一次（帧层不去重，
    /// 去重归业务层），但绝不失步，后续帧照常。
    #[test]
    fn 解码器重复注入不失步() {
        let one = encode(&chat("重复")).unwrap();
        let two = encode(&ping()).unwrap();
        let mut stream = Vec::new();
        stream.extend(&one);
        stream.extend(&one); // 整帧重复（切片重传的最坏形态）
        stream.extend(&two);
        let mut dec = FrameDecoder::new();
        dec.push(&stream);
        let got = drain(&mut dec);
        assert_eq!(got.len(), 3);
        assert_eq!(got[2], ping());
    }

    /// 失步注入④：帧内单字节被污染——CRC 拦截本帧，后续帧无恙。
    #[test]
    fn 解码器坏字节被crc拦截() {
        let mut stream = Vec::new();
        stream.extend(encode(&chat("被污染")).unwrap());
        stream.extend(encode(&chat("干净")).unwrap());
        stream[12] ^= 0xFF; // 翻转第一帧 payload 内一字节
        let mut dec = FrameDecoder::new();
        dec.push(&stream);
        let got = drain(&mut dec);
        assert_eq!(got, vec![chat("干净")]);
        assert!(dec.frames_dropped >= 1);
    }

    /// 失步注入⑤：随机垃圾字节垫在帧前——扫描魔数跳过垃圾。
    #[test]
    fn 解码器跳过前置垃圾() {
        let mut stream = vec![0x00, 0x99, 0x42, 0x00, 0xFF, 0x13];
        stream.extend(encode(&ping()).unwrap());
        let mut dec = FrameDecoder::new();
        dec.push(&stream);
        assert_eq!(drain(&mut dec), vec![ping()]);
        assert!(dec.bytes_skipped >= 5);
    }

    /// 任意切分喂入（逐字节）结果一致：切片边界不得影响解码。
    #[test]
    fn 解码器逐字节喂入一致() {
        let mut stream = Vec::new();
        stream.extend(encode(&chat("逐字节")).unwrap());
        stream.extend(encode(&ping()).unwrap());
        let mut dec = FrameDecoder::new();
        let mut got = Vec::new();
        for b in &stream {
            dec.push(&[*b]);
            got.extend(drain(&mut dec));
        }
        assert_eq!(got, vec![chat("逐字节"), ping()]);
    }
}
