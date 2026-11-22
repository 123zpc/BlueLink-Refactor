//! AI 客户端：OpenAI 兼容接口（/chat/completions），支持流式输出。
//! 错误按需求规范分成五类，各有明确中文文案（见 AiError 的展示文本）。

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 对话消息（OpenAI 格式）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// 富消息（v2.6.2，#13）：保留 OpenAI 消息的完整形态——content 原样
/// （字符串/分段数组/null）、tool_calls、tool_call_id、reasoning_content。
/// 本地 API 网关与 AI 转发必须走这个类型：旧链路把消息压平为
/// (role, content 字符串)，tool_calls 被丢弃后，assistant 的
/// content=null 与失去前置的 tool 消息被上游直接拒（真 DeepSeek
/// 实测 400），长历史里只要有一对工具调用就全盘失败。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RichMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl RichMessage {
    /// 纯文本消息的便捷构造（内部调用点用）。
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(serde_json::Value::String(content.to_string())),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }
}

/// 取消息内容的纯文本：字符串直取；分段数组拼接 text 段（非文本段跳过）；
/// null/缺失为空串。与本地 API 网关旧解析口径一致。
pub fn rich_content_text(content: Option<&serde_json::Value>) -> String {
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

/// 把富消息规范化为可直接发给上游的 JSON 消息数组（#13 核心修复）。
/// 规则全部来自真 DeepSeek 接口的实测验收（2026-11-21 二分复现）：
/// - 纯长历史（200 条、56 万 token）本身不被拒，被拒的只有消息形态；
/// - assistant 的 content=null 且无 tool_calls/reasoning_content → 400，
///   规范化为 content:""（空串实测可过）；
/// - role=tool 的消息若找不到带匹配 tool_calls 的前置 assistant → 400，
///   降级为 user 消息（内容不变），保证整段历史可达；
/// - 分段数组 content 压平为文本（文本模型不收图片段）；
/// - 规范的 tool 配对（assistant 带 tool_calls + content=null）原样保留。
pub fn normalize_messages(msgs: &[RichMessage]) -> Vec<serde_json::Value> {
    // 先算每条 tool 消息是否有合法前置：向前跳过连续 tool 消息，
    // 找到最近的 assistant，检查其 tool_calls 是否含本条的 id。
    let tool_ok: Vec<bool> = msgs
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if m.role != "tool" {
                return false;
            }
            let Some(want) = m.tool_call_id.as_deref() else {
                return false;
            };
            let mut j = i;
            while j > 0 {
                j -= 1;
                if msgs[j].role == "tool" {
                    continue;
                }
                if msgs[j].role != "assistant" {
                    return false;
                }
                return msgs[j]
                    .tool_calls
                    .as_ref()
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(want))
                    })
                    .unwrap_or(false);
            }
            false
        })
        .collect();
    msgs.iter()
        .enumerate()
        .map(|(i, m)| {
            let text = rich_content_text(m.content.as_ref());
            let has_calls = m
                .tool_calls
                .as_ref()
                .map(|t| t.as_array().map(|a| !a.is_empty()).unwrap_or(false))
                .unwrap_or(false);
            match m.role.as_str() {
                "assistant" => {
                    let mut v = serde_json::json!({"role": "assistant"});
                    if has_calls {
                        // 规范形态：content 可为 null，原样保留
                        v["content"] = m.content.clone().unwrap_or(serde_json::Value::Null);
                        v["tool_calls"] = m.tool_calls.clone().unwrap();
                    } else if m.content.is_none() && m.reasoning_content.is_none() {
                        // 上游硬拒形态（实测 400）：补空串
                        v["content"] = serde_json::json!("");
                    } else {
                        v["content"] = serde_json::json!(text);
                    }
                    if let Some(r) = &m.reasoning_content {
                        v["reasoning_content"] = serde_json::json!(r);
                    }
                    v
                }
                "tool" if tool_ok[i] => serde_json::json!({
                    "role": "tool",
                    "content": text,
                    "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                }),
                "tool" => {
                    // 失去前置 tool_calls 的工具结果：降级为 user，内容不丢
                    serde_json::json!({"role": "user", "content": text})
                }
                "system" => serde_json::json!({"role": "system", "content": text}),
                _ => serde_json::json!({"role": "user", "content": text}),
            }
        })
        .collect()
}

/// AI 错误分类（与 REQUIREMENTS 的五类错误一一对应）。
#[derive(Debug, Error)]
pub enum AiError {
    #[error("尚未配置 AI：请先在设置里填写接口地址、密钥和模型")]
    NotConfigured,
    #[error("网络连接失败：请检查网络后重试")]
    Network,
    #[error("接口拒绝了请求：密钥无效或无权限，请检查设置")]
    Auth,
    #[error("请求过于频繁或额度不足：请稍后再试")]
    RateLimited,
    #[error("接口返回异常（状态码 {0}）：请稍后重试")]
    Server(u16),
    /// 上游错误原文（v2.6.2，#13）：非鉴权/限流类失败时把上游返回的
    /// 错误正文（截断脱敏后）带出来——旧版只留状态码，上游到底拒什么
    /// 无从查起，TRAE 长历史 422 就是这样被吞掉的。
    #[error("接口返回异常（状态码 {0}）：{1}")]
    Upstream(u16, String),
    #[error("接口返回内容无法解析")]
    BadResponse,
}

/// AI 配置（v2.2 起补全字段：流式开关、温度、最大回复长度、系统提示词）。
/// 新字段都带默认值，旧的三字段构造与旧数据不受影响。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 是否流式输出（默认开；关时整段返回）。
    #[serde(default = "default_true")]
    pub stream: bool,
    /// 温度（0–2），None 表示不传给接口、用接口默认。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 最大回复长度（tokens），None 表示不限。
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 系统提示词（为空则不附加）。由实际调用上游的一侧自动加在消息列表最前。
    #[serde(default)]
    pub system_prompt: String,
}

fn default_true() -> bool {
    true
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            stream: true,
            temperature: None,
            max_tokens: None,
            system_prompt: String::new(),
        }
    }
}

/// 单次请求级选项（v2.6.1）：覆盖配置级参数，随请求走——本机直连时
/// 直接进请求体，经对端转发时随 AiProxy 请求带到对端、对端组装请求体时用。
/// 全部 None = 完全按配置走（旧行为）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AiRequestOptions {
    /// 思考模式开关：Some(true)=开、Some(false)=关、None=不指定。
    /// 仅 DeepSeek 兼容接口认这个参数（见 thinking_supported），其余接口忽略。
    pub thinking: Option<bool>,
    /// 温度：Some 时覆盖配置里的温度。
    pub temperature: Option<f32>,
    /// 最大回复长度：Some 时覆盖配置里的设置。
    pub max_tokens: Option<u32>,
    /// top_p：配置里没有此项，只有请求级才有（本地 API 网关透传用）。
    pub top_p: Option<f32>,
}

/// 上游是否支持思考模式开关：目前只有 DeepSeek 兼容接口认 thinking 参数
/// （{"type":"enabled"/"disabled"}）。按接口地址判断——地址里带 deepseek
/// 字样才算，宁可漏报不可假装支持（开关没生效必须如实提示用户）。
pub fn thinking_supported(base_url: &str) -> bool {
    base_url.to_ascii_lowercase().contains("deepseek")
}

impl AiConfig {
    pub fn is_ready(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
            && !self.model.trim().is_empty()
    }

    /// 组装请求体：带上已设置的温度/最大长度；系统提示词自动置顶（调用方已带 system 时不重复加）。
    /// 无选项版仅供单测与旧调用点，新代码走 build_body_opts。
    #[allow(dead_code)]
    fn build_body(&self, messages: &[ChatMessage], stream: bool) -> serde_json::Value {
        self.build_body_opts(messages, stream, &AiRequestOptions::default())
            .0
    }

    /// 带请求级选项组装请求体。thinking 是否真被带上由返回值告知调用方
    /// （不支持的上游不带、且要如实提示，不许假装开了）。
    fn build_body_opts(
        &self,
        messages: &[ChatMessage],
        stream: bool,
        opts: &AiRequestOptions,
    ) -> (serde_json::Value, bool) {
        let mut msgs: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);
        let prompt = self.system_prompt.trim();
        if !prompt.is_empty() && messages.first().map(|m| m.role.as_str()) != Some("system") {
            msgs.push(serde_json::json!({"role": "system", "content": prompt}));
        }
        msgs.extend(
            messages
                .iter()
                .map(|m| serde_json::json!({"role": m.role, "content": m.content})),
        );
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": msgs,
            "stream": stream,
        });
        // 请求级参数优先于配置级（网关/界面的单次指定不改用户保存的配置）
        if let Some(t) = opts.temperature.or(self.temperature) {
            body["temperature"] = t.into();
        }
        if let Some(m) = opts.max_tokens.or(self.max_tokens) {
            body["max_tokens"] = m.into();
        }
        if let Some(p) = opts.top_p {
            body["top_p"] = p.into();
        }
        // 思考模式：仅 DeepSeek 兼容接口带 thinking 参数，其余接口不带并
        // 经第二个返回值如实告知未生效。
        let mut thinking_applied = false;
        if let Some(on) = opts.thinking {
            if thinking_supported(&self.base_url) {
                body["thinking"] = serde_json::json!({
                    "type": if on { "enabled" } else { "disabled" }
                });
                thinking_applied = true;
            }
        }
        (body, thinking_applied)
    }

    /// 富消息版请求体（v2.6.2）：消息先经 normalize_messages 规范化，
    /// 其余参数口径与 build_body_opts 完全一致。
    fn build_body_rich(
        &self,
        messages: &[RichMessage],
        stream: bool,
        opts: &AiRequestOptions,
    ) -> (serde_json::Value, bool) {
        let mut msgs: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);
        let prompt = self.system_prompt.trim();
        if !prompt.is_empty() && messages.first().map(|m| m.role.as_str()) != Some("system") {
            msgs.push(serde_json::json!({"role": "system", "content": prompt}));
        }
        msgs.extend(normalize_messages(messages));
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": msgs,
            "stream": stream,
        });
        if let Some(t) = opts.temperature.or(self.temperature) {
            body["temperature"] = t.into();
        }
        if let Some(m) = opts.max_tokens.or(self.max_tokens) {
            body["max_tokens"] = m.into();
        }
        if let Some(p) = opts.top_p {
            body["top_p"] = p.into();
        }
        let mut thinking_applied = false;
        if let Some(on) = opts.thinking {
            if thinking_supported(&self.base_url) {
                body["thinking"] = serde_json::json!({
                    "type": if on { "enabled" } else { "disabled" }
                });
                thinking_applied = true;
            }
        }
        (body, thinking_applied)
    }
}

/// 流式字节的增量 UTF-8 解码器（v2.6.5，#4）：网络块边界任意切分，
/// 一个多字节汉字可能被劈成两半分落前后两块。旧实现对每块单独
/// from_utf8_lossy——劈开的汉字两半各自变成替换符（U+FFFD），回复里
/// 出现「一□□话」式乱码、时有时无全看切口。此解码器保留未完的尾
/// 字节到下一块，只在完整字符边界解码；真正非法的字节（上游不是
/// UTF-8 时）才按 lossy 语义替换单个字节，绝不连累后续字符。
#[derive(Default)]
pub struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一块字节，返回本块能完整解出的文本（未完尾字节留待下块）。
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // 安全：valid_up_to 之前的部分是合法 UTF-8
                        out.push_str(unsafe {
                            std::str::from_utf8_unchecked(&self.pending[..valid])
                        });
                        self.pending.drain(..valid);
                    }
                    match e.error_len() {
                        // 尾部是不完整的多字节序列：留着等下一块拼齐
                        None => break,
                        // 中间真有非法字节：替换这一个字节后继续，不许整段变替换符
                        Some(n) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..n.min(self.pending.len()));
                        }
                    }
                }
            }
        }
        out
    }

    /// 流结束时收尾：残留的未完字节按 lossy 语义给出（正常流不该有残留）。
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let s = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        s
    }
}

/// 从一行 SSE 数据中提取增量文本（纯函数，便于测试）。
/// 返回 None 表示该行不带增量（如 [DONE] 或心跳）。
pub fn parse_sse_delta(line: &str) -> Option<String> {
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" || data.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    v.pointer("/choices/0/delta/content")?
        .as_str()
        .map(|s| s.to_string())
}

/// AI 客户端。
pub struct AiClient {
    cfg: AiConfig,
    http: reqwest::Client,
}

impl AiClient {
    pub fn new(cfg: AiConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::new(),
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    fn check_ready(&self) -> Result<(), AiError> {
        if self.cfg.is_ready() {
            Ok(())
        } else {
            Err(AiError::NotConfigured)
        }
    }

    /// 非成功状态 → 错误：401/403 归鉴权、429 归限流，其余读上游错误
    /// 正文（截断 400 字、折叠空白）包进 Upstream——原文只进错误与
    /// 日志，绝不回显密钥（请求头不在正文里，天然不含）。
    async fn status_error(resp: reqwest::Response) -> AiError {
        let code = resp.status().as_u16();
        match code {
            401 | 403 => AiError::Auth,
            429 => AiError::RateLimited,
            _ => {
                let body = resp.text().await.unwrap_or_default();
                let brief: String = body
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(400)
                    .collect();
                if brief.is_empty() {
                    AiError::Server(code)
                } else {
                    AiError::Upstream(code, brief)
                }
            }
        }
    }

    /// 流式对话：每收到一段增量就回调 on_delta，结束返回完整文本。
    pub async fn chat_stream<F: FnMut(&str)>(
        &self,
        messages: &[ChatMessage],
        on_delta: F,
    ) -> Result<String, AiError> {
        Ok(self
            .chat_stream_opts(messages, &AiRequestOptions::default(), on_delta)
            .await?
            .0)
    }

    /// 带请求级选项的流式对话：返回（完整文本，思考开关是否真被上游接受）。
    pub async fn chat_stream_opts<F: FnMut(&str)>(
        &self,
        messages: &[ChatMessage],
        opts: &AiRequestOptions,
        mut on_delta: F,
    ) -> Result<(String, bool), AiError> {
        self.check_ready()?;
        let (body, thinking_applied) = self.cfg.build_body_opts(messages, true, opts);
        let resp = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| AiError::Network)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        let mut full = String::new();
        let mut buffer = String::new();
        let mut decoder = Utf8StreamDecoder::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|_| AiError::Network)?;
            buffer.push_str(&decoder.push(&bytes));
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                if let Some(delta) = parse_sse_delta(line.trim()) {
                    full.push_str(&delta);
                    on_delta(&delta);
                }
            }
        }
        buffer.push_str(&decoder.finish());
        if let Some(delta) = parse_sse_delta(buffer.trim()) {
            full.push_str(&delta);
            on_delta(&delta);
        }
        if full.is_empty() {
            return Err(AiError::BadResponse);
        }
        Ok((full, thinking_applied))
    }

    /// 非流式对话（代理模式与连通性测试用）。
    pub async fn chat_once(&self, messages: &[ChatMessage]) -> Result<String, AiError> {
        Ok(self
            .chat_once_opts(messages, &AiRequestOptions::default())
            .await?
            .0)
    }

    /// 带请求级选项的非流式对话：返回（完整文本，思考开关是否真被上游接受）。
    pub async fn chat_once_opts(
        &self,
        messages: &[ChatMessage],
        opts: &AiRequestOptions,
    ) -> Result<(String, bool), AiError> {
        self.check_ready()?;
        let (body, thinking_applied) = self.cfg.build_body_opts(messages, false, opts);
        let resp = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| AiError::Network)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|_| AiError::BadResponse)?;
        let text = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or(AiError::BadResponse)?;
        Ok((text, thinking_applied))
    }

    /// 富消息流式对话（v2.6.2）：网关/转发专用，消息形态完整保留。
    pub async fn chat_rich_stream_opts<F: FnMut(&str)>(
        &self,
        messages: &[RichMessage],
        opts: &AiRequestOptions,
        mut on_delta: F,
    ) -> Result<(String, bool), AiError> {
        self.check_ready()?;
        let (body, thinking_applied) = self.cfg.build_body_rich(messages, true, opts);
        let resp = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| AiError::Network)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        let mut full = String::new();
        let mut buffer = String::new();
        let mut decoder = Utf8StreamDecoder::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|_| AiError::Network)?;
            buffer.push_str(&decoder.push(&bytes));
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                if let Some(delta) = parse_sse_delta(line.trim()) {
                    full.push_str(&delta);
                    on_delta(&delta);
                }
            }
        }
        buffer.push_str(&decoder.finish());
        if let Some(delta) = parse_sse_delta(buffer.trim()) {
            full.push_str(&delta);
            on_delta(&delta);
        }
        if full.is_empty() {
            return Err(AiError::BadResponse);
        }
        Ok((full, thinking_applied))
    }

    /// 富消息非流式对话（v2.6.2）。
    pub async fn chat_rich_once_opts(
        &self,
        messages: &[RichMessage],
        opts: &AiRequestOptions,
    ) -> Result<(String, bool), AiError> {
        self.check_ready()?;
        let (body, thinking_applied) = self.cfg.build_body_rich(messages, false, opts);
        let resp = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| AiError::Network)?;
        if !resp.status().is_success() {
            return Err(Self::status_error(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|_| AiError::BadResponse)?;
        let text = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or(AiError::BadResponse)?;
        Ok((text, thinking_applied))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse增量解析() {
        let line = r#"data: {"choices":[{"delta":{"content":"你好"}}]}"#;
        assert_eq!(parse_sse_delta(line).as_deref(), Some("你好"));
        assert_eq!(parse_sse_delta("data: [DONE]"), None);
        assert_eq!(parse_sse_delta(": heartbeat"), None);
        assert_eq!(parse_sse_delta(r#"data: {"choices":[{"delta":{}}]}"#), None);
    }

    #[test]
    fn 请求体带可选参数与系统提示词() {
        let cfg = AiConfig {
            base_url: "http://x/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            temperature: Some(0.7),
            max_tokens: Some(512),
            system_prompt: "你是助手".into(),
            ..Default::default()
        };
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        let body = cfg.build_body(&msgs, true);
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 0.001);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        // 未设置的可选参数不出现在请求体里
        let plain = AiConfig::default().build_body(&msgs, false);
        assert!(plain.get("temperature").is_none());
        assert!(plain.get("max_tokens").is_none());
        assert_eq!(plain["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn 思考开关只给deepseek接口带参数() {
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        let opts = AiRequestOptions {
            thinking: Some(true),
            ..Default::default()
        };
        // DeepSeek 地址：带 thinking enabled，且如实报告已生效
        let ds = AiConfig {
            base_url: "https://api.deepseek.com".into(),
            api_key: "k".into(),
            model: "deepseek-chat".into(),
            ..Default::default()
        };
        let (body, applied) = ds.build_body_opts(&msgs, true, &opts);
        assert!(applied);
        assert_eq!(body["thinking"]["type"], "enabled");
        // 关：disabled
        let off = AiRequestOptions {
            thinking: Some(false),
            ..Default::default()
        };
        let (body, applied) = ds.build_body_opts(&msgs, true, &off);
        assert!(applied);
        assert_eq!(body["thinking"]["type"], "disabled");
        // 其他接口：不带参数、如实报告未生效
        let other = AiConfig {
            base_url: "https://api.openai.com/v1".into(),
            api_key: "k".into(),
            model: "gpt-4o-mini".into(),
            ..Default::default()
        };
        let (body, applied) = other.build_body_opts(&msgs, true, &opts);
        assert!(!applied);
        assert!(body.get("thinking").is_none());
        // 不指定（None）：谁都不带
        let (body, applied) = ds.build_body_opts(&msgs, true, &AiRequestOptions::default());
        assert!(!applied);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn 请求级参数覆盖配置级() {
        let cfg = AiConfig {
            base_url: "http://x/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            temperature: Some(0.7),
            max_tokens: Some(512),
            ..Default::default()
        };
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        let opts = AiRequestOptions {
            temperature: Some(0.2),
            top_p: Some(0.9),
            ..Default::default()
        };
        let (body, _) = cfg.build_body_opts(&msgs, false, &opts);
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 0.001);
        assert_eq!(body["max_tokens"], 512); // 没覆盖的仍用配置
        assert!((body["top_p"].as_f64().unwrap() - 0.9).abs() < 0.001);
    }

    #[test]
    fn 未配置时报错文案() {
        let c = AiClient::new(AiConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            ..Default::default()
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.chat_once(&[])).unwrap_err();
        assert!(matches!(err, AiError::NotConfigured));
        assert!(err.to_string().contains("尚未配置"));
    }

    #[test]
    fn 规范化修掉上游硬拒的消息形态() {
        // #13 真接口实测的拒收形态：assistant content=null 无 tool_calls、
        // tool 失去前置 tool_calls——规范化后都必须变成可达形态。
        let msgs = vec![
            RichMessage::text("user", "看代码"),
            RichMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            RichMessage {
                role: "tool".into(),
                content: Some(serde_json::json!("文件内容")),
                tool_calls: None,
                tool_call_id: Some("call_9".into()),
                reasoning_content: None,
            },
            RichMessage::text("user", "继续"),
        ];
        let out = normalize_messages(&msgs);
        assert_eq!(out[1]["content"], ""); // null → 空串（实测可过）
        assert_eq!(out[2]["role"], "user"); // 孤儿 tool 降级
        assert_eq!(out[2]["content"], "文件内容");
    }

    #[test]
    fn 规范化保留合法tool配对() {
        let msgs = vec![
            RichMessage {
                role: "assistant".into(),
                content: None,
                tool_calls: Some(serde_json::json!([{
                    "id": "call_1", "type": "function",
                    "function": {"name": "read", "arguments": "{}"}
                }])),
                tool_call_id: None,
                reasoning_content: None,
            },
            RichMessage {
                role: "tool".into(),
                content: Some(serde_json::json!("内容")),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                reasoning_content: None,
            },
        ];
        let out = normalize_messages(&msgs);
        assert!(out[0]["content"].is_null()); // 带 tool_calls 时 null 合法、原样
        assert!(out[0].get("tool_calls").is_some());
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["tool_call_id"], "call_1");
    }

    #[test]
    fn 规范化压平分段数组内容() {
        let msgs = vec![RichMessage {
            role: "user".into(),
            content: Some(serde_json::json!([
                {"type": "text", "text": "前"},
                {"type": "image_url", "image_url": {"url": "data:..."}},
                {"type": "text", "text": "后"}
            ])),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }];
        let out = normalize_messages(&msgs);
        assert_eq!(out[0]["content"], "前后");
    }

    /// #4 回归：多字节汉字在任意字节切口被劈开时，增量解码必须拼回
    /// 原文，全文不许出现替换符（旧 lossy 逐块解码在此必出 □□）。
    #[test]
    fn 增量解码任意切口不丢字() {
        let text = "一句话总结：蓝牙直连，不需要网络，也能聊天传文件。";
        let bytes = text.as_bytes();
        for cut in 0..=bytes.len() {
            let mut dec = Utf8StreamDecoder::new();
            let mut out = dec.push(&bytes[..cut]);
            out.push_str(&dec.push(&bytes[cut..]));
            out.push_str(&dec.finish());
            assert_eq!(out, text, "切口 {cut} 处解码不一致");
            assert!(!out.contains('\u{FFFD}'), "切口 {cut} 出现替换符");
        }
        // 逐字节喂入（最坏切法）
        let mut dec = Utf8StreamDecoder::new();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&dec.push(&[*b]));
        }
        out.push_str(&dec.finish());
        assert_eq!(out, text);
    }

    /// 增量解码：真非法字节只替换它自己，不连累后续合法文本。
    #[test]
    fn 增量解码非法字节只替换自身() {
        let mut dec = Utf8StreamDecoder::new();
        let mut out = dec.push(&[0xE4, 0xB8, 0x80, 0xFF, 0x61]);
        out.push_str(&dec.finish());
        assert_eq!(out, "一\u{FFFD}a");
    }
}
