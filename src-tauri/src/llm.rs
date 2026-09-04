// Reusable OpenAI-compatible async client.
//
// This is the single HTTP layer shared by all LLM calls. Vision extraction
// (this phase) and future Chat both funnel through `chat_json` — one request
// builder, one transport, one error type. Nothing here knows about images or
// knowledge schemas; callers build the OpenAI message array and parse content.
//
// The API key is attached only to the outgoing Authorization header. It is
// never included in any error value or log line (errors carry HTTP status +
// server body, both of which are key-free).
//
//   POST {base}/chat/completions
//   {
//     "model": "...",
//     "messages": [...],
//     "temperature": 0.2,
//     "max_tokens": ...,
//     "response_format": { "type": "json_object" }
//   }

use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Debug)]
pub enum LlmError {
    /// No API key configured (Settings not filled in).
    MissingConfig(String),
    MalformedImage(String),
    /// Transport / connectivity failure (DNS, refused, TLS, timeout).
    Network(String),
    /// The endpoint answered but with a non-2xx status.
    Api { status: u16, body: String },
    /// Response body couldn't be decoded as expected.
    BadResponse(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::MissingConfig(m) => write!(f, "{m}"),
            LlmError::MalformedImage(m) => write!(f, "图片格式无效：{m}"),
            LlmError::Network(m) => write!(f, "网络请求失败：{m}"),
            LlmError::Api { status, body } => {
                write!(f, "模型服务返回错误（HTTP {status}）：{body}")
            }
            LlmError::BadResponse(m) => write!(f, "模型返回内容无效：{m}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// A function call requested by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String, // JSON string
}

/// A parsed assistant message: plain `content` and/or `tool_calls`.
#[derive(Debug, Clone)]
pub struct ModelMessage {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// Parse an OpenAI chat-completions `message` object into a ModelMessage.
/// Malformed tool_calls entries are skipped (we only keep calls with an id and a
/// name); a message without tool_calls simply yields empty content or content.
pub fn parse_model_message(message: &serde_json::Value) -> ModelMessage {
    let content = message
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    let mut tool_calls = Vec::new();
    if let Some(arr) = message.get("tool_calls").and_then(serde_json::Value::as_array) {
        for tc in arr {
            let id = tc.get("id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
            let name = tc
                .pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = tc
                .pointer("/function/arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            if !id.is_empty() && !name.is_empty() {
                tool_calls.push(ToolCall { id, name, arguments });
            }
        }
    }
    ModelMessage { content, tool_calls }
}

#[derive(Clone)]
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiCompatClient {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .connect_timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client config");
        Self {
            http,
            base_url,
            api_key,
            model,
        }
    }

    fn chat_endpoint(&self) -> String {
        join_chat_completions_url(&self.base_url)
    }

    /// Shared chat-completion call. `messages` is the full OpenAI messages
    /// array (system/user/assistant). Requests JSON output and returns the
    /// parsed JSON of the assistant's first-choice message content.
    pub async fn chat_json(
        &self,
        messages: Vec<serde_json::Value>,
        max_tokens: u32,
    ) -> Result<serde_json::Value, LlmError> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.2,
            "max_tokens": max_tokens,
            "response_format": { "type": "json_object" },
        });

        let request = self
            .http
            .post(self.chat_endpoint())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let status = request.status();
        let text = request.text().await.map_err(|e| LlmError::BadResponse(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: sanitize_error_body(&text),
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::BadResponse(format!("响应不是合法 JSON：{e}")))?;
        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| LlmError::BadResponse("响应缺少 message.content".to_string()))?;
        serde_json::from_str(content)
            .map_err(|e| LlmError::BadResponse(format!("内容不是合法 JSON：{e}")))
    }

    /// Standard OpenAI-compatible tool-calling chat completion.
    ///
    /// `tools` is the `tools` array (or None). Unlike `chat_json` this does NOT
    /// force `response_format: json_object`, so the model may return free-form
    /// content and/or `tool_calls`. Returns the parsed assistant message.
    ///
    /// If the provider does not support the standard `tools` / `tool_calls` /
    /// `role=tool` protocol it will answer with a non-2xx status which we surface
    /// as an error — we never silently fall back to text-emulated tools.
    pub async fn chat_with_tools(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
    ) -> Result<ModelMessage, LlmError> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "temperature": 0.3,
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }

        let request = self
            .http
            .post(self.chat_endpoint())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Network(e.to_string()))?;
        let status = request.status();
        let text = request.text().await.map_err(|e| LlmError::BadResponse(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: sanitize_error_body(&text),
            });
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::BadResponse(format!("响应不是合法 JSON：{e}")))?;
        let message = parsed
            .pointer("/choices/0/message")
            .ok_or_else(|| LlmError::BadResponse("响应缺少 choices[0].message".to_string()))?;
        Ok(parse_model_message(message))
    }

    /// Lightweight connectivity check against `GET {base}/models`. Confirms the
    /// base URL and API key are accepted without spending a heavy call.
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let url = join_endpoint(&self.base_url, "/models");
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| LlmError::Network(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| LlmError::BadResponse(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: sanitize_error_body(&text),
            });
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::BadResponse(e.to_string()))?;
        let models = parsed
            .pointer("/data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(models)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Join a base URL (e.g. `https://api.openai.com/v1`) with a path suffix such
/// as `/chat/completions`, tolerating a trailing slash on the base.
pub fn join_endpoint(base: &str, suffix: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), suffix)
}

pub fn join_chat_completions_url(base: &str) -> String {
    join_endpoint(base, "/chat/completions")
}

/// Server-produced error bodies occasionally leak request payloads; keep only
/// the first line and strip anything that looks like a key/token.
fn sanitize_error_body(body: &str) -> String {
    let first = body.lines().next().unwrap_or_default().to_string();
    if first.len() > 300 {
        return format!("{}…", &first[..300]);
    }
    first
}

/// Parse a `data:image/png;base64,XXXX` URL into (mime, raw bytes). Rejects
/// non-image mime types and undecodable payloads.
pub fn parse_data_url(url: &str) -> Result<(String, Vec<u8>), LlmError> {
    let (meta, payload) = url
        .strip_prefix("data:")
        .and_then(|rest| {
            let (m, b) = rest.split_once(',')?;
            Some((m, b))
        })
        .ok_or_else(|| LlmError::MalformedImage("缺少 data: 前缀".to_string()))?;

    let mime = meta.split(';').next().unwrap_or("").trim().to_lowercase();
    if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/jpg" | "image/webp") {
        return Err(LlmError::MalformedImage(format!("不支持的图片类型：{:?}（仅支持 PNG / JPEG / WebP）", mime)));
    }

    // base64 crate "standard" alphabet; tolerate optional newlines.
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|_| LlmError::MalformedImage("图片数据不是合法 base64".to_string()))?;
    if bytes.is_empty() {
        return Err(LlmError::MalformedImage("图片数据为空".to_string()));
    }
    Ok((mime, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_endpoint_tolerates_trailing_slash() {
        assert_eq!(
            join_chat_completions_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            join_chat_completions_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(join_endpoint("http://localhost:11434", "/models"), "http://localhost:11434/models");
    }

    #[test]
    fn parses_valid_png_data_url() {
        let url = "data:image/png;base64,aGVsbG8=";
        let (mime, bytes) = parse_data_url(url).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes, b"hello");
    }

    #[test]
    fn rejects_unsupported_mime() {
        let err = parse_data_url("data:image/gif;base64,AAAA").unwrap_err();
        assert!(matches!(err, LlmError::MalformedImage(_)));
    }

    #[test]
    fn rejects_bad_base64() {
        let err = parse_data_url("data:image/png;base64,!!!not-base64!!!").unwrap_err();
        assert!(matches!(err, LlmError::MalformedImage(_)));
    }

    #[test]
    fn malformed_image_error_is_key_free_and_human() {
        let err = parse_data_url("not-a-url").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("图片格式无效"), "human readable: {msg}");
    }

    #[test]
    fn parses_message_with_tool_calls() {
        let msg = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "knowledge_search", "arguments": "{\"query\":\"BERT\"}" }
            }]
        });
        let m = parse_model_message(&msg);
        assert!(m.content.is_none());
        assert_eq!(m.tool_calls.len(), 1);
        assert_eq!(m.tool_calls[0].id, "call_1");
        assert_eq!(m.tool_calls[0].name, "knowledge_search");
        assert_eq!(m.tool_calls[0].arguments, r#"{"query":"BERT"}"#);
    }

    #[test]
    fn parses_plain_final_message() {
        let msg = serde_json::json!({ "role": "assistant", "content": "你好！" });
        let m = parse_model_message(&msg);
        assert_eq!(m.content.as_deref(), Some("你好！"));
        assert!(m.tool_calls.is_empty());
    }

    #[test]
    fn skips_malformed_tool_calls() {
        let msg = serde_json::json!({
            "role": "assistant",
            "tool_calls": [
                { "id": "", "function": { "name": "x", "arguments": "{}" } },
                { "id": "ok", "function": { "name": "y", "arguments": "{}" } }
            ]
        });
        let m = parse_model_message(&msg);
        assert_eq!(m.tool_calls.len(), 1);
        assert_eq!(m.tool_calls[0].name, "y");
    }
}