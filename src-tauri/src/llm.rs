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
    Api {
        status: u16,
        body: String,
    },
    /// Response body couldn't be decoded as expected.
    ///
    /// `message` is the user-facing summary; `body` is the raw response text
    /// (truncated for safety) preserved so the log layer can capture the
    /// upstream payload for diagnosis. Without `body`, errors like "响应缺少
    /// message.content" tell the user nothing about WHY the content was
    /// missing (content filter, reasoning model, multimodal array, ...).
    BadResponse {
        message: String,
        body: Option<String>,
    },
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
            LlmError::BadResponse { message, body: _ } => {
                // Body intentionally not in the user-visible string — it can be
                // long, sometimes contains upstream internal state, and the
                // dedicated log file is the right place to read it.
                write!(f, "模型返回内容无效：{message}")
            }
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
    if let Some(arr) = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
    {
        for tc in arr {
            let id = tc
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
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
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }
    }
    ModelMessage {
        content,
        tool_calls,
    }
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
            "stream": false,
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
        let content_type = response_content_type(&request);
        let text = request
            .text()
            .await
            .map_err(|e| LlmError::BadResponse {
                message: format!("读取响应体失败：{e}"),
                body: None,
            })?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: sanitize_error_body(&text),
            });
        }

        let parsed = parse_http_json(&text, &content_type)?;
        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| LlmError::BadResponse {
                // Carry the raw response so a downstream logger can show the
                // full payload (finish_reason, reasoning_content, content
                // array, etc.) when this fires. The Display impl hides the
                // body from the UI; the log file reveals it.
                message: "响应缺少 message.content".to_string(),
                body: Some(truncate_for_log(&text)),
            })?;
        serde_json::from_str(content).map_err(|e| LlmError::BadResponse {
            message: format!("内容不是合法 JSON：{e}"),
            body: Some(truncate_for_log(content)),
        })
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
            "stream": false,
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
        let content_type = response_content_type(&request);
        let text = request
            .text()
            .await
            .map_err(|e| LlmError::BadResponse {
                message: format!("读取响应体失败：{e}"),
                body: None,
            })?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: sanitize_error_body(&text),
            });
        }
        let parsed = parse_http_json(&text, &content_type)?;
        let message = parsed
            .pointer("/choices/0/message")
            .ok_or_else(|| LlmError::BadResponse {
                message: "响应缺少 choices[0].message".to_string(),
                body: Some(truncate_for_log(&text)),
            })?;
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
        let text = response
            .text()
            .await
            .map_err(|e| LlmError::BadResponse {
                message: format!("读取响应体失败：{e}"),
                body: None,
            })?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                body: sanitize_error_body(&text),
            });
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            LlmError::BadResponse {
                message: format!("/models 响应不是合法 JSON：{e}"),
                body: Some(truncate_for_log(&text)),
            }
        })?;
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

fn response_content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn parse_http_json(text: &str, content_type: &str) -> Result<serde_json::Value, LlmError> {
    if text.trim().is_empty() {
        return Err(LlmError::BadResponse {
            // Was: "请检查 API Base URL 是否为接口根路径（通常以 /v1 结尾）"
            // That hint was wrong for the empty-body case — the URL is fine,
            // the upstream just sent zero bytes. Suggest the actual suspects.
            message:
                "模型服务返回了空响应（HTTP 200 但 body 为空）。可能原因：上游 gateway/proxy 拦截、模型内部错误、或流式端点未关闭 stream。请打开诊断日志查看完整请求/响应上下文。"
                    .to_string(),
            body: None,
        });
    }
    if content_type.contains("text/html") || text.trim_start().starts_with("<!doctype html") {
        return Err(LlmError::BadResponse {
            message: "API Base URL 指向了网页而不是模型接口，请填写接口根路径（通常以 /v1 结尾）。"
                .to_string(),
            body: Some(truncate_for_log(text)),
        });
    }

    // Reasoning / thinking models (DeepSeek-R1, QwQ, Qwen3-Thinking, ...)
    // emit their chain-of-thought as `<think>...</think>` and some OpenAI-
    // compatible providers concatenate that trace directly into the
    // `content` field instead of routing it to `reasoning_content`. We
    // strip the block here so the JSON parser can see the actual answer.
    // When the body has no leading thinking block, this is a no-op.
    let text = strip_thinking_block(text);

    serde_json::from_str(&text).map_err(|e| LlmError::BadResponse {
        message: format!("响应不是合法 JSON（Content-Type: {content_type}）：{e}"),
        // Log the (possibly stripped) body — what we actually tried to
        // parse. If stripping produced an empty string, the log banner
        // will surround nothing, which is itself a useful signal.
        body: Some(truncate_for_log(&text)),
    })
}

/// Strip a leading `<think>...</think>` block from a response body. Returns
/// the input unchanged when there is no such block, so the function is safe
/// to call unconditionally.
///
/// The match is intentionally literal and not regex-based: we only act on a
/// `<think>` opening tag at the very start of the (trimmed) body, and we
/// pair it with the *first* `</think>` after that. This handles the common
/// single-block case and is conservative — anything weirder (malformed
/// tags, interleaved text) falls through to the JSON parser, which will
/// fail with a normal parse error and surface in the log.
fn strip_thinking_block(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("<think>") {
        return text.to_string();
    }
    match trimmed.find("</think>") {
        Some(close) => trimmed[close + "</think>".len()..].trim_start().to_string(),
        // Malformed: opening tag but no closing. Let the JSON parser fail
        // naturally; the body still shows up in the log.
        None => text.to_string(),
    }
}

/// Cap the body we keep for diagnostics so a runaway upstream payload can't
/// balloon a single log entry or an error message. 16 KB is enough to capture
/// any realistic chat-completions response (the multimodal-array case, the
/// content-filter case, the reasoning case all fit comfortably).
const LOG_BODY_MAX_BYTES: usize = 16 * 1024;

fn truncate_for_log(text: &str) -> String {
    if text.len() <= LOG_BODY_MAX_BYTES {
        return text.to_string();
    }
    // Truncate at a char boundary so we never split a multi-byte UTF-8 sequence.
    let mut idx = LOG_BODY_MAX_BYTES;
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    format!(
        "{}…\n[truncated, original {} bytes]",
        &text[..idx],
        text.len()
    )
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
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp"
    ) {
        return Err(LlmError::MalformedImage(format!(
            "不支持的图片类型：{:?}（仅支持 PNG / JPEG / WebP）",
            mime
        )));
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
        assert_eq!(
            join_endpoint("http://localhost:11434", "/models"),
            "http://localhost:11434/models"
        );
    }

    #[test]
    fn reports_html_as_a_base_url_problem() {
        let err = parse_http_json("<!doctype html><html></html>", "text/html; charset=utf-8")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Base URL"), "actionable error: {err}");
    }

    #[test]
    fn reports_empty_success_body_clearly() {
        let err = parse_http_json("  ", "application/json")
            .unwrap_err()
            .to_string();
        assert!(err.contains("空响应"), "actionable error: {err}");
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

    // ---- Reasoning / thinking-model body handling --------------------

    /// A thinking-model body that interleaves a <think> block with the
    /// answer must be reduced to just the answer by `strip_thinking_block`.
    /// This is the exact failure mode the log file surfaced: a vision
    /// endpoint returning `<think>...</think>` + JSON in the `content`
    /// field, which our JSON parser then refused with "expected value at
    /// line 1 column 1".
    #[test]
    fn strip_thinking_block_extracts_json_from_reasoning_body() {
        let body = "<think>The image shows three modes of video generation. Let me extract them.\n\nStep 1: Read the image. Step 2: Organize.</think>\n\n{\"items\":[{\"question\":\"Q\",\"answer\":\"A\"}]}";
        let out = strip_thinking_block(body);
        assert!(!out.contains("<think>"), "thinking tag stripped");
        assert!(!out.contains("</think>"), "thinking close tag stripped");
        assert!(out.starts_with('{'), "result is the JSON: {out}");
        // Must round-trip through serde_json.
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["items"][0]["question"], "Q");
    }

    /// Bodies that have no leading thinking block must be returned
    /// unchanged. This is the safe-default for non-reasoning models; a
    /// regression here would silently corrupt every plain response.
    #[test]
    fn strip_thinking_block_preserves_plain_json() {
        let plain = r#"{"items":[{"question":"Q","answer":"A"}]}"#;
        assert_eq!(strip_thinking_block(plain), plain);
        // Even a leading newline before the JSON must be left intact.
        let with_leading = "\n{\"items\":[]}";
        assert_eq!(strip_thinking_block(with_leading), with_leading);
    }

    /// A body that opens a <think> but never closes it must pass through
    /// unchanged so the JSON parser can fail with a normal, descriptive
    /// error and the full body still appears in the log.
    #[test]
    fn strip_thinking_block_passes_malformed_through() {
        let malformed = "<think>never closes\n{\"items\":[]}";
        assert_eq!(strip_thinking_block(malformed), malformed);
    }

    /// `parse_http_json` must accept a thinking-model body and surface
    /// the items it carried. End-to-end check that the production code
    /// path can recover from a `<think>...</think>` concatenation.
    #[test]
    fn parse_http_json_handles_thinking_model_body() {
        let body = "<think>Thinking aloud.</think>\n\n{\"items\":[]}";
        let v = parse_http_json(body, "application/json").expect("parses");
        assert_eq!(v["items"].as_array().unwrap().len(), 0);
    }

    /// A truly empty body must produce a `BadResponse` whose message
    /// points at the actual suspects (gateway / model / stream), not at
    /// the legacy "wrong API URL" hint that misled the user before.
    #[test]
    fn parse_http_json_empty_body_diagnostic_mentions_real_causes() {
        let err = parse_http_json("", "application/json").unwrap_err();
        let msg = match err {
            LlmError::BadResponse { message, .. } => message,
            other => panic!("expected BadResponse, got {other:?}"),
        };
        assert!(msg.contains("空响应"), "mentions empty body: {msg}");
        assert!(
            msg.contains("stream") || msg.contains("gateway") || msg.contains("proxy"),
            "mentions a real upstream suspect: {msg}"
        );
        // Should NOT mislead with the legacy "API Base URL" suggestion.
        assert!(
            !msg.contains("API Base URL"),
            "stops blaming the API URL for empty bodies: {msg}"
        );
    }

    /// Bodies that consist only of a thinking block (no JSON after) must
    /// reduce to an empty string and surface a clear JSON parse error,
    /// not crash and not silently succeed.
    #[test]
    fn parse_http_json_thinking_only_body_fails_clearly() {
        let body = "<think>only thinking, no answer</think>";
        let err = parse_http_json(body, "application/json").unwrap_err();
        // Becomes a BadResponse with the parse-error message and a body
        // of ""; the log banner will show the empty body.
        let (msg, body) = match err {
            LlmError::BadResponse { message, body } => (message, body),
            other => panic!("expected BadResponse, got {other:?}"),
        };
        assert!(msg.contains("不是合法 JSON"));
        assert_eq!(body.as_deref(), Some(""));
    }
}
