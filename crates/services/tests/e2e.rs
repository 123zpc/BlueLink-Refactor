//! 端到端测试：两个真实节点经环回连接互通，覆盖聊天、文件传输、断点续传、AI 代理。

use bluelink_ai::AiConfig;
use bluelink_domain::*;
use bluelink_services::{derive_transfer_id, sha256_file, Node};
use bluelink_transport::loopback_pair;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn 临时目录(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bluelink-e2e-{tag}-{}", new_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn 连接两节点(a: &Arc<Node>, b: &Arc<Node>) {
    let (ca, cb) = loopback_pair();
    let code = a.my_code();
    let (ra, rb) = tokio::join!(a.attach(ca, true, ""), b.attach(cb, false, &code),);
    ra.unwrap();
    rb.unwrap();
}

async fn 等消息(node: &Arc<Node>, 关键词: &str) -> Message {
    for _ in 0..100 {
        for c in node.storage.list_conversations().unwrap() {
            for m in node.storage.list_messages(&c.id, None, 50).unwrap() {
                if m.body.contains(关键词) {
                    return m;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等待消息超时：{关键词}");
}

#[tokio::test]
async fn 聊天互通与送达回执() {
    let a = Node::new(&临时目录("a"), "甲").unwrap();
    let b = Node::new(&临时目录("b"), "乙").unwrap();
    连接两节点(&a, &b).await;

    let sent = a.send_text("你好，乙", None).await.unwrap();
    let got = 等消息(&b, "你好，乙").await;
    assert_eq!(got.sender, Sender::Peer);

    // 甲侧应收到送达回执
    let mut delivered = false;
    for _ in 0..40 {
        if let Some(m) = a.storage.get_message(&sent.id).unwrap() {
            if m.status == MessageStatus::Delivered {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(delivered, "未收到送达回执");

    b.send_text("你好，甲", None).await.unwrap();
    等消息(&a, "你好，甲").await;
}

fn 写测试文件(dir: &Path, name: &str, size: usize) -> PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    // 确定性伪随机内容（便于校验）
    let mut x: u64 = 0x12345678;
    let mut buf = vec![0u8; size];
    for b in buf.iter_mut() {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (x >> 33) as u8;
    }
    f.write_all(&buf).unwrap();
    p
}

/// #13（v2.6.5）：接收文件按接收日期落在 downloads/<YYYY-MM-DD>/ 下，
/// 测试按同一口径找落地文件（根目录优先，其次日期子目录）。
fn 找落地文件(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let root = dir.join(name);
    if root.exists() {
        return root;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path().join(name);
            if p.exists() {
                return p;
            }
        }
    }
    root
}

#[tokio::test]
async fn 文件传输完整且哈希一致() {
    let a = Node::new(&临时目录("fa"), "甲").unwrap();
    let b = Node::new(&临时目录("fb"), "乙").unwrap();
    连接两节点(&a, &b).await;

    let src_dir = 临时目录("src");
    let file = 写测试文件(&src_dir, "数据.bin", 3 * 1024 * 1024 + 123);
    let sha = sha256_file(&file).unwrap();
    a.send_file(&file).await.unwrap();

    let landed = 找落地文件(&b.download_dir(), "数据.bin");
    assert!(landed.exists(), "文件未落地");
    assert_eq!(sha256_file(&landed).unwrap(), sha);
}

#[tokio::test]
async fn 断点续传只补缺失部分() {
    let a = Node::new(&临时目录("ra"), "甲").unwrap();
    let bdir = 临时目录("rb");
    let b = Node::new(&bdir, "乙").unwrap();
    连接两节点(&a, &b).await;

    let src_dir = 临时目录("rsrc");
    let file = 写测试文件(&src_dir, "大文件.dat", 2 * 1024 * 1024);
    let sha = sha256_file(&file).unwrap();
    let tid = derive_transfer_id(&sha, &b.device_id);

    // 在乙侧预置：已收前 1MiB（.part 前段 + 传输记录）
    let part = b.download_dir().join(format!("{tid}.part"));
    {
        let data = std::fs::read(&file).unwrap();
        let mut f = std::fs::File::create(&part).unwrap();
        f.write_all(&data[..1024 * 1024]).unwrap();
    }
    b.storage
        .save_transfer(
            &tid,
            "",
            "in",
            &a.device_id,
            2 * 1024 * 1024,
            &vec![(0, 1024 * 1024)],
            "receiving",
        )
        .unwrap();

    // 订阅进度：首个进度事件的已收字节应 >= 1MiB（证明走了续传而非重传）
    let mut rx = a.subscribe();
    a.send_file(&file).await.unwrap();
    let mut saw_resume = false;
    while let Ok(e) = rx.try_recv() {
        if let bluelink_services::Event::TransferProgress { received, .. } = e {
            if received >= 1024 * 1024 {
                saw_resume = true;
            }
        }
    }
    assert!(saw_resume, "未观察到续传进度");
    let landed = 找落地文件(&b.download_dir(), "大文件.dat");
    assert_eq!(sha256_file(&landed).unwrap(), sha);
}

/// 极简 SSE 模拟服务器：回一段固定流式文本。
async fn 模拟ai服务器() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"你好世界\"}}]}\n\ndata: [DONE]\n\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(), body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

#[tokio::test]
async fn ai代理无网方经有网方提问() {
    let base = 模拟ai服务器().await;
    let a = Node::new(&临时目录("pa"), "无网方").unwrap();
    let b = Node::new(&临时目录("pb"), "有网方").unwrap();
    b.set_ai_config(Some(AiConfig {
        base_url: base,
        api_key: "sk-test".into(),
        model: "mock".into(),
        ..Default::default()
    }))
    .await;
    连接两节点(&a, &b).await;

    // 未开启转发许可时必须被明确拒绝（默认关，花的是 B 的额度）
    let denied = a.ai_send("报个到", None, None).await;
    assert!(denied.is_err(), "未开许可不应转发成功");
    // 开启许可后转发通畅，且统计有记录
    b.storage.set_setting("ai_proxy_allow", "1").unwrap();
    let reply = a.ai_send("报个到", None, None).await.unwrap();
    assert_eq!(reply.body, "你好世界");
    assert_eq!(b.proxy_today_stats().0, 1);
    assert_eq!(reply.sender, Sender::Ai);
}

#[tokio::test]
async fn 剪贴板同步开关默认关且状态双向通告() {
    let a = Node::new(&临时目录("ca"), "甲").unwrap();
    let b = Node::new(&临时目录("cb"), "乙").unwrap();
    // 默认关（同意模型：没开就绝不动对方剪贴板）
    assert!(!a.clipboard_local_on());
    assert!(!b.clipboard_local_on());

    连接两节点(&a, &b).await;
    // 连接建立后互通状态：双方都没开 → 对方状态均为关
    assert!(!a.clipboard_peer_on());
    assert!(!b.clipboard_peer_on());

    // 甲开启 → 乙应收到通告；乙未开 → 甲看到的对方状态仍是关
    a.set_clipboard_sync(true).await.unwrap();
    for _ in 0..40 {
        if b.clipboard_peer_on() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(b.clipboard_peer_on(), "乙未收到甲的开启通告");
    assert!(!a.clipboard_peer_on());

    // 乙开启 → 甲收到通告，双方都开（同步生效条件达成）
    b.set_clipboard_sync(true).await.unwrap();
    for _ in 0..40 {
        if a.clipboard_peer_on() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(a.clipboard_peer_on(), "甲未收到乙的开启通告");

    // 甲关闭 → 乙看到的对方状态变关
    a.set_clipboard_sync(false).await.unwrap();
    for _ in 0..40 {
        if !b.clipboard_peer_on() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!b.clipboard_peer_on(), "乙未收到甲的关闭通告");
}

/// ⑱ 复制图片的失败口径（成功路径要真剪贴板，留给 GUI 自测在 Xvfb 下
/// 点按钮 + 读回验证）：文件不存在 → 「已不存在」；内容不是图片 →
/// 「解码失败」。两条都在碰剪贴板之前返回，无显示环境也能测。
#[tokio::test]
async fn 复制图片失败口径() {
    let a = Node::new(&临时目录("ci"), "甲").unwrap();
    let err = a
        .copy_image_blocking(Path::new("/nonexistent/没有这张图.png"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("已不存在"), "实际：{err}");
    let bad = 临时目录("ci-bad").join("假图片.png");
    std::fs::write(&bad, b"this is not an image").unwrap();
    let err = a.copy_image_blocking(&bad).unwrap_err().to_string();
    assert!(err.contains("解码失败"), "实际：{err}");
}
