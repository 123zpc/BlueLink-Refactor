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
        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => AiError::Auth,
                429 => AiError::RateLimited,
                code => AiError::Server(code),
            });
        }
        let mut full = String::new();
        let mut buffer = String::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|_| AiError::Network)?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                if let Some(delta) = parse_sse_delta(line.trim()) {
                    full.push_str(&delta);
                    on_delta(&delta);
                }
            }
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
        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => AiError::Auth,
                429 => AiError::RateLimited,
                code => AiError::Server(code),
            });
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
}
