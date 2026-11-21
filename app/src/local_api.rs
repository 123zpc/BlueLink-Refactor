//! 本地 API 服务：在 127.0.0.1:8321 开放 OpenAI 兼容接口，供本机其他软件调用。
//! 请求经 Node 网关出去（本机直连上游，或经已连接设备转发到对方配置的 AI），
//! 上游密钥只存在真正调用上游的一侧，永远不过蓝牙、也不经过本接口回传。
//! 只绑回环地址、不对外监听；设置里可另加本机访问密钥（Authorization: Bearer <密钥>）。

use bluelink_ai::RichMessage;
use bluelink_services::Node;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 本地接口端口（原型定版：http://127.0.0.1:8321/v1）。
pub const PORT: u16 = 8321;

/// 服务是否真正在监听（绑定成功后置位、任务结束清除）。
/// Tauri 的 JoinHandle 没有 is_finished，运行状态靠这个标志如实反映。
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_running() -> bool {
    RUNNING.load(std::sync::atomic::Ordering::Relaxed)
}

/// 上层 abort 任务时调用：被取消的任务来不及自己清标志，这里补上。
pub fn mark_stopped() {
    RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// 启动服务（返回任务句柄，上层持有以便按设置开关启停）。
/// 必须走 Tauri 的异步运行时：setup 阶段主线程上没有 Tokio 上下文，
/// 直接 tokio::spawn 会 panic（开启本地 API 后重启应用必崩，已实测）。
pub fn spawn(node: Arc<Node>) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)).await {
            Ok(l) => l,
            Err(e) => {
                bluelink_services::log_line(
                    "api",
                    &format!("本地 API 服务启动失败（端口 {PORT} 可能被占用）：{e}"),
                );
                return;
            }
        };
        RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
        while let Ok((sock, _)) = listener.accept().await {
            let n = node.clone();
            tokio::spawn(async move {
                let _ = handle(n, sock).await;
            });
        }
        RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "200 OK",
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        404 => "404 Not Found",
        413 => "413 Payload Too Large",
        502 => "502 Bad Gateway",
        _ => "500 Internal Server Error",
    }
}

async fn respond_json(
    sock: &mut TcpStream,
    code: u16,
    v: &serde_json::Value,
) -> std::io::Result<()> {
    let body = v.to_string();
    let head = format!(
        "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        status_text(code),
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.flush().await
}

/// OpenAI 风格错误体：调用方软件能直接把 message 展示给用户。
async fn respond_error(sock: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    respond_json(
        sock,
        code,
        &serde_json::json!({"error": {"message": msg, "type": "bluelink_error"}}),
    )
    .await
}

/// 写一个 chunked 分块（SSE 的一条 data 事件）。
async fn write_sse(sock: &mut TcpStream, data: &str) -> std::io::Result<()> {
    let payload = format!("data: {data}\n\n");
    sock.write_all(format!("{:x}\r\n", payload.len()).as_bytes())
        .await?;
    sock.write_all(payload.as_bytes()).await?;
    sock.write_all(b"\r\n").await?;
    sock.flush().await
}

async fn handle(node: Arc<Node>, mut sock: TcpStream) -> std::io::Result<()> {
    // 读请求（头上限 64 KB、体上限 4 MB，本地接口足够）
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut authorization = String::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = v.trim().parse().unwrap_or(0),
            "authorization" => authorization = v.trim().to_string(),
            _ => {}
        }
    }
    if content_length > 4 * 1024 * 1024 {
        return respond_error(&mut sock, 413, "请求体过大").await;
    }
    let mut body: Vec<u8> = buf.split_off(header_end);
    while body.len() < content_length {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    // 本机访问密钥：设置了才校验（留空 = 本机任何软件可调）
    let expect_key = node
        .storage
        .get_setting("local_api_key")
        .ok()
        .flatten()
        .unwrap_or_default();
    if !expect_key.is_empty() && authorization != format!("Bearer {expect_key}") {
        return respond_error(&mut sock, 401, "本地接口访问密钥不正确").await;
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/v1/models") => {
            let model = node.gateway_model().await;
            respond_json(
                &mut sock,
                200,
                &serde_json::json!({
                    "object": "list",
                    "data": [{"id": model, "object": "model", "owned_by": "bluelink"}]
                }),
            )
            .await
        }
        ("POST", "/v1/chat/completions") => chat_completions(node, &mut sock, &body).await,
        _ => respond_error(&mut sock, 404, "接口不存在").await,
    }
}

/// 取一条消息的文本内容：兼容 OpenAI 的两种形态——纯字符串，或分段数组
/// （[{type:"text",text:"…"},…]，TRAE 等编程工具发的是后者）。
/// 数组时拼接全部 text 段，非文本段（图片等）跳过；v2.6.1 前只认纯字符串，
/// 数组内容被解析成空字符串、空消息发到上游被 422 拒（真机实证）。
fn message_content_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                match p {
                    serde_json::Value::String(s) => out.push_str(s),
                    v if v.get("type").and_then(|t| t.as_str()) == Some("text") => {
                        if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                            out.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 从请求体提取可透传的生成参数（temperature / max_tokens / top_p）：
/// 只收数值合法的，其余忽略——调用方软件给什么尽量带给上游（经对端
/// 转发时随 AiProxy 请求一起带过去），不给就按执行侧配置走。
fn gateway_options(v: &serde_json::Value) -> bluelink_services::AiCallOptions {
    bluelink_services::AiCallOptions {
        thinking: None,
        temperature: v
            .get("temperature")
            .and_then(|t| t.as_f64())
            .map(|t| t as f32)
            .filter(|t| (0.0..=2.0).contains(t)),
        max_tokens: v
            .get("max_tokens")
            .and_then(|t| t.as_u64())
            .and_then(|t| u32::try_from(t).ok())
            .filter(|t| *t > 0),
        top_p: v
            .get("top_p")
            .and_then(|t| t.as_f64())
            .map(|t| t as f32)
            .filter(|t| (0.0..=1.0).contains(t)),
    }
}

async fn chat_completions(
    node: Arc<Node>,
    sock: &mut TcpStream,
    body: &[u8],
) -> std::io::Result<()> {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return respond_error(sock, 400, "请求不是有效 JSON").await,
    };
    let stream = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    // 富消息解析（v2.6.2，#13）：content 原样、tool_calls/tool_call_id
    // 完整保留——旧版在这里就压平成纯文本，对端组装时工具调用历史
    // 必然残缺、被上游拒收（TRAE 长历史必 422 的第一现场）。
    let mut msgs: Vec<RichMessage> = Vec::new();
    if let Some(arr) = v.get("messages").and_then(|m| m.as_array()) {
        for m in arr {
            msgs.push(RichMessage {
                role: m
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("user")
                    .to_string(),
                content: m.get("content").cloned(),
                tool_calls: m.get("tool_calls").cloned(),
                tool_call_id: m
                    .get("tool_call_id")
                    .and_then(|t| t.as_str())
                    .map(|t| t.to_string()),
                reasoning_content: m
                    .get("reasoning_content")
                    .and_then(|t| t.as_str())
                    .map(|t| t.to_string()),
            });
        }
    }
    if msgs.is_empty() {
        return respond_error(sock, 400, "messages 为空").await;
    }
    let opts = gateway_options(&v);
    bluelink_services::log_line(
        "api",
        &format!(
            "本地 API 请求：chat/completions（{} 条消息，流式={}）",
            msgs.len(),
            stream
        ),
    );
    let model = node.gateway_model().await;
    let created = secs_now();

    if !stream {
        let prompt_chars: usize = msgs
            .iter()
            .map(|m| message_content_text(m.content.as_ref()).chars().count())
            .sum();
        let mut sink = |_: &str| {};
        return match node.gateway_chat(&msgs, &opts, &mut sink).await {
            Ok(full) => {
                let completion_chars = full.chars().count();
                respond_json(
                    sock,
                    200,
                    &serde_json::json!({
                        "id": "chatcmpl-bluelink",
                        "object": "chat.completion",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": full},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": prompt_chars,
                            "completion_tokens": completion_chars,
                            "total_tokens": prompt_chars + completion_chars
                        }
                    }),
                )
                .await
            }
            Err(e) => respond_error(sock, 502, &e.to_string()).await,
        };
    }

    // 流式：网关增量经无界通道转出（try_send 不丢片），逐条写成 SSE 分块
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
    let n2 = node.clone();
    let msgs2 = msgs.clone();
    tokio::spawn(async move {
        let tx2 = tx.clone();
        let mut cb = move |d: &str| {
            let _ = tx2.send(Ok(d.to_string()));
        };
        if let Err(e) = n2.gateway_chat(&msgs2, &opts, &mut cb).await {
            let _ = tx.send(Err(e.to_string()));
        }
    });
    // 先探第一条：若立刻失败且还没发响应头，改回明确的 502 错误 JSON
    let mut item = rx.recv().await;
    if let Some(Err(e)) = &item {
        let msg = e.clone();
        return respond_error(sock, 502, &msg).await;
    }
    sock.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
    )
    .await?;
    loop {
        match item {
            Some(Ok(delta)) => {
                let chunk = serde_json::json!({
                    "id": "chatcmpl-bluelink",
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {"content": delta}, "finish_reason": null}]
                });
                write_sse(sock, &chunk.to_string()).await?;
            }
            // 中途失败：直接收尾断开，好过让客户端假死等超时
            Some(Err(_)) => break,
            None => break,
        }
        item = rx.recv().await;
    }
    let fin = serde_json::json!({
        "id": "chatcmpl-bluelink",
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    write_sse(sock, &fin.to_string()).await?;
    write_sse(sock, "[DONE]").await?;
    sock.write_all(b"0\r\n\r\n").await?;
    sock.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 内容解析兼容数组分段() {
        // 纯字符串（旧形态）
        let v = serde_json::json!("你好");
        assert_eq!(message_content_text(Some(&v)), "你好");
        // 数组分段（TRAE 等编程工具的形态）：text 段拼接、非文本段跳过
        let v = serde_json::json!([
            {"type": "text", "text": "帮我看看"},
            {"type": "image_url", "image_url": {"url": "data:..."}},
            {"type": "text", "text": "这段代码"}
        ]);
        assert_eq!(message_content_text(Some(&v)), "帮我看看这段代码");
        // 缺失/ null / 其它类型：空字符串（不 panic）
        assert_eq!(message_content_text(None), "");
        assert_eq!(message_content_text(Some(&serde_json::Value::Null)), "");
    }

    #[test]
    fn 网关参数只收合法数值() {
        let v = serde_json::json!({"temperature": 0.5, "max_tokens": 256, "top_p": 0.9});
        let o = gateway_options(&v);
        assert_eq!(o.temperature, Some(0.5));
        assert_eq!(o.max_tokens, Some(256));
        assert_eq!(o.top_p, Some(0.9));
        // 越界/缺失一律忽略，不把非法值带给上游
        let v = serde_json::json!({"temperature": 9.9, "max_tokens": 0, "top_p": "x"});
        let o = gateway_options(&v);
        assert_eq!(o.temperature, None);
        assert_eq!(o.max_tokens, None);
        assert_eq!(o.top_p, None);
    }
}
