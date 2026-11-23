//! 演示/联调用节点 CLI：无图形界面地跑起一个完整节点。
//! 用法：
//!   bluelink-node host    --dir <数据目录> --name <名字> --addr 127.0.0.1:7800
//!   bluelink-node connect --dir <数据目录> --name <名字> --addr 127.0.0.1:7800 --code 123456
//! 连接后从标准输入读命令：send <文本> | file <路径> | group <图1,图2,…> | ai <问题> | code | bye（礼貌断开） | quit（拔线退出）
//! 可选环境变量：BLUELINK_AI_BASE / BLUELINK_AI_KEY / BLUELINK_AI_MODEL 配置本机 AI。

use bluelink_services::{Event, Node};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

fn arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == key).map(|w| w[1].clone())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let dir = PathBuf::from(arg(&args, "--dir").unwrap_or_else(|| "node-data".into()));
    let name = arg(&args, "--name").unwrap_or_else(|| "节点".into());
    let addr = arg(&args, "--addr").unwrap_or_else(|| "127.0.0.1:7800".into());

    let node = Node::new(&dir, &name)?;
    if let (Ok(base), Ok(key), Ok(model)) = (
        std::env::var("BLUELINK_AI_BASE"),
        std::env::var("BLUELINK_AI_KEY"),
        std::env::var("BLUELINK_AI_MODEL"),
    ) {
        node.set_ai_config(Some(bluelink_ai::AiConfig {
            base_url: base,
            api_key: key,
            model,
            ..Default::default()
        }))
        .await;
        println!("[本机 AI 已配置]");
    }
    println!("[{name}] 设备 ID：{}", node.device_id);
    println!(
        "[{name}] 连接码：{}",
        bluelink_domain::format_code(&node.my_code())
    );

    // 事件打印
    let mut rx = node.subscribe();
    let name2 = name.clone();
    tokio::spawn(async move {
        while let Ok(e) = rx.recv().await {
            match e {
                Event::MessageNew(m) => {
                    println!("[{name2}] 收到消息（{:?}）：{}", m.sender, m.body)
                }
                Event::TransferProgress {
                    received, total, ..
                } => println!("[{name2}] 传输进度：{received}/{total}"),
                Event::TransferState { state, .. } => println!("[{name2}] 传输状态：{state}"),
                Event::AiChunk { delta, .. } => print!("{delta}"),
                Event::AiDone { .. } => println!(),
                Event::ConnectionChanged { connected, .. } => {
                    println!("[{name2}] 连接状态：{connected}")
                }
                _ => {}
            }
            let _ = std::io::stdout().flush();
        }
    });

    match mode {
        "host" => {
            let listener = bluelink_transport::listen_tcp(&addr).await?;
            println!("[{name}] 正在监听 {addr}，等待连接…");
            let conn = bluelink_transport::accept_tcp(&listener).await?;
            let peer = node.attach(conn, true, "").await?;
            println!("[{name}] 已连接：{}", peer.name);
        }
        "connect" => {
            let code = arg(&args, "--code").ok_or("缺少 --code")?;
            let conn = bluelink_transport::connect_tcp(&addr).await?;
            let peer = node.attach(conn, false, &code).await?;
            println!("[{name}] 已连接：{}", peer.name);
        }
        _ => {
            eprintln!("用法：bluelink-node host|connect --dir D --name N --addr A [--code C]");
            std::process::exit(2);
        }
    }

    // 命令循环（在阻塞线程读 stdin，避免堵住异步运行时）
    let node2: Arc<Node> = node.clone();
    let (tx, mut rx_cmd) = tokio::sync::mpsc::channel::<String>(16);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    while let Some(line) = rx_cmd.recv().await {
        let line = line.trim().to_string();
        if line == "quit" {
            break;
        }
        if line == "bye" {
            // 礼貌断开（v2.6.6 联调）：走 Node::disconnect 发告别帧再退出，
            // 与 quit 的拔线式退出区分——专验对方收到 Goodbye 不重连。
            node2.disconnect().await;
            println!("[已礼貌断开（告别帧已发）]");
            break;
        }
        if line == "code" {
            println!("连接码：{}", bluelink_domain::format_code(&node2.my_code()));
        } else if let Some(text) = line.strip_prefix("send ") {
            match node2.send_text(text, None).await {
                Ok(_) => println!("[已发送]"),
                Err(e) => println!("[发送失败] {e}"),
            }
        } else if let Some(path) = line.strip_prefix("file ") {
            match node2.send_file(std::path::Path::new(path.trim())).await {
                Ok(id) => println!("[文件发送完成] {id}"),
                Err(e) => println!("[文件发送失败] {e}"),
            }
        } else if let Some(list) = line.strip_prefix("group ") {
            // 联调用：把逗号分隔的多个图片文件按「图片组」发出（v2.6.0）
            let paths: Vec<std::path::PathBuf> = list
                .split(',')
                .map(|s| std::path::PathBuf::from(s.trim()))
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
            match node2.send_image_group(&paths).await {
                Ok(id) => println!("[图片组发送完成] {id}"),
                Err(e) => println!("[图片组发送失败] {e}"),
            }
        } else if let Some(q) = line.strip_prefix("ai ") {
            match node2.ai_send(q, None, None).await {
                Ok(m) => println!("[AI 回复] {}", m.body),
                Err(e) => println!("[AI 失败] {e}"),
            }
        }
    }
    Ok(())
}
