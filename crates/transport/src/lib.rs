//! 传输层：把协议帧跑在某种字节流上。
//! 产品通道：经典蓝牙 RFCOMM（见 classic 模块，Windows；老版本验证过的机制）与
//! BLE（见 ble 模块，广播/扫描/GATT 字节流）。两者产出同一个 Conn，服务层不区分。
//! SimpleBLE（见 simpleble 模块）是第三个 BLE 实现，与 ble 模块同语义，
//! 仅开发者模式可选，且需构建时开启 simpleble feature 才真正编译进来。
//! TCP 与进程内环回仅供开发测试（node-cli 与自动化测试），不进入产品界面。

pub mod ble;
pub mod classic;
mod probe;
pub mod simpleble;

pub use probe::{set_log_sink, set_step_sink, take_probe_lines};

use bluelink_protocol::{Frame, FrameDecoder, ProtocolError};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("协议错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("连接已关闭")]
    Closed,
    #[error("蓝牙错误：{0}")]
    Ble(String),
}

/// 接收侧状态：帧解码器 + 读缓冲（解码器跨调用保留未消费字节）。
struct RecvState {
    decoder: FrameDecoder,
    scratch: Vec<u8>,
    /// 上次落日志时的丢帧计数（只在计数变化时落行，防刷屏）。
    logged_dropped: u64,
}

/// 当前时刻（Unix 毫秒），与服务层 now_ms 同一口径。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一条已建立的连接：内部对写端加锁，允许多个任务并发发送。
pub struct Conn {
    reader: Mutex<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    recv_state: Mutex<RecvState>,
    /// 对端地址描述（TCP 地址或 "loopback"）。
    pub peer_addr: String,
    /// 已接受发送的字节总数（send 编码后全长）。
    written_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// 已真正交给底层（无线电/套接字）的字节数：通道类传输由泵逐片
    /// 累加（见 ble::StreamEnds::aired_bytes，与本字段是同一个计数器）；
    /// 直连流（TCP/环回）在 flush 完成后即视为已交付、与 written 同步。
    aired_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// aired 计数是否由外部泵驱动（通道类传输在构造时注入共享计数器）：
    /// true 时 send 只记 written、aired 等泵逐片累加；false（直连流）
    /// 时 flush 完成即视为已交付，aired 与 written 同步推进。
    external_air: bool,
    /// 最近一次读到底层字节的时刻（毫秒）：长帧传输期间完整帧迟迟不
    /// 成帧，但字节一直在流——心跳判死必须看这个而不是只看成帧时刻，
    /// 否则一张大图传一半就会被自家心跳误杀（v2.6.5，#18 定案）。
    last_inbound_byte_ms: std::sync::atomic::AtomicU64,
}

impl Conn {
    pub fn new<S>(stream: S, peer_addr: String) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::build(stream, peer_addr, None)
    }

    /// 通道类传输（BLE/SimpleBLE/经典）用：aired 计数器与泵共享，
    /// 泵每送出一片就累加，outbound_lag 才反映真实空中滞留。
    pub fn new_with_air<S>(
        stream: S,
        peer_addr: String,
        aired_bytes: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::build(stream, peer_addr, Some(aired_bytes))
    }

    fn build<S>(
        stream: S,
        peer_addr: String,
        aired: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self {
            reader: Mutex::new(Box::new(r)),
            writer: Arc::new(Mutex::new(Box::new(w))),
            recv_state: Mutex::new(RecvState {
                decoder: FrameDecoder::new(),
                scratch: vec![0u8; 64 * 1024],
                logged_dropped: 0,
            }),
            peer_addr,
            written_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            external_air: aired.is_some(),
            aired_bytes: aired.unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            last_inbound_byte_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 出站滞留字节：已接受发送但还没真正交到底层的部分。通道类传输
    /// 上 >0 表示队列/泵里还有货在空中排队；直连流恒为 0（flush 即交付）。
    pub fn outbound_lag(&self) -> u64 {
        let w = self
            .written_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let a = self.aired_bytes.load(std::sync::atomic::Ordering::Relaxed);
        w.saturating_sub(a)
    }

    /// 最近一次读到底层字节的时刻（Unix 毫秒，0 = 还没读到过）。
    pub fn last_inbound_byte_ms(&self) -> u64 {
        self.last_inbound_byte_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 发送一帧。
    pub async fn send(&self, frame: &Frame) -> Result<(), TransportError> {
        let bytes = bluelink_protocol::encode(frame)?;
        let n = bytes.len() as u64;
        {
            let mut w = self.writer.lock().await;
            w.write_all(&bytes).await?;
            w.flush().await?;
            // written 在锁内累加（第三方评审采纳）：与字节进通道原子
            // 成对，排空判定读到的 written 永远不落后于已入通道的字节。
            self.written_bytes
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        }
        if !self.external_air {
            // 直连流：flush 完成即已交付，aired 与 written 同步推进
            self.aired_bytes.store(
                self.written_bytes
                    .load(std::sync::atomic::Ordering::Relaxed),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        Ok(())
    }

    /// 主动关闭连接：关掉写端——对端会读到 EOF 从而结束会话，
    /// 各平台的出站泵也随之拆链（BLE 断开外设、经典关 socket）。
    /// 本端接收任务在对端关闭或进程退出时结束。
    pub async fn close(&self) {
        let mut w = self.writer.lock().await;
        let _ = w.shutdown().await;
    }

    /// 接收一帧；对端正常关闭返回 Closed 错误。
    ///
    /// 故障隔离（v2.6.2 硬规）：帧层损坏（失步/CRC 不符/坏 JSON）由
    /// FrameDecoder 在内部丢弃并重新对齐，**不冒泡为错误**——单帧损坏
    /// 绝不许杀死连接。这里只会在真 EOF（Closed）或底层 IO 出错时
    /// 返回 Err，那才是连接合法的结束原因。
    pub async fn recv(&self) -> Result<Frame, TransportError> {
        let mut st = self.recv_state.lock().await;
        let RecvState {
            decoder,
            scratch,
            logged_dropped,
        } = &mut *st;
        loop {
            if let Some(f) = decoder.next_frame() {
                return Ok(f);
            }
            if decoder.frames_dropped != *logged_dropped {
                *logged_dropped = decoder.frames_dropped;
                crate::probe::log(
                    "proto",
                    &format!(
                        "帧层丢弃损坏帧并重新对齐（累计丢帧 {}、重对齐 {} 次、跳过 {} 字节），连接保持",
                        decoder.frames_dropped, decoder.resyncs, decoder.bytes_skipped
                    ),
                );
            }
            let n = {
                let mut r = self.reader.lock().await;
                r.read(scratch).await?
            };
            if n == 0 {
                return Err(TransportError::Closed);
            }
            // 字节级入站活动（#18）：成帧之前先记一笔，心跳判死看它
            self.last_inbound_byte_ms
                .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
            decoder.push(&scratch[..n]);
        }
    }
}

/// TCP 监听：accept 后逐个返回连接。
pub async fn listen_tcp(addr: &str) -> Result<TcpListener, TransportError> {
    Ok(TcpListener::bind(addr).await?)
}

/// 发起 TCP 连接。
pub async fn connect_tcp(addr: &str) -> Result<Conn, TransportError> {
    let stream = TcpStream::connect(addr).await?;
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.to_string());
    Ok(Conn::new(stream, peer))
}

/// 接受一个 TCP 连接。
pub async fn accept_tcp(listener: &TcpListener) -> Result<Conn, TransportError> {
    let (stream, addr) = listener.accept().await?;
    Ok(Conn::new(stream, addr.to_string()))
}

/// 进程内环回连接对（测试与演示用）：两端互发帧。
pub fn loopback_pair() -> (Conn, Conn) {
    let (a, b) = tokio::io::duplex(1024 * 1024);
    (
        Conn::new(a, "loopback-a".into()),
        Conn::new(b, "loopback-b".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bluelink_protocol::Control;

    #[tokio::test]
    async fn 环回互发帧() {
        let (a, b) = loopback_pair();
        a.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(b.recv().await.unwrap(), Frame::Control(Control::Ping));
        b.send(&Frame::Control(Control::ChatText {
            msg_id: "m".into(),
            text: "嗨".into(),
            reply_to: None,
            ts: 1,
        }))
        .await
        .unwrap();
        match a.recv().await.unwrap() {
            Frame::Control(Control::ChatText { text, .. }) => assert_eq!(text, "嗨"),
            _ => panic!("帧类型不对"),
        }
    }

    #[tokio::test]
    async fn 关闭后对端读到结束() {
        let (a, b) = loopback_pair();
        a.close().await;
        // a 的写端已关：b 再收应得 Closed；a 仍可收 b 发来的帧（半关闭）
        b.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(a.recv().await.unwrap(), Frame::Control(Control::Ping));
        assert!(matches!(b.recv().await, Err(TransportError::Closed)));
    }

    /// 故障隔离回归（v2.6.2 硬规）：在正常帧流中注入垃圾字节与一帧
    /// 被篡改的合法帧，随后消息必须在**同一条连接**上继续成功往来，
    /// 连接全程不断、双向都通。代理在 A→B 方向第一批字节之后注入。
    #[tokio::test]
    async fn 坏帧注入后同连接继续往来() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (a_io, x1) = tokio::io::duplex(64 * 1024);
        let (b_io, x2) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let (mut r1, mut w1) = tokio::io::split(x1);
            let (mut r2, mut w2) = tokio::io::split(x2);
            let p1 = async move {
                let mut buf = vec![0u8; 8192];
                let mut injected = false;
                loop {
                    let n = r1.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    if w2.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    if !injected {
                        injected = true;
                        // 垃圾：半截假帧头 + 无魔数噪声
                        let _ = w2
                            .write_all(&[0x42, 0x4C, 0xFF, 0xFF, 0xFF, 0xFF, 9, 9, 9, 0, 1, 2])
                            .await;
                        // 一帧 CRC 被篡改的合法帧（必须被帧层丢弃）
                        let mut bad =
                            bluelink_protocol::encode(&Frame::Control(Control::Ping)).unwrap();
                        let last = bad.len() - 1;
                        bad[last] ^= 0xFF;
                        let _ = w2.write_all(&bad).await;
                    }
                }
            };
            let p2 = async move {
                let _ = tokio::io::copy(&mut r2, &mut w1).await;
            };
            tokio::join!(p1, p2);
        });
        let a = Conn::new(a_io, "fault-a".into());
        let b = Conn::new(b_io, "fault-b".into());
        // 第一帧正常到达（注入发生在这之后）
        a.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(b.recv().await.unwrap(), Frame::Control(Control::Ping));
        // 注入物已在路上：后续帧必须穿过垃圾与坏帧、原样到达
        for i in 0..3 {
            a.send(&Frame::Control(Control::ChatText {
                msg_id: format!("m{i}"),
                text: format!("注入后仍达{i}"),
                reply_to: None,
                ts: i,
            }))
            .await
            .unwrap();
            match b.recv().await.unwrap() {
                Frame::Control(Control::ChatText { text, .. }) => {
                    assert_eq!(text, format!("注入后仍达{i}"))
                }
                other => panic!("第 {i} 条收到错误帧：{other:?}"),
            }
        }
        // 反方向同一条连接照常往来
        b.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(a.recv().await.unwrap(), Frame::Control(Control::Ping));
    }

    #[tokio::test]
    async fn tcp连接互发帧() {
        let listener = listen_tcp("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { accept_tcp(&listener).await.unwrap() });
        let client = connect_tcp(&format!("127.0.0.1:{port}")).await.unwrap();
        let server = server.await.unwrap();
        client.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(server.recv().await.unwrap(), Frame::Control(Control::Ping));
    }
}
