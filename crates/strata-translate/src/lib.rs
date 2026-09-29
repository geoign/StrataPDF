//! Translation of reflowed documents into Japanese: which nodes to translate,
//! engines behind one trait, a per-document cache and a background job that
//! fills it starting from the reading position.

pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod providers;
#[cfg(feature = "llama")]
pub mod llama;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::RwLock;
use strata_core::reflow::{Node, ReflowDoc, Span};

/// One translatable node of a [`ReflowDoc`].
#[derive(Clone, Debug)]
pub struct Segment {
    /// Index into `ReflowDoc::nodes`.
    pub node: usize,
    pub text: String,
    /// Hash of `text`: the cache key, stable across re-layouts.
    pub hash: u64,
}

/// What to translate in a document.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub segments: Vec<Segment>,
    /// Beginning of the document (title, abstract), sent with every request so
    /// that terms are translated consistently.
    pub context: String,
    pub title: String,
}

impl Plan {
    pub fn nodes(&self) -> HashSet<usize> {
        self.segments.iter().map(|s| s.node).collect()
    }
}

/// Stable 64-bit FNV-1a (the std hasher is not guaranteed stable across releases).
pub fn text_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn plain(spans: &[Span]) -> String {
    let s: String = spans.iter().map(|s| s.text.as_str()).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_references(title: &str) -> bool {
    let t = title.trim().trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c.is_whitespace()).to_lowercase();
    matches!(t.as_str(), "references" | "reference" | "bibliography" | "literature cited" | "references cited" | "works cited" | "参考文献" | "引用文献" | "文献")
}

/// Mostly numbers and symbols (axis labels, coordinates, table fragments):
/// fewer than a third of the characters belong to words of two or more letters.
fn is_mostly_symbols(s: &str) -> bool {
    let total = s.chars().filter(|c| !c.is_whitespace()).count();
    let wordy: usize = s
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| w.chars().count() >= 2)
        .map(|w| w.chars().count())
        .sum();
    // CJK text has no spaces: every run of ideographs counts as a word.
    wordy * 3 < total
}

/// A reference-list entry that escaped the "References" heading (reflow may not
/// find it): "Author X (2010) Title. Journal 12(3):45–67". Both a year in
/// parentheses near the start and a volume:pages range are required, which
/// running text with citations rarely has.
fn looks_like_reference(s: &str) -> bool {
    let c: Vec<char> = s.chars().collect();
    let digits = |i: usize, n: usize| i + n <= c.len() && c[i..i + n].iter().all(|d| d.is_ascii_digit());
    let year_near_start = (0..c.len().min(90)).any(|i| c[i] == '(' && digits(i + 1, 4) && matches!(c.get(i + 5), Some(')') | Some('a'..='z')));
    let pages = (0..c.len()).any(|i| {
        c[i] == ':' && {
            let mut j = i + 1;
            while j < c.len() && c[j] == ' ' {
                j += 1;
            }
            let start = j;
            while j < c.len() && c[j].is_ascii_digit() {
                j += 1;
            }
            j > start && matches!(c.get(j), Some('–' | '-' | '—')) && c.get(j + 1).is_some_and(|d| d.is_ascii_digit())
        }
    });
    year_near_start && pages
}

/// Already Japanese: kana make up a noticeable share of the letters.
fn is_japanese(s: &str) -> bool {
    let (mut kana, mut letters) = (0usize, 0usize);
    for c in s.chars() {
        if c.is_alphabetic() {
            letters += 1;
            if ('\u{3040}'..='\u{30ff}').contains(&c) {
                kana += 1;
            }
        }
    }
    letters > 0 && kana * 10 >= letters
}

/// Nodes worth translating: headings, paragraphs, list items, footnotes and
/// captions, except the reference list and text that is already Japanese.
pub fn plan(doc: &ReflowDoc) -> Plan {
    let mut segments = Vec::new();
    // Inside a reference list: skip until a heading of the same or a higher level.
    let mut refs_level: Option<u8> = None;
    for (i, n) in doc.nodes.iter().enumerate() {
        let text = match n {
            Node::Heading { level, spans } => {
                let t = plain(spans);
                if refs_level.is_some_and(|l| *level <= l) {
                    refs_level = None;
                }
                if is_references(&t) {
                    refs_level = Some(*level);
                    continue;
                }
                t
            }
            _ if refs_level.is_some() => continue,
            Node::Paragraph { spans } | Node::ListItem { spans } | Node::Footnote { spans } => plain(spans),
            Node::Figure { caption, .. } | Node::Table { caption, .. } => plain(caption),
            _ => continue,
        };
        if text.chars().filter(|c| c.is_alphabetic()).count() < 2 || is_japanese(&text) || is_mostly_symbols(&text) || looks_like_reference(&text) {
            continue;
        }
        segments.push(Segment { node: i, hash: text_hash(&text), text });
    }
    // The first ~1500 characters of running text (usually the abstract).
    let mut context = String::new();
    for s in &segments {
        if context.len() > 1500 {
            break;
        }
        context.push_str(&s.text);
        context.push('\n');
    }
    Plan { segments, context, title: doc.title.clone() }
}

/// Paragraphs sent in one request.
pub struct Batch<'a> {
    pub title: &'a str,
    pub context: &'a str,
    /// (node index, source text)
    pub items: Vec<(usize, &'a str)>,
}

#[derive(Clone, Debug)]
pub enum Error {
    /// Too many requests. `daily`: the quota for today is used up.
    RateLimited { retry_after: Option<Duration>, daily: bool, message: String },
    /// Bad or missing API key.
    Auth(String),
    /// The answer was cut off: send fewer paragraphs at once.
    TooLong,
    /// No credit left, or billing not set up.
    Billing(String),
    Other(String),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::RateLimited { message, .. } => write!(f, "利用上限: {message}"),
            Error::Auth(m) => write!(f, "API キーを確認してください: {m}"),
            Error::Billing(m) => write!(f, "残高または支払い設定を確認してください: {m}"),
            Error::TooLong => write!(f, "出力が長すぎて途中で切れました"),
            Error::Other(m) => write!(f, "{m}"),
            Error::Cancelled => write!(f, "中止しました"),
        }
    }
}

pub trait Translator: Send + Sync {
    /// Engine, model and prompt version; part of the cache key.
    fn id(&self) -> String;
    /// Name shown to the user.
    fn label(&self) -> String;
    /// Source characters per request: (first request, later requests). The
    /// first is small so that something appears quickly.
    fn chunk_chars(&self) -> (usize, usize);
    /// Translate every item; the result may miss some ids.
    fn translate(&self, batch: &Batch, cancel: &AtomicBool) -> Result<Vec<(usize, String)>, Error>;
}

/// Shared instructions for LLM engines.
pub const SYSTEM_PROMPT: &str = "You translate academic papers into Japanese for Japanese researchers. \
The items are consecutive paragraphs, headings, list items, footnotes or figure captions of one document, in English or Chinese. \
Translate every item completely and faithfully into natural, fluent academic Japanese (である調); never summarize, merge or omit. \
Keep citation markers such as [12] or (Smith et al., 2020), numbers, units, chemical formulas, gene and species names, variable names and mathematical expressions unchanged. \
Use the standard Japanese term of the field for technical terms; keep proper nouns and acronyms in their original spelling unless a common Japanese form exists. \
Translate headings as short headings and captions as captions (keep labels such as \"Figure 3\" as \"図3\"). \
Never guess kanji for personal names: keep names of people as written in the source. \
Text was extracted from a PDF, so an item may start or end in the middle of a sentence: translate exactly what is given, without completing it or adding anything (such as citation numbers). \
Return every id exactly once.";

/// Bumped when the prompt changes, so that cached translations are redone.
pub const PROMPT_VERSION: u32 = 2;

/// The user message for LLM engines: title, context and the items as JSON.
pub fn user_prompt(batch: &Batch) -> String {
    let items: Vec<serde_json::Value> = batch.items.iter().map(|(id, t)| serde_json::json!({"id": id, "text": t})).collect();
    format!(
        "Document title: {}\n\nBeginning of the document (context for terminology only; do not translate it):\n{}\n\nTranslate these items into Japanese. Answer with JSON of the form {{\"items\": [{{\"id\": <id>, \"ja\": \"<translation>\"}}]}}:\n{}",
        batch.title,
        batch.context,
        serde_json::json!({ "items": items })
    )
}

/// JSON schema of the answer: `{"items": [{"id": 3, "ja": "…"}]}`.
pub fn answer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"items": {"type": "array", "items": {
            "type": "object",
            "properties": {"id": {"type": "integer"}, "ja": {"type": "string"}},
            "required": ["id", "ja"],
            "additionalProperties": false
        }}},
        "required": ["items"],
        "additionalProperties": false
    })
}

/// Read the answer, tolerating a Markdown code fence around the JSON (engines
/// without schema enforcement).
pub fn parse_answer(text: &str) -> Result<Vec<(usize, String)>, Error> {
    let t = text.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).map(|s| s.trim_end().trim_end_matches("```")).unwrap_or(t);
    let start = t.find('{').ok_or_else(|| Error::Other("応答に JSON がありません".into()))?;
    let end = t.rfind('}').ok_or_else(|| Error::Other("応答の JSON が途中で切れています".into()))?;
    let v: serde_json::Value = serde_json::from_str(&t[start..=end]).map_err(|e| Error::Other(format!("JSON を解釈できません: {e}")))?;
    Ok(v["items"]
        .as_array()
        .map(|a| a.iter().filter_map(|i| Some((i["id"].as_u64()? as usize, i["ja"].as_str()?.to_string()))).collect())
        .unwrap_or_default())
}

/// Seconds from a `Retry-After` header (delay-seconds form only).
pub(crate) fn retry_after(resp: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<f64>().ok()).map(Duration::from_secs_f64)
}

// ---------------------------------------------------------------- cache

/// Translations of one document by one engine, persisted as JSON lines.
pub struct Store {
    map: RwLock<HashMap<u64, String>>,
    file: Option<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Line {
    h: String,
    t: String,
}

fn slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

impl Store {
    pub fn open(doc_path: &Path, engine_id: &str) -> Store {
        let file = directories::ProjectDirs::from("", "", "StrataPDF")
            .zip(strata_core::ocr::cache_key(doc_path))
            .map(|(d, k)| d.data_local_dir().join("translate").join(k).join(format!("{}.jsonl", slug(engine_id))));
        let mut map = HashMap::new();
        if let Some(f) = &file
            && let Ok(s) = std::fs::read_to_string(f)
        {
            for l in s.lines() {
                if let Ok(Line { h, t }) = serde_json::from_str::<Line>(l)
                    && let Ok(h) = u64::from_str_radix(&h, 16)
                {
                    map.insert(h, t);
                }
            }
        }
        Store { map: RwLock::new(map), file }
    }

    pub fn get(&self, hash: u64) -> Option<String> {
        self.map.read().get(&hash).cloned()
    }

    pub fn len(&self) -> usize {
        self.map.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn insert(&self, hash: u64, text: String) {
        if let Some(f) = &self.file
            && let Some(dir) = f.parent()
            && std::fs::create_dir_all(dir).is_ok()
            && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(f)
            && let Ok(line) = serde_json::to_string(&Line { h: format!("{hash:016x}"), t: text.clone() })
        {
            let _ = writeln!(file, "{line}");
        }
        self.map.write().insert(hash, text);
    }
}

// ---------------------------------------------------------------- job

#[derive(Clone, Debug)]
pub enum Event {
    /// (node index, translation)
    Translated(Vec<(usize, String)>),
    Progress { done: usize, total: usize },
    /// Waiting for the rate limit before retrying.
    Waiting { seconds: u64, message: String },
    /// Nodes the engine could not translate.
    Failed(Vec<usize>),
    /// The job stopped before finishing.
    Stopped(Error),
    Done,
}

pub struct Job {
    pub rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Translate everything not yet in `store`, starting at the first segment at or
/// after node `from` and wrapping around. Cached translations are sent first.
pub fn start(plan: Arc<Plan>, store: Arc<Store>, engine: Arc<dyn Translator>, from: usize, wake: Arc<dyn Fn() + Send + Sync>) -> Job {
    let (tx, rx) = unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    std::thread::Builder::new()
        .name("strata-translate".into())
        .spawn(move || {
            run(&plan, &store, engine.as_ref(), from, &c, &tx, wake.as_ref());
            wake();
        })
        .ok();
    Job { rx, cancel }
}

fn sleep_cancellable(d: Duration, cancel: &AtomicBool) -> bool {
    let end = std::time::Instant::now() + d;
    while std::time::Instant::now() < end {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn run(plan: &Plan, store: &Store, engine: &dyn Translator, from: usize, cancel: &AtomicBool, tx: &Sender<Event>, wake: &(dyn Fn() + Send + Sync)) {
    let send = |e: Event| {
        let _ = tx.send(e);
        wake();
    };
    let total = plan.segments.len();
    let cached: Vec<(usize, String)> = plan.segments.iter().filter_map(|s| store.get(s.hash).map(|t| (s.node, t))).collect();
    let mut done = cached.len();
    if !cached.is_empty() {
        send(Event::Translated(cached));
    }
    send(Event::Progress { done, total });
    // Reading order from the current position, then the part before it.
    let start = plan.segments.iter().position(|s| s.node >= from).unwrap_or(0);
    let mut queue: Vec<&Segment> = plan.segments[start..].iter().chain(&plan.segments[..start]).filter(|s| store.get(s.hash).is_none()).collect();
    let (first, rest) = engine.chunk_chars();
    let mut limit = first;
    let mut failed = Vec::new();
    let mut retried: HashSet<usize> = HashSet::new();
    while !queue.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        // A repeated text may have been translated in an earlier request.
        let mut hit = Vec::new();
        queue.retain(|s| match store.get(s.hash) {
            Some(t) => {
                hit.push((s.node, t));
                false
            }
            None => true,
        });
        if !hit.is_empty() {
            done += hit.len();
            send(Event::Translated(hit));
            if queue.is_empty() {
                break;
            }
        }
        // Take paragraphs up to the character budget (at least one).
        let mut n = 0;
        let mut chars = 0;
        while n < queue.len() && (n == 0 || chars + queue[n].text.len() <= limit) && n < 80 {
            chars += queue[n].text.len();
            n += 1;
        }
        let chunk: Vec<&Segment> = queue[..n].to_vec();
        let batch = Batch { title: &plan.title, context: &plan.context, items: chunk.iter().map(|s| (s.node, s.text.as_str())).collect() };
        let mut attempts = 0;
        let result = loop {
            attempts += 1;
            match engine.translate(&batch, cancel) {
                Err(Error::RateLimited { retry_after, daily: false, message }) if attempts <= 6 => {
                    let wait = retry_after.unwrap_or(Duration::from_secs(20)).max(Duration::from_secs(2));
                    send(Event::Waiting { seconds: wait.as_secs(), message });
                    if !sleep_cancellable(wait, cancel) {
                        return;
                    }
                }
                Err(Error::Other(m)) if attempts <= 2 => {
                    log::warn!("translate: {m}; retrying");
                    if !sleep_cancellable(Duration::from_secs(3 * attempts as u64), cancel) {
                        return;
                    }
                }
                r => break r,
            }
        };
        match result {
            Ok(out) => {
                let ids: HashMap<usize, &Segment> = chunk.iter().map(|s| (s.node, *s)).collect();
                for (id, t) in out {
                    let t = t.trim().to_string();
                    if let Some(s) = ids.get(&id)
                        && !t.is_empty()
                        && store.get(s.hash).is_none()
                    {
                        store.insert(s.hash, t);
                    }
                }
                // Per segment, so that repeated texts (same hash) are all reported.
                let got: Vec<(usize, String)> = chunk.iter().filter_map(|s| store.get(s.hash).map(|t| (s.node, t))).collect();
                done += got.len();
                queue.drain(..n);
                // Missing ones are retried once, one request later.
                for s in chunk {
                    if store.get(s.hash).is_none() {
                        if retried.insert(s.node) {
                            queue.push(s);
                        } else {
                            failed.push(s.node);
                        }
                    }
                }
                if !got.is_empty() {
                    send(Event::Translated(got));
                }
                send(Event::Progress { done, total });
                limit = rest;
            }
            Err(Error::TooLong) if n > 1 => limit = (chars / 2).max(1),
            Err(Error::TooLong) => {
                failed.push(chunk[0].node);
                queue.remove(0);
            }
            Err(Error::Cancelled) => return,
            Err(e) => {
                send(Event::Stopped(e));
                return;
            }
        }
    }
    if !failed.is_empty() {
        send(Event::Failed(failed));
    }
    send(Event::Done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use strata_core::reflow::Style;

    fn sp(t: &str) -> Vec<Span> {
        vec![Span { text: t.into(), style: Style::default(), link: None }]
    }

    #[test]
    fn plan_skips_references_and_japanese() {
        let doc = ReflowDoc {
            nodes: vec![
                Node::Heading { level: 1, spans: sp("Introduction") },
                Node::Paragraph { spans: sp("Some   text\nhere.") },
                Node::Paragraph { spans: sp("すでに日本語の段落である。") },
                Node::Heading { level: 1, spans: sp("References") },
                Node::ListItem { spans: sp("[1] A. Author. Title. 2020.") },
                Node::Heading { level: 1, spans: sp("Appendix A") },
                Node::Paragraph { spans: sp("More text.") },
            ],
            ..Default::default()
        };
        let p = plan(&doc);
        let nodes: Vec<usize> = p.segments.iter().map(|s| s.node).collect();
        assert_eq!(nodes, vec![0, 1, 5, 6]);
        assert_eq!(p.segments[1].text, "Some text here.");
    }

    #[test]
    fn reference_entries() {
        assert!(looks_like_reference("Giannetti B (1996) Volcanology of trachytic and associated basaltic pyroclastic deposits at Roccamonfina volcano, Italy. J Volcanol Geotherm Res 71(2–4):229–248"));
        assert!(!looks_like_reference("Tephra from the 2010 eruption (Biass et al. 2011) was sampled at 40 sites; see Table 2 for details: 12–15 cm thick."));
    }

    #[test]
    fn symbols_are_not_prose() {
        assert!(is_mostly_symbols("130°E 135°E 140°E 145°E 138°E 139°E"));
        assert!(is_mostly_symbols("0 5 10 km 20 40"));
        assert!(!is_mostly_symbols("The Off Joetsu seep (37°32.00'N, 137°55.00'E, ~1000 m deep) is characterised by methane."));
        assert!(!is_mostly_symbols("冷湧水域的群落由新种螺类主导。"));
    }
}
