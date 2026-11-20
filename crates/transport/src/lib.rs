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

use bluelink_protocol::{read_frame, write_frame, Frame, ProtocolError};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
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

/// 一条已建立的连接：内部对写端加锁，允许多个任务并发发送。
pub struct Conn {
    reader: Mutex<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    /// 对端地址描述（TCP 地址或 "loopback"）。
    pub peer_addr: String,
}

impl Conn {
    pub fn new<S>(stream: S, peer_addr: String) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self {
            reader: Mutex::new(Box::new(r)),
            writer: Arc::new(Mutex::new(Box::new(w))),
            peer_addr,
        }
    }

    /// 发送一帧。
    pub async fn send(&self, frame: &Frame) -> Result<(), TransportError> {
        let mut w = self.writer.lock().await;
        write_frame(&mut **w, frame).await?;
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
    pub async fn recv(&self) -> Result<Frame, TransportError> {
        let mut r = self.reader.lock().await;
        match read_frame(&mut **r).await? {
            Some(f) => Ok(f),
            None => Err(TransportError::Closed),
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
