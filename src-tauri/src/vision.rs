// Vision extraction: turn a screenshot into 0..N structured knowledge drafts.
//
// This module owns the Vision system prompt (kept centrally in the backend,
// not scattered in React) and the structured-output contract. The model is asked
// for a stable JSON shape and every response is runtime-validated here before
// any draft is allowed to reach the Review screen — malformed / empty results
// never become drafts.

use crate::db::KnowledgePayload;
use crate::llm::{parse_data_url, OpenAiCompatClient};

/// Backend-owned system prompt. The model is a personal-interview knowledge
/// organizer, not an OCR dump: it must preserve meaning, may polish phrasing,
/// must not invent facts, must split independent questions, and — most
/// importantly for this revision — must pick a layout that matches the
/// answer's *content* (parallel points / steps / comparisons / short answer)
/// instead of collapsing everything into one long paragraph.
///
/// The prompt is written as a Rust raw string literal so its newlines,
/// numbered list markers, indented sub-explanations, and code-fence examples
/// are preserved byte-for-byte when serialised into the request. A previous
/// version used line-continuation `"\"` which is correct, but is harder to
/// read and easy to drift away from the desired layout during future edits.
pub const VISION_SYSTEM_PROMPT: &str = r###"
你正在从面试相关截图中提取并整理个人知识库内容。

目标不是逐字 OCR，而是理解截图内容，并整理成清晰、适合后续复习的问答知识。

## 1. 独立问题拆分
- 每个独立面试问题生成一个 item。
- 如果截图包含多个问题，必须拆成多个 item。
- 不要把多个不同问题合并成一个问答。

## 2. question
- 保留原问题核心含义。
- 可以轻微整理，使表达完整、自然。
- 不改变问题本身的考察重点。

## 3. answer 的目标与内容优先级
answer 是适合快速复习的编辑后技术内容，不是 OCR 原文、长篇技术文章、AI 解释作文或 Markdown 结构展示。

内容按以下优先级组织：核心结论 → 关键机制 / 关键事实 → 必要解释 → 必要例子。

- 保留截图中的有效信息、原意、必要顺序和真实代码。
- 可以整理表达、修正明显口语化和碎片化内容。
- 删除或合并重复表达、无意义铺垫、AI 式过渡句，以及与截图核心知识无关的扩写。
- 不要凭空补充截图中没有的信息，也不要为了显得完整而扩写通用知识。

## 4. 标题与总体结构
根据内容本身决定排版，不要机械套模板。结构选择优先级是：普通段落 → 同级列表 → `##` → `###`。不要把 Heading 当成默认分点工具。

- `##` 只用于真正不同的内容章节，例如“核心原理”“工作流程”“优点与局限”。
- 不要把单个知识点各自做成 `##`。多个原因、机制、特点应归入一个必要章节，并使用同级列表。
- 大多数答案保持 2 个主要结构层级；复杂答案最多 3 个主要结构层级。
- 正常只使用 `##`；`###` 仅在确实存在独立子章节时使用。
- `###` 和嵌套子列表尽量不要同时出现。
- 能用同级列表表达的内容，不继续向下嵌套；优先横向拆分，不要不断纵向加层。
- 同一级内容保持相同结构。

不要生成这种结构：

## 基于内存
## 单线程
## I/O 多路复用
## 高效数据结构

应优先整理为：

## 关键原因

- **内存访问**：主要数据位于内存，减少磁盘随机 I/O。
- **单线程执行**：减少线程切换和锁竞争。
- **I/O 多路复用**：单线程也可以高效处理大量连接。
- **高效数据结构**：针对常见操作降低处理开销。

## 5. 知识点表达方式
- 两个及以上并列知识点必须分点，每个列表项只表达一个核心意思。
- 并列知识点优先采用 `- **关键词**：解释。`。
- 关键词要短、明确；解释直接进入结论，不写“之所以……其中一个原因是……”等冗长铺垫。
- 短答案不强制使用列表。
- 一个要点确需展开时，可在下一层使用 2~4 个空格缩进；列表最多一层子列表。

## 6. 不同题型的推荐结构
- 定义题：直接使用 1~3 句回答，不强行添加标题或列表。
- 原理题：先给核心结论，再列关键机制或因果链。
- 原因题：先给总括，原因使用同级列表。
- 比较题：按比较维度组织，不要先完整讲完 A，再完整讲完 B。例如在“## 核心区别”下使用“**架构**”“**训练目标**”“**使用场景**”等同级条目，并在每个维度内同时说明 A 与 B。
- 流程题：严格按照发生顺序使用有序列表，不打乱流程顺序。
- 优缺点题：内容充分时可使用“## 优点”“## 局限”“## 适用场景”；内容较少时只使用列表，不强制标题。
- 代码题：代码块与解释分离，不把大段代码混进普通段落；只保留截图中真实存在或明确表达的代码。

## 7. 首段与核心结论
- 不强制每个 answer 都有 summary。
- 内容较复杂时，允许开头先写 1~2 句核心结论，再展开具体内容。
- 短定义题直接回答即可。
- 不要生成固定的“## 核心结论”标题，除非内容本身确实需要这个独立章节。

## 8. Markdown Contract
- 页面已经展示 question，answer 中禁止使用 `#` 一级标题。
- 允许使用：普通段落、`##`、`###`、有序列表、无序列表、最多一层子列表、`**粗体**`、行内代码、代码块、引用。
- 禁止使用：`####` 及更深标题、表格、链接、图片、原始 HTML、水平分隔线、斜体，以及上述范围之外的 Markdown。
- blockquote 的正式语义是 Important / 重点提醒。生成每个 answer 前，必须主动判断截图内容中是否存在值得用户单独记忆的关键句。
- 如果存在以下内容，应优先选择其中最重要的一句话使用 blockquote：易混淆或常见误区、反直觉事实、决定整题理解正确性的核心结论、关键边界 / 前提 / 限制条件、面试中用于区分理解深度的重要结论、明显注意事项或错误认知纠正。
- Important 必须是一句完整、独立的话；单独拿出来仍然有意义，并且值得用户快速复习时停下来记住。只使用标准 Markdown 的 `> 内容` 表达。
- 不要标记普通定义、普通知识点、普通列表项或重复前文的总结；不要为了“好看”生成 blockquote，也不要为了其他视觉效果制造重点。
- 每个 answer 通常使用 0~1 个 blockquote：简单定义题可以是 0 个；如果答案中存在明确 Important，应生成 1 个。复杂答案最多 2 个，禁止为了凑数量强行生成。
- 不得输出任意颜色、HTML style、CSS class 或其他自定义 HTML；Important 只通过标准 Markdown blockquote 表达，具体颜色与样式由页面决定。

应该生成 Important 的完整 answer 示例 1：

Redis 的高性能来自内存访问、简化的命令执行模型和高效的网络处理机制。

## 关键原因

- **内存访问**：主要数据位于内存，减少磁盘随机 I/O。
- **命令执行**：核心命令串行执行，减少线程切换和锁竞争。
- **网络处理**：通过 I/O 多路复用高效处理大量连接。

> Redis 的“单线程”主要指核心命令执行线程，并不意味着所有网络 I/O 和后台任务都只能使用一个线程。

应该生成 Important 的完整 answer 示例 2：

MVCC 通过保存数据的多个版本，让事务读取符合其可见性规则的版本，从而减少读写之间的相互阻塞。

## 关键机制

- **版本记录**：数据更新时保留可用于构造历史版本的信息。
- **可见性判断**：事务根据快照和事务状态判断哪个版本可见。
- **并发读取**：普通读取可以访问合适的历史版本，不必等待当前写入完成。

> MVCC 的核心目标不是消除所有并发冲突，而是让读写尽可能互不阻塞；写写冲突仍需要并发控制。

不应该生成 Important 的完整 answer 示例：

HTTP 是一种应用层协议，用于客户端和服务器之间传输请求与响应。它本身是无状态的，每个请求原则上独立处理。

## 9. answer 使用换行保留层级
answer 是一个 string，请用换行与列表体现层级。模型看到的 answer 里，
真实换行会原样保存到知识库；不要在 answer 里写 \\n 字面量。

可以参考以下形态：

核心结论。

1. 第一个关键点
   进一步解释。

2. 第二个关键点
   进一步解释。

或：

## 核心区别

- A：……
- B：……

## 10. domain / topic / tags
- Topic 是用户自己创建和维护的收藏夹。你只能从 user instruction 提供的已有 Topic 列表中选择，并且必须原样返回已有名称。
- 禁止创建、改写、扩展或组合新的 Topic 名称。不要把 question、答案摘要或更细的知识点当作新 Topic。
- 只有内容明确属于某个已有 Topic 时才选择它；没有合适 Topic 时，topic 返回空字符串。不要为了分类而勉强选择最接近的 Topic。
- 如果用户当前没有任何 Topic，topic 必须返回空字符串。
- domain 是旧数据兼容字段，本次不要主动创建或推断 Domain，始终返回空字符串；后端会根据最终 Topic 保留兼容值。
- tags 继续由你自动生成 1~4 个，必须简洁、可搜索，并描述该 Knowledge 的实际知识点。
- tags 不要使用“技术”“知识”“面试”“基础”等宽泛、无实际检索价值的词。

## 11. followUpQuestions
- 只生成与当前问题紧密相关、面试中可能继续追问的问题。
- 不为了数量强行生成。
- 没有则返回空数组。

## 12. 如果截图内容不足以形成有效知识
- 可以返回空 items。
- 不猜测或编造缺失内容。

## 13. 输出前内部自检
输出 JSON 前在内部检查以下项目，不要输出检查过程：
- 是否把多个并列知识点挤成了长段落。
- 是否为单个知识点创建了过多 `##`。
- 是否存在 `####` 或更深标题。
- 是否存在超过一层的子列表。
- 是否重复表达同一个结论。
- 是否存在无意义铺垫或 AI 式过渡句。
- 是否生成了截图中没有的信息。
- 是否使用了 Markdown Contract 禁止的语法。
- 是否存在明显的易错点、反直觉事实、关键边界或决定理解正确性的核心结论？如果存在，是否已经选择最重要的一句话标记为 Important？

只返回合法 JSON，不输出 JSON 之外的文字，也不要使用 Markdown 代码块包裹。
"###;

/// The user-facing instruction accompanying the image. It stays short and
/// just points the model at the per-image task + JSON shape; the formatting
/// rules live in the system prompt so a single edit covers every call.
const VISION_USER_INSTRUCTION: &str = r###"
请从这张图片中识别并整理所有独立的面试题，严格遵循 system 中的整理与排版规则，
并按下面的 JSON 结构输出（不要输出任何解释、前后缀或 Markdown 代码块）：

{
  "items": [
    {
      "question": "整理后的问题",
      "answer": "整理后的答案，使用换行和列表保留层级（真实换行，不是 \\n）",
      "domain": "",
      "topic": "已有 Topic 名称或空字符串",
      "tags": ["标签1", "标签2"],
      "followUpQuestions": ["相关追问"]
    }
  ]
}

如果图片中不包含任何面试题，则输出 {"items":[]}。
"###;

/// Build the OpenAI messages array for a single image. Each image is extracted
/// independently; image order/adjacency is never used to merge content.
pub fn vision_messages(
    image_data_url: &str,
    existing_categories: &[(String, String)],
) -> Vec<serde_json::Value> {
    let categories = if existing_categories.is_empty() {
        "当前没有已创建的 Topic。所有提取结果的 topic 都必须返回空字符串并保持未分类。禁止自行创建 Topic。domain 也必须返回空字符串。".to_string()
    } else {
        let topics = existing_categories
            .iter()
            .map(|(_, topic)| topic.trim())
            .filter(|topic| !topic.is_empty())
            .collect::<std::collections::BTreeSet<_>>();
        let list = topics
            .into_iter()
            .map(|topic| format!("- {topic}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("以下是用户已经创建的 Topic。你只能从这些 Topic 中选择并原样返回：\n{list}\n\n如果内容不明确属于任何已有 Topic，topic 返回空字符串并保持未分类。禁止创建、改写或组合新的 Topic 名称，也不要强行选择最接近的 Topic。domain 必须返回空字符串。")
    };
    let instruction = format!("{VISION_USER_INSTRUCTION}\n\n{categories}");
    let content = vec![
        serde_json::json!({"type": "text", "text": instruction}),
        serde_json::json!({"type": "image_url", "image_url": {"url": image_data_url}}),
    ];
    vec![
        serde_json::json!({"role": "system", "content": VISION_SYSTEM_PROMPT}),
        serde_json::json!({"role": "user", "content": content}),
    ]
}

/// Structured contract for one extracted draft. Field names are camelCase to
/// match the model's JSON and the frontend `Knowledge` type.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisionDraft {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub follow_up_questions: Vec<String>,
}

fn contains_single_emphasis(text: &str, marker: char) -> bool {
    let chars = text.chars().collect::<Vec<_>>();
    let mut open = false;
    for (index, current) in chars.iter().enumerate() {
        if *current != marker {
            continue;
        }
        let adjacent_same = index.checked_sub(1).is_some_and(|i| chars[i] == marker)
            || chars.get(index + 1).is_some_and(|c| *c == marker);
        if adjacent_same {
            continue; // `**bold**` is part of the allowed contract.
        }
        if open {
            return true;
        }
        open = true;
    }
    false
}

fn validate_answer_markdown(answer: &str) -> Result<(), String> {
    let mut in_code_fence = false;
    for line in answer.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }

        // Keep this deliberately line-oriented: the extraction contract is
        // intentionally small, so common forbidden constructs can be rejected
        // without adding a second Markdown parser to the backend.
        let compact = trimmed.trim();
        let thematic_rule = compact.len() >= 3
            && compact
                .chars()
                .filter(|c| !c.is_whitespace())
                .all(|c| c == '-' || c == '*' || c == '_')
            && compact.chars().filter(|c| !c.is_whitespace()).count() >= 3;
        if thematic_rule {
            return Err("answer 不允许使用水平分隔线".to_string());
        }
        if compact.contains("](") && compact.contains('[') {
            return Err("answer 不允许使用链接或图片".to_string());
        }
        if (compact.starts_with('<') && compact.contains('>')) || compact.contains("</") {
            return Err("answer 不允许使用原始 HTML".to_string());
        }
        if compact.starts_with('|') || compact.ends_with('|') {
            return Err("answer 不允许使用 Markdown 表格".to_string());
        }
        if contains_single_emphasis(compact, '*') || contains_single_emphasis(compact, '_') {
            return Err("answer 不允许使用斜体".to_string());
        }

        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if hashes > 0
            && trimmed.chars().nth(hashes).is_some_and(char::is_whitespace)
            && hashes != 2
            && hashes != 3
        {
            return Err("answer 标题只允许使用 ## 或 ###".to_string());
        }

        let indent = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum::<usize>();
        let unordered = ["- ", "* ", "+ "]
            .iter()
            .any(|marker| trimmed.starts_with(marker));
        let ordered = trimmed.split_once(". ").is_some_and(|(number, _)| {
            !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        });
        if (unordered || ordered) && indent > 4 {
            return Err("answer 列表最多允许一层子列表".to_string());
        }
    }
    Ok(())
}

/// Parse and strictly validate the model's content string into drafts.
/// Returns an error (never silently bad data) for invalid JSON, missing/changed
/// types, or any item with a blank question/answer. An empty `items: []` is a
/// valid result meaning "no questions found".
pub fn parse_drafts(content: &str) -> Result<Vec<VisionDraft>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("模型返回的不是合法 JSON：{e}"))?;

    let items = parsed
        .get("items")
        .and_then(|i| i.as_array())
        .ok_or_else(|| "模型返回缺少 items 数组".to_string())?;

    let mut drafts = Vec::with_capacity(items.len());
    for item in items {
        let draft: VisionDraft = serde_json::from_value(item.clone())
            .map_err(|e| format!("items 中的条目字段类型错误：{e}"))?;
        if draft.question.trim().is_empty() {
            return Err("存在一条草稿缺少 question（问题）".to_string());
        }
        if draft.answer.trim().is_empty() {
            return Err(format!(
                "草稿「{}」缺少 answer（答案）",
                draft.question.trim()
            ));
        }
        validate_answer_markdown(&draft.answer)
            .map_err(|e| format!("草稿「{}」格式不符合要求：{e}", draft.question.trim()))?;
        let topic = draft.topic.trim();
        let question_like = ["什么是", "为什么", "如何", "怎么", "哪些", "是否"]
            .iter()
            .any(|word| topic.contains(word));
        if !topic.is_empty() && (topic.chars().count() > 24
            || topic.contains(['\n', '\r', '？', '?'])
            || topic == draft.question.trim()
            || question_like)
        {
            return Err(format!(
                "草稿「{}」的主题不够简洁（主题应不超过 24 个字符且不能是问句）",
                draft.question.trim()
            ));
        }
        drafts.push(draft);
    }
    Ok(drafts)
}

/// Convert validated drafts into frontend `Knowledge`-compatible payloads,
/// assigning fresh draft ids, timestamps and a source label. On confirm these
/// are inserted into SQLite (numeric id replaces the draft id).
pub fn to_knowledge_payloads(
    drafts: &[VisionDraft],
    source: &str,
    existing_categories: &[(String, String)],
) -> Vec<KnowledgePayload> {
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
    drafts
        .iter()
        .map(|d| {
            // The model's category fields are suggestions, never authority.
            // Only an exact existing Topic is accepted; its stored legacy
            // Domain is reused. Everything else is persisted as uncategorized.
            let selected = d.topic.trim();
            let matched = existing_categories
                .iter()
                .find(|(_, topic)| topic.trim() == selected && !selected.is_empty());
            let (domain, topic) = matched
                .map(|(domain, topic)| (domain.trim().to_string(), topic.trim().to_string()))
                .unwrap_or_else(|| ("未分类".to_string(), String::new()));
            KnowledgePayload {
            id: format!("draft-{}", uuid()),
            // Empty sync_id -> save() will mint a fresh UUIDv4 on insert.
            sync_id: String::new(),
            question: d.question.trim().to_string(),
            answer: d.answer.trim().to_string(),
            domain,
            topic,
            tags: d
                .tags
                .iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .take(4)
                .collect(),
            follow_ups: d.follow_up_questions.clone(),
            related_ids: Vec::new(),
            source: format!("截图导入 · {source}"),
            created_at: now.clone(),
            updated_at: now.clone(),
            favorite: Some(false),
            last_read_at: None,
            deleted_at: None,
        }})
        .collect()
}

/// Random hex id for a draft (not persisted).
fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:06x}", randish() % 0x1000000)
}

/// Small unseeded pseudo-random helper so we don't pull in a rand dependency.
fn randish() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() ^ d.subsec_nanos() as u64) as u64)
        .unwrap_or(7)
}

/// Shared tail after a successful LLM call: validate the structured output and
/// map drafts to frontend payloads.
async fn run_extraction(
    client: &OpenAiCompatClient,
    messages: Vec<serde_json::Value>,
    source: &str,
    max_tokens: u32,
    existing_categories: &[(String, String)],
) -> Result<Vec<KnowledgePayload>, crate::llm::LlmError> {
    let content = client.chat_json(messages, max_tokens).await?;
    let drafts = parse_drafts(&content.to_string()).map_err(|e| {
        // The model returned JSON but our schema validation rejected it; the
        // raw text is the most useful diagnostic.
        crate::llm::LlmError::BadResponse {
            message: e,
            body: Some(content.to_string()),
        }
    })?;
    Ok(to_knowledge_payloads(&drafts, source, existing_categories))
}

/// Run the full extraction for one image (independent mode): validate the data
/// URL, call the LLM, validate the structured output, return drafts.
pub async fn extract_from_image(
    client: &OpenAiCompatClient,
    image_data_url: &str,
    source: &str,
    max_tokens: u32,
    existing_categories: &[(String, String)],
) -> Result<Vec<KnowledgePayload>, crate::llm::LlmError> {
    parse_data_url(image_data_url)?; // fail fast on unsupported/corrupt image
    run_extraction(
        client,
        vision_messages(image_data_url, existing_categories),
        source,
        max_tokens,
        existing_categories,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_items() {
        let content = r#"{"items":[
            {"question":"Q1","answer":"A1","domain":"后端","topic":"Redis","tags":["Redis"],"followUpQuestions":["追问1"]},
            {"question":"Q2","answer":"A2","domain":"数据库","topic":"MySQL","tags":[],"followUpQuestions":[]}
        ]}"#;
        let drafts = parse_drafts(content).unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].question, "Q1");
        assert_eq!(drafts[0].follow_up_questions, vec!["追问1".to_string()]);
        assert_eq!(drafts[1].topic, "MySQL");
    }

    #[test]
    fn empty_items_is_valid_zero_drafts() {
        let drafts = parse_drafts(r#"{"items":[]}"#).unwrap();
        assert!(drafts.is_empty());
    }

    #[test]
    fn blank_question_is_rejected() {
        let content = r#"{"items":[{"question":"  ","answer":"A"}]}"#;
        let err = parse_drafts(content).unwrap_err();
        assert!(err.contains("question"), "error mentions question: {err}");
    }

    #[test]
    fn blank_answer_is_rejected() {
        let content = r#"{"items":[{"question":"Q","answer":""}]}"#;
        let err = parse_drafts(content).unwrap_err();
        assert!(err.contains("answer"), "error mentions answer: {err}");
    }

    #[test]
    fn invalid_json_is_rejected() {
        assert!(parse_drafts("not json at all").is_err());
        assert!(parse_drafts(r#"{"items":"oops"}"#).is_err());
        assert!(parse_drafts(r#"{"foo":1}"#).is_err());
    }

    #[test]
    fn verbose_or_question_like_topic_is_rejected() {
        let verbose = r#"{"items":[{"question":"Q","answer":"A","topic":"这是一个超过二十四个字符而且明显不适合作为分类名称的主题"}]}"#;
        assert!(parse_drafts(verbose).unwrap_err().contains("主题不够简洁"));
        let question = r#"{"items":[{"question":"Q","answer":"A","topic":"Redis 为什么快？"}]}"#;
        assert!(parse_drafts(question).unwrap_err().contains("主题不够简洁"));
        let copied =
            r#"{"items":[{"question":"Redis为什么快","answer":"A","topic":"Redis为什么快"}]}"#;
        assert!(parse_drafts(copied).unwrap_err().contains("主题不够简洁"));
    }

    #[test]
    fn answer_markdown_allows_flat_structure_and_rejects_excess_depth() {
        let valid = "核心结论。\n\n## 原理\n\n- **内存访问**：减少 I/O\n  - 子项\n\n### 补充\n\n```rust\n# not-a-heading\n```";
        assert!(validate_answer_markdown(valid).is_ok());
        assert!(validate_answer_markdown("# 一级标题").is_err());
        assert!(validate_answer_markdown("#### 四级标题").is_err());
        assert!(validate_answer_markdown("- 一级\n  - 二级\n      - 三级").is_err());
        assert!(validate_answer_markdown("---").is_err());
        assert!(validate_answer_markdown("[文档](https://example.com)").is_err());
        assert!(validate_answer_markdown("| A | B |").is_err());
        assert!(validate_answer_markdown("<aside>注意</aside>").is_err());
        assert!(validate_answer_markdown("这是 *斜体* 内容").is_err());
        assert!(validate_answer_markdown("这是 _斜体_ 内容").is_err());
        assert!(validate_answer_markdown("这是 **粗体** 内容").is_ok());
    }

    #[test]
    fn single_messages_use_independent_prompt() {
        let categories = vec![("后端开发".to_string(), "Redis 持久化".to_string())];
        let msgs = vision_messages("data:image/png;base64,QQ==", &categories);
        let system = msgs[0]["content"].as_str().unwrap();
        assert_eq!(
            system, VISION_SYSTEM_PROMPT,
            "independent keeps its own prompt"
        );
        let instruction = msgs[1]["content"][0]["text"].as_str().unwrap();
        assert!(instruction.contains("- Redis 持久化"));
        assert!(!instruction.contains("后端开发 / Redis 持久化"));
        assert!(instruction.contains("只能从这些 Topic 中选择"));
        assert!(instruction.contains("topic 返回空字符串"));
        assert!(instruction.contains("禁止创建、改写或组合"));
        // A leftover phrase from the old multi-image mode should not leak in.
        assert!(!system.contains("连续内容"));
    }

    /// The new prompt must spell out the layout rules so the model knows when
    /// to break a long paragraph into a list, when to keep a short answer as
    /// a one-liner, and when an indented sub-explanation is appropriate. If
    /// someone later strips these requirements by accident, this test fires.
    #[test]
    fn system_prompt_specifies_layout_rules() {
        assert!(
            VISION_SYSTEM_PROMPT.contains("独立问题拆分"),
            "prompt must require splitting independent questions"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("标题与总体结构"),
            "prompt must spell out layout principles"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("是否把多个并列知识点挤成了长段落"),
            "prompt must forbid long-paragraph collapse"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("不要机械套模板"),
            "prompt must warn against templating every answer the same way"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("1~3 句回答"),
            "prompt must allow short answers to stay short"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("两个及以上并列知识点"),
            "prompt must require bullet/numbered list for parallel points"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("流程题：严格按照发生顺序"),
            "prompt must require ordered list for steps"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("比较题：按比较维度组织"),
            "prompt must cover comparison-style answers"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("## "),
            "prompt must allow ## subheadings"
        );
        assert!(
            VISION_SYSTEM_PROMPT.contains("凭空补充"),
            "prompt must forbid fabricating content not in the screenshot"
        );
        assert!(VISION_SYSTEM_PROMPT.contains("用户自己创建和维护的收藏夹"));
        assert!(VISION_SYSTEM_PROMPT.contains("禁止创建、改写、扩展或组合新的 Topic"));
        assert!(VISION_SYSTEM_PROMPT.contains("topic 返回空字符串"));
        assert!(VISION_SYSTEM_PROMPT.contains("domain 是旧数据兼容字段"));
        assert!(VISION_SYSTEM_PROMPT.contains("tags 继续由你自动生成 1~4 个"));
        assert!(VISION_SYSTEM_PROMPT.contains("`####` 及更深标题"));
        assert!(VISION_SYSTEM_PROMPT.contains("列表最多一层子列表"));
        assert!(
            VISION_SYSTEM_PROMPT.contains("结构选择优先级是：普通段落 → 同级列表 → `##` → `###`")
        );
        assert!(VISION_SYSTEM_PROMPT.contains("定义题：直接使用 1~3 句回答"));
        assert!(VISION_SYSTEM_PROMPT.contains("比较题：按比较维度组织"));
        assert!(VISION_SYSTEM_PROMPT.contains("流程题：严格按照发生顺序使用有序列表"));
        assert!(VISION_SYSTEM_PROMPT.contains("输出前内部自检"));
        assert!(VISION_SYSTEM_PROMPT.contains("水平分隔线、斜体"));
        assert!(VISION_SYSTEM_PROMPT.contains("blockquote 的正式语义是 Important / 重点提醒"));
        assert!(VISION_SYSTEM_PROMPT.contains("每个 answer 通常使用 0~1 个 blockquote"));
        assert!(VISION_SYSTEM_PROMPT.contains("复杂答案最多 2 个"));
        assert!(VISION_SYSTEM_PROMPT.contains("不要为了“好看”生成 blockquote"));
    }

    #[test]
    fn prompt_examples_cover_the_reading_style_guide() {
        // Redis-style cause question: one real section plus keyword-led peers,
        // rather than one H2 per fact.
        assert!(VISION_SYSTEM_PROMPT.contains("## 关键原因"));
        assert!(VISION_SYSTEM_PROMPT.contains("- **内存访问**"));
        assert!(VISION_SYSTEM_PROMPT.contains("不要把单个知识点各自做成 `##`"));

        // Comparison, flow and complex answers each have a bounded structure.
        assert!(VISION_SYSTEM_PROMPT.contains("**架构**"));
        assert!(VISION_SYSTEM_PROMPT.contains("不打乱流程顺序"));
        assert!(VISION_SYSTEM_PROMPT.contains("复杂答案最多 3 个主要结构层级"));
        assert!(VISION_SYSTEM_PROMPT.contains("`###` 和嵌套子列表尽量不要同时出现"));
    }

    /// The new prompt's user instruction must remind the model that answer is
    /// a real string with real newlines, not a JSON-escaped `\\n` literal.
    #[test]
    fn user_instruction_keeps_json_shape() {
        assert!(VISION_USER_INSTRUCTION.contains("\"items\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"question\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"answer\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"domain\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"topic\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"tags\""));
        assert!(VISION_USER_INSTRUCTION.contains("\"followUpQuestions\""));
        assert!(VISION_USER_INSTRUCTION.contains("{\"items\":[]}"));
    }

    /// The parser must keep the embedded newlines in `answer` intact so the
    /// layout the model produced (headings, lists, indented sub-lines) reaches
    /// SQLite byte-for-byte. Only outer whitespace is trimmed.
    #[test]
    fn parser_preserves_newlines_in_answer() {
        let content = r#"{"items":[{
            "question":"Redis 为什么快？",
            "answer":"核心结论。\n\n1. 基于内存\n   读写绕开磁盘。\n\n2. 单线程避免上下文切换\n   主流程不抢锁。\n\n## 参考\n\n- 内存访问\n- 非阻塞 IO",
            "domain":"后端",
            "topic":"Redis",
            "tags":["Redis"],
            "followUpQuestions":[]
        }]}"#;
        let drafts = parse_drafts(content).unwrap();
        assert_eq!(drafts.len(), 1);
        let a = &drafts[0].answer;
        assert!(
            a.contains("\n\n1. 基于内存"),
            "numbered list line preserved"
        );
        assert!(
            a.contains("   读写绕开磁盘。"),
            "indented sub-line preserved"
        );
        assert!(
            a.contains("\n\n## 参考\n"),
            "heading + blank lines preserved"
        );
        assert!(a.contains("\n- 内存访问"), "unordered list preserved");
        // Sanity: the answer is not collapsed to one line.
        assert!(a.lines().count() >= 8);
    }

    /// `to_knowledge_payloads` must NOT eat internal newlines either; it
    /// trims the outer whitespace only.
    #[test]
    fn payload_preserves_newlines_in_answer() {
        let drafts = vec![VisionDraft {
            question: "Q".to_string(),
            answer: " 首行\n\n1. A\n   解释\n\n2. B\n".to_string(),
            domain: "后端".to_string(),
            topic: "T".to_string(),
            tags: vec![],
            follow_up_questions: vec![],
        }];
        let categories = vec![("后端".to_string(), "T".to_string())];
        let items = to_knowledge_payloads(&drafts, "shot.png", &categories);
        let ans = &items[0].answer;
        assert!(ans.starts_with("首行"), "leading whitespace trimmed");
        assert!(!ans.ends_with('\n'), "trailing whitespace trimmed");
        assert!(ans.contains("\n\n1. A\n   解释\n\n2. B"));
    }

    #[test]
    fn to_payload_maps_fields_and_defaults() {
        let drafts = vec![VisionDraft {
            question: " Q ".to_string(),
            answer: " A ".to_string(),
            domain: " ".to_string(), // blank -> 未分类
            topic: "T".to_string(),
            tags: vec![" a ".to_string(), "".to_string()],
            follow_up_questions: vec!["F".to_string()],
        }];
        let categories = vec![("后端".to_string(), "T".to_string())];
        let items = to_knowledge_payloads(&drafts, "shot.png", &categories);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].question, "Q");
        assert_eq!(items[0].domain, "后端");
        assert_eq!(items[0].tags, vec!["a".to_string()]);
        assert!(items[0].id.starts_with("draft-"));
        assert!(items[0].source.contains("shot.png"));
    }

    #[test]
    fn blank_topic_is_valid_and_stays_uncategorized() {
        let drafts = parse_drafts(
            r#"{"items":[{"question":"Kubernetes 是什么？","answer":"容器编排系统。","domain":"云原生","topic":"","tags":["Kubernetes"],"followUpQuestions":[]}]}"#,
        )
        .unwrap();
        let items = to_knowledge_payloads(&drafts, "shot.png", &[]);
        assert_eq!(items[0].topic, "");
        assert_eq!(items[0].domain, "未分类");
    }

    #[test]
    fn ai_can_only_select_an_exact_existing_topic() {
        let categories = vec![
            ("后端".to_string(), "Redis".to_string()),
            ("数据库".to_string(), "MySQL".to_string()),
            ("AI".to_string(), "RAG".to_string()),
        ];
        let exact = vec![VisionDraft {
            question: "Redis 为什么快？".to_string(),
            answer: "主要数据位于内存。".to_string(),
            domain: "模型随意返回的领域".to_string(),
            topic: "Redis".to_string(),
            tags: vec!["内存".to_string(), "单线程".to_string()],
            follow_up_questions: vec![],
        }];
        let selected = to_knowledge_payloads(&exact, "redis.png", &categories);
        assert_eq!(selected[0].topic, "Redis");
        assert_eq!(selected[0].domain, "后端");
        assert_eq!(selected[0].tags, vec!["内存", "单线程"]);

        let invented = vec![VisionDraft {
            topic: "Redis 原理".to_string(),
            ..exact[0].clone()
        }];
        let uncategorized = to_knowledge_payloads(&invented, "redis.png", &categories);
        assert_eq!(uncategorized[0].topic, "");
        assert_eq!(uncategorized[0].domain, "未分类");
    }

    #[test]
    fn no_existing_topics_forces_every_draft_to_uncategorized() {
        let drafts = vec![VisionDraft {
            question: "Redis 为什么快？".to_string(),
            answer: "主要数据位于内存。".to_string(),
            domain: "后端".to_string(),
            topic: "Redis".to_string(),
            tags: vec!["内存".to_string()],
            follow_up_questions: vec![],
        }];
        let items = to_knowledge_payloads(&drafts, "shot.png", &[]);
        assert_eq!(items[0].topic, "");
        assert_eq!(items[0].domain, "未分类");
        assert_eq!(items[0].tags, vec!["内存"]);
    }
}
