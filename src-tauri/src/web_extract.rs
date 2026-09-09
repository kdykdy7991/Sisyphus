use crate::db::KnowledgePayload;
use crate::llm::{LlmError, OpenAiCompatClient};
use crate::vision::{parse_drafts, to_knowledge_payloads, VISION_SYSTEM_PROMPT};

// WeChat article pages often include sizeable scripts and embedded metadata in
// addition to the article body. Keep a bounded response, but leave enough room
// for those pages; only the cleaned article text is sent to the model.
const MAX_HTML_BYTES: usize = 20 * 1024 * 1024;
const MAX_ARTICLE_CHARS: usize = 80_000;

fn validate_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value.trim()).map_err(|_| "请输入有效的网页链接。".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("仅支持 http 或 https 网页链接。".to_string());
    }
    let host = url.host_str().ok_or_else(|| "链接缺少域名。".to_string())?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err("不允许访问本机地址。".to_string());
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let blocked = match ip {
            std::net::IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified(),
            std::net::IpAddr::V6(v) => v.is_loopback() || v.is_unspecified() || v.is_unique_local() || v.is_unicast_link_local(),
        };
        if blocked { return Err("不允许访问本机或局域网地址。".to_string()); }
    }
    Ok(url)
}

fn decode_entities(value: &str) -> String {
    value.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<")
        .replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

fn strip_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let article = if let Some(id) = lower.find("id=\"js_content\"").or_else(|| lower.find("id='js_content'")) {
        let start = html[..id].rfind('<').unwrap_or(id);
        &html[start..]
    } else { html };
    let mut out = String::with_capacity(article.len().min(MAX_ARTICLE_CHARS));
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skip: Option<&str> = None;
    for ch in article.chars() {
        if ch == '<' { in_tag = true; tag.clear(); continue; }
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                if t.starts_with("script") { skip = Some("script"); }
                else if t.starts_with("style") { skip = Some("style"); }
                else if t.starts_with("/script") || t.starts_with("/style") { skip = None; }
                else if skip.is_none() && (t.starts_with("br") || t.starts_with("/p") || t.starts_with("/section") || t.starts_with("/li") || t.starts_with("/h")) { out.push('\n'); }
            } else { tag.push(ch); }
            continue;
        }
        if skip.is_none() { out.push(ch); }
        if out.chars().count() >= MAX_ARTICLE_CHARS { break; }
    }
    let decoded = decode_entities(&out);
    decoded.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join("\n")
}

fn page_title(html: &str, fallback: &str) -> String {
    let lower = html.to_ascii_lowercase();
    lower.find("<title").and_then(|start| lower[start..].find('>').map(|x| start + x + 1))
        .and_then(|start| lower[start..].find("</title>").map(|x| decode_entities(html[start..start+x].trim())))
        .filter(|x| !x.is_empty()).unwrap_or_else(|| fallback.to_string())
}

pub async fn extract_from_url(
    client: &OpenAiCompatClient, url: &str, max_tokens: u32,
    existing_categories: &[(String, String)],
) -> Result<Vec<KnowledgePayload>, LlmError> {
    let parsed = validate_url(url).map_err(LlmError::InvalidInput)?;
    let response = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30))
        .user_agent("Mozilla/5.0 Sisyphus/0.1").build().map_err(|e| LlmError::Network(e.to_string()))?
        .get(parsed.clone()).send().await.map_err(|e| LlmError::Network(e.to_string()))?;
    if !response.status().is_success() { return Err(LlmError::Api { status: response.status().as_u16(), body: "网页无法访问或需要登录验证。".to_string() }); }
    if let Some(size) = response.content_length().filter(|n| *n > MAX_HTML_BYTES as u64) {
        return Err(LlmError::InvalidInput(format!(
            "网页响应为 {:.2} MiB，超过当前 20 MiB 限制。",
            size as f64 / 1024.0 / 1024.0
        )));
    }
    let bytes = response.bytes().await.map_err(|e| LlmError::Network(e.to_string()))?;
    if bytes.len() > MAX_HTML_BYTES {
        return Err(LlmError::InvalidInput(format!(
            "网页响应为 {:.2} MiB，超过当前 20 MiB 限制。",
            bytes.len() as f64 / 1024.0 / 1024.0
        )));
    }
    let html = String::from_utf8_lossy(&bytes);
    let title = page_title(&html, parsed.host_str().unwrap_or("网页文章"));
    let text = strip_html(&html);
    if text.chars().count() < 30 { return Err(LlmError::InvalidInput("未读取到足够的文章正文；该页面可能需要登录或访问验证。".to_string())); }
    let topics = existing_categories.iter().map(|(_, t)| t.trim()).filter(|t| !t.is_empty()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join("、");
    let instruction = format!("请从以下网页文章中提取所有能够形成完整内容的独立面试问答，不限一组。完整扫描全文，不要只返回第一组；每个问题生成一个 item，并确保答案与问题正确配对。只能选择已有 Topic：{}（没有合适项则返回空字符串）。只返回与图片提取相同的 {{\"items\": [...]}} JSON 结构。\n\n文章标题：{}\n文章链接：{}\n\n文章正文：\n{}", if topics.is_empty() { "（无，topic 必须为空）" } else { &topics }, title, parsed, text);
    let messages = vec![serde_json::json!({"role":"system","content":VISION_SYSTEM_PROMPT}), serde_json::json!({"role":"user","content":instruction})];
    let content = client.chat_json(messages, max_tokens).await?;
    let drafts = parse_drafts(&content.to_string()).map_err(|message| LlmError::BadResponse { message, body: Some(content.to_string()) })?;
    Ok(to_knowledge_payloads(&drafts, &format!("{} · {}", title, parsed), existing_categories))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn rejects_local_urls() { assert!(validate_url("http://127.0.0.1/x").is_err()); assert!(validate_url("file:///tmp/a").is_err()); }
    #[test] fn extracts_wechat_article_body() {
        let html = r#"<title>面试合集</title><script>noise()</script><div id="js_content"><h2>问题一</h2><p>答案一</p><p>问题二<br>答案二</p></div>"#;
        let text = strip_html(html); assert!(text.contains("问题一\n答案一")); assert!(text.contains("问题二\n答案二")); assert!(!text.contains("noise"));
        assert_eq!(page_title(html, "fallback"), "面试合集");
    }
}
