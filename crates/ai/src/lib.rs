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

/// AI 配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl AiConfig {
    pub fn is_ready(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
            && !self.model.trim().is_empty()
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
        mut on_delta: F,
    ) -> Result<String, AiError> {
        self.check_ready()?;
        let body = serde_json::json!({
            "model": self.cfg.model,
            "messages": messages,
            "stream": true,
        });
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
        Ok(full)
    }

    /// 非流式对话（代理模式与连通性测试用）。
    pub async fn chat_once(&self, messages: &[ChatMessage]) -> Result<String, AiError> {
        self.check_ready()?;
        let body = serde_json::json!({
            "model": self.cfg.model,
            "messages": messages,
            "stream": false,
        });
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
        v.pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or(AiError::BadResponse)
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
    fn 未配置时报错文案() {
        let c = AiClient::new(AiConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(c.chat_once(&[])).unwrap_err();
        assert!(matches!(err, AiError::NotConfigured));
        assert!(err.to_string().contains("尚未配置"));
    }
}
