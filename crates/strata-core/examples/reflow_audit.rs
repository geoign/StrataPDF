//! reflow_audit <file> [--dump <out.txt>]: quality measures of the reflowed
//! text of one document, as one JSON line on stdout, for corpus-wide evaluation
//! (tools/corpus). With `--dump`, the reflowed text: one node per line, tagged
//! with its kind, and `=== p N` at each page start.
//!
//! Measures compare the reflow output with the raw text of the pages: words of
//! the raw text missing from the output (lost), words the output has more often
//! (duplicated), paragraphs cut in the middle of a sentence, hyphens left
//! inside joined words, glued words, undecodable characters, running heads
//! leaking into paragraphs, and the shape of headings and reference entries.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use serde_json::json;
use strata_core::Document;
use strata_core::reflow::{Node, ReflowEvent, ReflowOptions, Span};
use strata_core::rich::{RichBlock, RichPage, reflow_flags};

fn text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// Comparison tokens: runs of letters and digits (hyphens and apostrophes
/// inside a word are dropped, so "species-poor" and "speciespoor" agree),
/// lowercased; each CJK character is a token of its own.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let v: Vec<char> = s.chars().collect();
    for (i, &c) in v.iter().enumerate() {
        if is_cjk(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(c.to_string());
        } else if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else if matches!(c, '-' | '\u{2010}' | '\u{2011}' | '\u{00AD}' | '\'' | '’') && !cur.is_empty() && v.get(i + 1).is_some_and(|n| n.is_alphanumeric()) {
            // inside a word
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // Numbers are compared poorly (superscripts, ranges, spacing): words and CJK characters only.
    out.retain(|w| w.chars().any(is_cjk) || w.chars().filter(|c| c.is_alphabetic()).count() >= 2);
    out
}

fn digits_key(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).map(|c| if c.is_ascii_digit() { '#' } else { c }).collect()
}

fn is_caption(t: &str) -> bool {
    let l = t.to_lowercase();
    ["fig.", "fig ", "figure", "table", "plate"].iter().any(|p| l.starts_with(p) && l[p.len()..].trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()))
        || t.starts_with('図')
        || t.starts_with('表')
}

fn ends_sentence(t: &str) -> bool {
    t.trim_end().ends_with(['.', '!', '?', ':', '。', '．', '！', '？', '」', '』', '）', ')'])
}

/// A reference-list entry: a year and a volume:pages range, a quoted title,
/// "pp", a DOI or a URL.
fn is_reference(t: &str) -> bool {
    let b = t.as_bytes();
    let year = b.windows(4).any(|w| (w.starts_with(b"19") || w.starts_with(b"20")) && w[2].is_ascii_digit() && w[3].is_ascii_digit());
    let tail: String = t.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect::<String>().to_lowercase();
    let pages = t.contains(|c| c == '–') || t.contains(':');
    (year && (pages || t.contains('“') || t.contains(" pp"))) || tail.contains("doi") || tail.contains("http")
}

/// "Smith, J.", "Smith J.A.", "Smith, John": a surname then initials, the way reference entries start.
fn author_starts(t: &str) -> usize {
    // Count "Surname, X." patterns: an uppercase word followed by ", " and an uppercase initial with a period.
    let v: Vec<char> = t.chars().collect();
    let mut n = 0;
    let mut i = 0;
    while i + 4 < v.len() {
        if v[i] == ',' && v[i + 1] == ' ' && v[i + 2].is_uppercase() && v[i + 3] == '.' && i >= 2 && v[i - 1].is_lowercase() {
            n += 1;
        }
        i += 1;
    }
    n
}

/// Years in author-year form ("2004.", "(2004)", "2004a,"): how many entries a paragraph holds.
fn year_marks(t: &str) -> usize {
    let v: Vec<char> = t.chars().collect();
    let mut n = 0;
    for i in 0..v.len().saturating_sub(4) {
        let four = v[i..i + 4].iter().all(|c| c.is_ascii_digit());
        if !four || (i > 0 && v[i - 1].is_ascii_digit()) || v.get(i + 4).is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let y: u32 = v[i..i + 4].iter().collect::<String>().parse().unwrap_or(0);
        if !(1800..=2030).contains(&y) {
            continue;
        }
        let before = if i > 0 { v[i - 1] } else { ' ' };
        let mut j = i + 4;
        if v.get(j).is_some_and(|c| c.is_ascii_lowercase()) && !v.get(j + 1).is_some_and(|c| c.is_alphabetic()) {
            j += 1;
        }
        let after = v.get(j).copied().unwrap_or(' ');
        if (before == '(' && after == ')') || ((before == ' ' || before == ',') && matches!(after, '.' | ',' | ')')) {
            n += 1;
        }
    }
    n
}

struct Raw {
    /// Per page: lines as (text, size, words>=5, rotated, in margin band).
    lines: Vec<(u32, String, f32)>,
    body: f32,
    repeated: Vec<String>,
}

fn raw_text(path: &str) -> Result<Raw, String> {
    let doc = mupdf::Document::open(path).map_err(|e| e.to_string())?;
    let n = doc.page_count().map_err(|e| e.to_string())?;
    let mut pages: Vec<RichPage> = Vec::new();
    for p in 0..n {
        let Ok(page) = doc.load_page(p) else {
            pages.push(RichPage::default());
            continue;
        };
        let b = page.bounds().map_err(|e| e.to_string())?;
        match page.to_text_page(reflow_flags()) {
            Ok(tp) => pages.push(RichPage::from_text_page(&tp, b.width(), b.height())),
            Err(_) => pages.push(RichPage::default()),
        }
    }
    let mut hist: HashMap<i32, usize> = HashMap::new();
    let mut margin: HashMap<String, usize> = HashMap::new();
    let mut lines = Vec::new();
    for (pi, p) in pages.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        for b in &p.blocks {
            let RichBlock::Text { lines: ls, .. } = b else { continue };
            // Lines of a block, hyphenated line ends joined to the next line.
            let mut acc = String::new();
            let mut acc_size = 0.0f32;
            for (k, l) in ls.iter().enumerate() {
                if !l.vertical && l.dir[1].abs() > 0.5 {
                    continue;
                }
                let t = l.text();
                let mut sizes: Vec<f32> = l.chars.iter().map(|c| c.size).collect();
                sizes.sort_by(f32::total_cmp);
                let size = sizes.get(sizes.len() / 2).copied().unwrap_or(0.0);
                for c in &l.chars {
                    *hist.entry((c.size * 2.0).round() as i32).or_default() += 1;
                }
                if (l.bbox.y1 < p.height * 0.15 || l.bbox.y0 > p.height * 0.85) && seen.insert(digits_key(&t)) {
                    *margin.entry(digits_key(&t)).or_default() += 1;
                }
                acc.push_str(&t);
                acc_size = acc_size.max(size);
                let hyphen = t.trim_end().ends_with(['-', '\u{2010}', '\u{2011}', '\u{00AD}']) && k + 1 < ls.len();
                if hyphen {
                    while acc.ends_with(char::is_whitespace) {
                        acc.pop();
                    }
                    acc.pop();
                } else {
                    lines.push((pi as u32, std::mem::take(&mut acc), acc_size));
                    acc_size = 0.0;
                }
            }
            if !acc.is_empty() {
                lines.push((pi as u32, acc, acc_size));
            }
        }
    }
    let body = hist.into_iter().max_by_key(|e| e.1).map(|e| e.0 as f32 / 2.0).unwrap_or(10.0);
    let threshold = ((pages.len() as f32) * 0.25).ceil().max(3.0) as usize;
    let repeated = margin.into_iter().filter(|(_, n)| *n >= threshold).map(|(k, _)| k).collect();
    Ok(Raw { lines, body, repeated })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[0];
    let dump = args.iter().position(|a| a == "--dump").and_then(|i| args.get(i + 1));
    let t0 = Instant::now();
    let doc = match Document::open(path.as_ref(), None, Arc::new(|| {})) {
        Ok(d) => d,
        Err(e) => {
            println!("{}", json!({"path": path, "error": format!("open: {e}")}));
            return;
        }
    };
    let n_pages = doc.page_count();
    let rx = doc.reflow(ReflowOptions::default(), Arc::new(AtomicBool::new(false)));
    let d = loop {
        match rx.recv() {
            Ok(ReflowEvent::Done(d)) => break d,
            Ok(ReflowEvent::Error(e)) => {
                println!("{}", json!({"path": path, "pages": n_pages, "error": format!("reflow: {e}")}));
                return;
            }
            Ok(_) => {}
            Err(_) => {
                println!("{}", json!({"path": path, "pages": n_pages, "error": "reflow thread died"}));
                return;
            }
        }
    };
    let ms = t0.elapsed().as_millis();

    // Output text by node, with the page it starts on.
    let mut kinds: HashMap<&'static str, usize> = HashMap::new();
    let mut out_lines: Vec<(u32, &'static str, String)> = Vec::new();
    let mut page = 0u32;
    for n in &d.nodes {
        let (k, t) = match n {
            Node::PageStart { page: p } => {
                page = *p;
                continue;
            }
            Node::Heading { level, spans } => (["H1", "H2", "H3", "H4", "H5", "H6"][(*level as usize).clamp(1, 6) - 1], text(spans)),
            Node::Paragraph { spans } => ("P", text(spans)),
            Node::ListItem { spans } => ("L", text(spans)),
            Node::Footnote { spans } => ("N", text(spans)),
            Node::Figure { caption, .. } => ("FIG", text(caption)),
            Node::Table { caption, rows, .. } => ("TAB", format!("{} ‖ {}", text(caption), rows.join(" | "))),
            Node::Formula { text: t, .. } => ("EQ", t.split_whitespace().collect::<Vec<_>>().join(" ")),
            Node::PageImage { reason, .. } => ("IMG", reason.clone()),
        };
        *kinds.entry(k).or_default() += 1;
        out_lines.push((page, k, t));
    }
    if let Some(dump) = dump {
        let mut s = String::new();
        let mut last = u32::MAX;
        for (p, k, t) in &out_lines {
            if *p != last {
                s.push_str(&format!("=== p{}\n", p + 1));
                last = *p;
            }
            s.push_str(&format!("{k}\t{t}\n"));
        }
        let _ = std::fs::write(dump, s);
    }

    let raw = match raw_text(path) {
        Ok(r) => r,
        Err(e) => {
            println!("{}", json!({"path": path, "pages": n_pages, "ms": ms, "error": format!("raw: {e}")}));
            return;
        }
    };
    let page_images = kinds.get("IMG").copied().unwrap_or(0);
    let img_pages: std::collections::HashSet<u32> = out_lines.iter().filter(|l| l.1 == "IMG").map(|l| l.0).collect();

    // Lost and duplicated words (raw text of image pages is not expected in the output).
    let mut have: HashMap<String, i64> = HashMap::new();
    let mut out_tokens = 0usize;
    for (_, k, t) in &out_lines {
        if *k == "IMG" {
            continue;
        }
        for w in tokens(t) {
            *have.entry(w).or_default() += 1;
            out_tokens += 1;
        }
    }
    let (mut raw_tokens, mut lost, mut body_tokens, mut body_lost) = (0usize, 0usize, 0usize, 0usize);
    let mut lost_lines: Vec<(u32, f32, String)> = Vec::new();
    for (p, t, size) in &raw.lines {
        if img_pages.contains(p) {
            continue;
        }
        let ws = tokens(t);
        // Repeated running heads and lone page numbers are dropped on purpose.
        let key = digits_key(t);
        if raw.repeated.contains(&key) || (ws.len() <= 2 && ws.iter().all(|w| w.chars().all(|c| c.is_ascii_digit()))) {
            for w in &ws {
                if let Some(v) = have.get_mut(w) {
                    *v -= 1;
                }
            }
            continue;
        }
        let bodyish = (size - raw.body).abs() <= raw.body * 0.1 && ws.len() >= 6;
        let mut l = 0;
        for w in &ws {
            let v = have.entry(w.clone()).or_default();
            if *v > 0 {
                *v -= 1;
            } else {
                l += 1;
            }
        }
        raw_tokens += ws.len();
        lost += l;
        if bodyish {
            body_tokens += ws.len();
            body_lost += l;
            if l * 2 >= ws.len() {
                lost_lines.push((*p, l as f32 / ws.len() as f32, t.chars().take(100).collect()));
            }
        }
    }
    let dup: i64 = have.values().filter(|v| **v > 0).sum();

    // Paragraph-level checks.
    let mut cut_lower = 0;
    let mut cut_upper = 0;
    let mut cut_other = 0;
    let mut cut_examples: Vec<String> = Vec::new();
    let mut hyphen_left = 0;
    let mut long_tokens = 0;
    let mut bad_chars = 0usize;
    let mut out_chars = 0usize;
    let mut leak = 0;
    let mut leak_examples: Vec<String> = Vec::new();
    let mut caption_para = 0;
    let mut para_lens: Vec<usize> = Vec::new();
    let mut heading_long = 0;
    let mut heading_sentence = 0;
    let mut formula_prose = 0;
    let (mut ref_paras, mut ref_multi, mut ref_frag) = (0, 0, 0);
    let mut in_refs = false;
    let mut started = false;
    let repeated_keys: Vec<String> = raw.repeated.iter().filter(|k| k.chars().count() >= 20 || (k.contains('#') && k.chars().count() >= 12)).cloned().collect();
    for (i, (p, k, t)) in out_lines.iter().enumerate() {
        out_chars += t.chars().filter(|c| !c.is_whitespace()).count();
        bad_chars += t.chars().filter(|&c| c == '\u{FFFD}' || (c as u32) < 0x20 && c != '\t' || (0xE000..=0xF8FF).contains(&(c as u32))).count();
        if k.starts_with('H') {
            let lt = t.to_lowercase();
            if lt.contains("abstract") || lt.contains("introduction") || lt.contains("summary") {
                started = true;
            }
            in_refs = lt.ends_with("references") || lt.ends_with("bibliography") || lt == "literature cited" || lt.ends_with("references cited") || lt == "参考文献" || lt == "文献" || lt == "引用文献";
            if t.chars().count() > 150 {
                heading_long += 1;
            }
            if t.ends_with('.') && t.chars().count() > 60 {
                heading_sentence += 1;
            }
            continue;
        }
        if *k == "EQ" && t.split_whitespace().filter(|w| w.len() >= 4 && w.chars().all(|c| c.is_ascii_lowercase())).count() >= 6 {
            formula_prose += 1;
        }
        if *k != "P" && *k != "L" && *k != "N" {
            continue;
        }
        let tl = t.to_lowercase();
        if *k == "P" && (tl == "references" || tl == "bibliography" || tl == "参考文献" || tl == "引用文献") {
            in_refs = true;
            continue;
        }
        if *k == "P" {
            para_lens.push(t.chars().count());
            if is_caption(t) {
                caption_para += 1;
            }
        }
        // Glued words and leftover hyphens.
        for w in t.split_whitespace() {
            let letters = w.chars().filter(|c| c.is_alphabetic()).count();
            if letters >= 25 && !w.contains("http") && !w.contains('/') && !w.contains('@') && w.chars().filter(|c| !c.is_alphabetic()).count() <= 2 && !w.chars().any(is_cjk) {
                long_tokens += 1;
            }
        }
        let cs: Vec<char> = t.chars().collect();
        for j in 1..cs.len().saturating_sub(2) {
            if cs[j] == '-' && cs[j - 1].is_lowercase() && cs[j + 1] == ' ' && cs[j + 2].is_lowercase() {
                let next: String = cs[j + 2..].iter().take_while(|c| c.is_alphabetic()).collect();
                if !matches!(next.as_str(), "and" | "or" | "to" | "und" | "et" | "nor" | "as" | "versus" | "vs") {
                    hyphen_left += 1;
                }
            }
        }
        // Running heads inside paragraphs (keys long enough not to occur by chance;
        // journal names are cited in reference lists).
        if !in_refs && !repeated_keys.is_empty() {
            let pk: String = digits_key(t);
            for rk in &repeated_keys {
                if pk.contains(rk.as_str()) && pk.len() > rk.len() + 10 {
                    leak += 1;
                    if leak_examples.len() < 3 {
                        leak_examples.push(format!("p{} {}", p + 1, rk.chars().take(60).collect::<String>()));
                    }
                }
            }
        }
        if in_refs {
            ref_paras += 1;
            let years = year_marks(t);
            let authors = author_starts(t);
            if years >= 2 && t.chars().count() > 250 {
                ref_multi += 1;
            }
            if (t.chars().count() < 40 && years == 0) || t.starts_with(|c: char| c.is_lowercase()) {
                ref_frag += 1;
            }
            let _ = authors;
            continue;
        }
        if !started && *p >= 1 {
            started = true;
        }
        if *k != "P" || !started {
            continue;
        }
        if t.chars().count() < 40 || ends_sentence(t) || is_caption(t) || is_reference(t) {
            continue;
        }
        // What follows, skipping floats.
        let next = out_lines[i + 1..].iter().find(|(_, k2, t2)| match *k2 {
            "FIG" | "TAB" | "N" | "IMG" => false,
            "P" => !is_caption(t2),
            _ => true,
        });
        match next {
            Some((_, "P", t2)) => {
                let c = t2.chars().next().unwrap_or(' ');
                let tag = if c.is_lowercase() || matches!(c, ',' | ';' | ')') {
                    cut_lower += 1;
                    "L"
                } else if c.is_uppercase() {
                    cut_upper += 1;
                    "U"
                } else {
                    cut_other += 1;
                    "O"
                };
                if cut_examples.len() < 8 {
                    let tail: String = t.chars().rev().take(45).collect::<Vec<_>>().into_iter().rev().collect();
                    let head: String = t2.chars().take(45).collect();
                    cut_examples.push(format!("p{} {tag} …{tail} ‖ {head}", p + 1));
                }
            }
            Some((_, "EQ", _)) => {}
            Some((_, k2, _)) if k2.starts_with('H') => cut_other += 1,
            _ => {}
        }
    }
    para_lens.sort_unstable();
    let med = para_lens.get(para_lens.len() / 2).copied().unwrap_or(0);
    let short_paras = para_lens.iter().filter(|&&l| l < 40).count();
    let huge_paras = para_lens.iter().filter(|&&l| l > 6000).count();
    lost_lines.sort_by(|a, b| b.1.total_cmp(&a.1));
    let lost_examples: Vec<String> = lost_lines.iter().take(5).map(|(p, f, t)| format!("p{} {:.0}% {t}", p + 1, f * 100.0)).collect();
    let headings: usize = kinds.iter().filter(|(k, _)| k.starts_with('H')).map(|(_, v)| v).sum();
    println!(
        "{}",
        json!({
            "path": path,
            "pages": n_pages,
            "ms": ms,
            "vertical": d.vertical,
            "title": d.title,
            "body_size": raw.body,
            "nodes": kinds,
            "page_images": page_images,
            "headings": headings,
            "raw_tokens": raw_tokens,
            "out_tokens": out_tokens,
            "lost": lost,
            "body_tokens": body_tokens,
            "body_lost": body_lost,
            "dup": dup,
            "lost_examples": lost_examples,
            "cut_lower": cut_lower,
            "cut_upper": cut_upper,
            "cut_other": cut_other,
            "cut_examples": cut_examples,
            "hyphen_left": hyphen_left,
            "long_tokens": long_tokens,
            "bad_chars": bad_chars,
            "out_chars": out_chars,
            "leak": leak,
            "leak_examples": leak_examples,
            "caption_para": caption_para,
            "paras": para_lens.len(),
            "para_median": med,
            "short_paras": short_paras,
            "huge_paras": huge_paras,
            "heading_long": heading_long,
            "heading_sentence": heading_sentence,
            "formula_prose": formula_prose,
            "ref_paras": ref_paras,
            "ref_multi": ref_multi,
            "ref_frag": ref_frag,
        })
    );
}
