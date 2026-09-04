// Shared Chinese tokenizer (jieba-rs) driving SQLite FTS5 retrieval.
//
// We do NOT use an FTS5 custom C tokenizer. Instead every indexed field and
// every query is tokenized here in Rust (jieba-rs, pure Rust, dict embedded,
// cross-platform), producing space-joined tokens stored in an FTS5 table with
// the built-in `unicode61` tokenizer. Because document and query go through the
// SAME tokenizer, matching is token-based ("词") instead of the old N-character
// substring ("trigram") behavior.
//
//   Knowledge (original) -> knowledge_items  (raw text always preserved here)
//                          + knowledge_fts   (jieba space-joined tokens, derived)
//
//   Query -> jieba tokens -> stopword/low-value filter -> FTS5 MATCH
//
// Retrieval is recall-first with a simple two-tier strategy over the CORE
// tokens only: AND first, fall back to OR. Low-value question words (为什么 /
// 介绍 / 讲讲 ...) are given zero weight so they never drown out the real terms
// (BERT / Redis / 持久化 / 索引 ...).

use std::sync::OnceLock;

static JIEBA: OnceLock<jieba_rs::Jieba> = OnceLock::new();

/// A single shared Jieba instance (dictionary loaded once, thread-safe).
fn jieba() -> &'static jieba_rs::Jieba {
    JIEBA.get_or_init(jieba_rs::Jieba::new)
}

/// Tokens with no retrieval value (dropped everywhere). Deliberately scoped to
/// function words / openers ONLY — not topical single Han characters.
///
/// IMPORTANT: we do NOT blanket-drop single Han characters. Meaningful single
/// Han (锁 栈 堆 树 快 慢 读 写 库 表 …) must be kept, or queries like
/// "Redis 为什么快" would degrade to just "Redis". Only the explicit function
/// words below are dropped.
const STOPWORDS: &[&str] = &[
    "你", "我", "他", "她", "它", "你们", "我们", "她们", "它们", "人家", "自己", "大家", "别人",
    "的", "了", "着", "吗", "呢", "吧", "啊", "哦", "呀", "嘛", "呵",
    "是", "在", "有", "和", "与", "及", "跟", "对", "从", "向", "把", "被", "让", "叫", "给",
    "请", "下", "这", "那", "此", "个", "中", "里", "上", "之", "以",
    "都", "也", "还", "又", "很", "就", "才", "别", "而", "但", "且", "或",
    "想", "要", "帮", "帮忙", "想要", "需要", "请求", "希望",
    "一个", "一些", "这个", "那个", "这些", "那些", "可以", "能够",
];

/// Topically-empty *question scaffolding* words. They carry little retrieval
/// value and MUST NOT be treated as equal to core terms. They are never put in
/// the FTS query (so they cannot pollute AND/OR), but they are also not hard
/// stopwords (kept out for rule simplicity rather than dictionary NLP).
const LOW_VALUE: &[&str] = &[
    "为什么", "为啥", "为何", "是什么", "什么叫", "什么是", "怎么", "如何", "怎么样", "怎样",
    "什么", "怎么办", "哪种", "哪些",
    "介绍", "介绍下", "介绍一下", "讲讲", "说说", "聊聊", "讲解", "解释", "说明",
    "一下", "请问", "多少", "个", "吗", "呢",
];

/// A character that carries token value: ASCII alphanumeric or a Han ideograph.
fn meaningful(c: char) -> bool {
    c.is_ascii_alphanumeric() || ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// True when `t` is pure punctuation (no letters/digits/CJK), so it is neither
/// stored as an index token nor used as a query token.
fn is_pure_punct(t: &str) -> bool {
    !t.chars().any(meaningful)
}

fn collect(words: Vec<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words {
        if w.is_empty() || is_pure_punct(w) {
            continue;
        }
        let s = w.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// All jieba tokens (deduped, punctuation + single-Han dropped). Used for
/// indexing a field — never read back; `knowledge_items` keeps the raw text.
pub fn doc_tokens(text: &str) -> Vec<String> {
    let words: Vec<&str> = jieba().cut(text, false).iter().map(|t| t.word).collect();
    collect(words)
}

/// Space-joined token stream stored in `knowledge_fts` (the derived index).
pub fn doc_space(text: &str) -> String {
    doc_tokens(text).join(" ")
}

/// CORE query tokens: jieba tokens with stopwords and low-value words removed.
pub fn core_tokens(query: &str) -> Vec<String> {
    let words: Vec<&str> = jieba()
        .cut(query, false)
        .iter()
        .map(|t| t.word.trim())
        .filter(|w| !STOPWORDS.contains(w) && !LOW_VALUE.contains(w))
        .collect();
    collect(words)
}

fn quoted<'a>(tokens: &'a [String], op: &str) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(op)
}

/// Tier-1 FTS query: all core tokens ANDed. Precise; often under-recalls.
pub fn and_query(query: &str) -> String {
    quoted(&core_tokens(query), " AND ")
}

/// Tier-2 FTS query: core tokens ORed. Recall-first fallback.
pub fn or_query(query: &str) -> String {
    quoted(&core_tokens(query), " OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_keeps_tech_and_drops_question_words() {
        assert_eq!(core_tokens("你介绍下BERT"), vec!["BERT"]);
        assert_eq!(core_tokens("讲讲Redis"), vec!["Redis"]);
        assert_eq!(core_tokens("BERT 是什么"), vec!["BERT"]);
        assert_eq!(core_tokens("BERT是什么"), vec!["BERT"]);
        assert_eq!(core_tokens("MySQL索引"), vec!["MySQL", "索引"]);
        assert_eq!(core_tokens("TCP三次握手"), vec!["TCP", "三次", "握手"]);
        // meaningful single Han are KEPT (快/锁), only function words dropped;
        // low-value question words (为什么/怎么) are excluded from retrieval.
        assert_eq!(core_tokens("Redis 为什么快"), vec!["Redis", "快"]);
        assert_eq!(core_tokens("Redis 锁怎么实现"), vec!["Redis", "锁", "实现"]);
        // single-Han function words explicitly dropped
        assert!(core_tokens("Redis 是 我的 下 那里").iter().all(|t| !["是", "下", "的"].contains(&t.as_str())));
        // completely unrelated terms stay as core (so a real topic keeps value)
        assert_eq!(core_tokens("量子纠缠实验"), vec!["量子", "纠缠", "实验"]);
    }

    // jieba itself may split a tech token (B+Tree -> ["B","Tree"], Node.js ->
    // ["Node","js"], TCP/IP -> ["TCP","IP"]). What matters for retrieval is that
    // the SAME surface form tokenizes identically as a doc and as a query, so
    // its query core tokens are a sub-set of its own doc tokens (self-match).
    #[test]
    fn tech_tokens_are_self_consistent() {
        for t in ["B+Tree", "C++", "C#", "Node.js", "TCP/IP", "MySQL", "gRPC", "RAG"] {
            let doc = doc_tokens(t);
            let core = core_tokens(t);
            assert!(!doc.is_empty(), "{t} indexes at least one token");
            assert!(
                core.iter().all(|c| doc.contains(c)),
                "{t}: query core {:?} must be a subset of its own doc {:?}",
                core,
                doc
            );
        }
    }
}