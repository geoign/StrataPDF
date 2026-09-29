//! reflow_breaks <file>...: paragraphs of the reflowed text that end in the
//! middle of a sentence, grouped by what follows them. A regression measure for
//! paragraph joining across columns, pages and figures. With `-v`, examples.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use strata_core::Document;
use strata_core::reflow::{Node, ReflowEvent, ReflowOptions, Span};

fn text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "Fig. 4 …", "Figure 2: …", "Table 1 …": captions float and are not body text.
fn is_caption(t: &str) -> bool {
    let l = t.to_lowercase();
    ["fig.", "fig ", "figure", "table"].iter().any(|p| l.starts_with(p) && l[p.len()..].trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// A reference-list entry: a year and a volume:pages range, a quoted title or "pp".
fn is_reference(t: &str) -> bool {
    let b = t.as_bytes();
    let year = b.windows(4).any(|w| (w.starts_with(b"19") || w.starts_with(b"20")) && w[2].is_ascii_digit() && w[3].is_ascii_digit());
    let c: Vec<char> = t.chars().collect();
    let pages = (0..c.len()).any(|i| {
        c[i] == ':' && {
            let mut j = i + 1;
            while j < c.len() && c[j] == ' ' {
                j += 1;
            }
            let s = j;
            while j < c.len() && c[j].is_ascii_digit() {
                j += 1;
            }
            j > s && matches!(c.get(j), Some('–' | '-'))
        }
    });
    let tail: String = t.chars().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect::<String>().to_lowercase();
    (year && (pages || t.contains('“') || t.contains(" pp"))) || tail.contains("doi") || tail.contains("http")
}

fn ends_sentence(t: &str) -> bool {
    t.trim_end().ends_with(['.', '!', '?', ':', '。', '．', '！', '？', '」', '』', '）', ')'])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "-v");
    let mut total: BTreeMap<&'static str, usize> = BTreeMap::new();
    for path in args.iter().filter(|a| *a != "-v") {
        let doc = Document::open(path.as_ref(), None, Arc::new(|| {})).unwrap();
        let rx = doc.reflow(ReflowOptions::default(), Arc::new(AtomicBool::new(false)));
        let d = loop {
            match rx.recv().unwrap() {
                ReflowEvent::Done(d) => break d,
                ReflowEvent::Error(e) => panic!("{e}"),
                _ => {}
            }
        };
        // `DUMP_PAGE=5`: print the nodes of that page (1-based).
        if let Some(dp) = std::env::var("DUMP_PAGE").ok().and_then(|v| v.parse::<u32>().ok()) {
            let mut pg = 0;
            for n in &d.nodes {
                let (kind, t) = match n {
                    Node::PageStart { page } => {
                        pg = page + 1;
                        continue;
                    }
                    Node::Heading { spans, .. } => ("H", text(spans)),
                    Node::Paragraph { spans } => ("P", text(spans)),
                    Node::ListItem { spans } => ("L", text(spans)),
                    Node::Footnote { spans } => ("N", text(spans)),
                    Node::Figure { caption, .. } => ("FIG", text(caption)),
                    Node::Table { caption, .. } => ("TAB", text(caption)),
                    Node::Formula { text: t, .. } => ("EQ", t.clone()),
                    Node::PageImage { .. } => ("IMG", String::new()),
                };
                if pg == dp {
                    let c: Vec<char> = t.chars().collect();
                    let head: String = c.iter().take(50).collect();
                    let tail: String = c.iter().skip(c.len().saturating_sub(40)).collect();
                    println!("  {kind:<3} {head} … {tail}");
                }
            }
        }
        // `HEADINGS=1`: list headings, and paragraphs and nodes in total (to spot lost text).
        if std::env::var("HEADINGS").is_ok() {
            let mut chars = 0usize;
            for n in &d.nodes {
                match n {
                    Node::Heading { spans, .. } => println!("H {}", text(spans)),
                    Node::Paragraph { spans } | Node::ListItem { spans } | Node::Footnote { spans } => chars += text(spans).chars().count(),
                    _ => {}
                }
            }
            println!("TEXT CHARS {chars}");
        }
        // `TEXT_DUMP=1`: the full text of every text node, one per line (to diff runs).
        if std::env::var("TEXT_DUMP").is_ok() {
            for n in &d.nodes {
                if let Node::Heading { spans, .. } | Node::Paragraph { spans } | Node::ListItem { spans } | Node::Footnote { spans } = n {
                    println!("{}", text(spans));
                }
                if let Node::Figure { caption, .. } | Node::Table { caption, .. } = n {
                    println!("CAPTION {}", text(caption));
                }
            }
        }
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut page = 0;
        let mut in_refs = false;
        // Front matter (affiliations, dates, keywords) has no sentences: start at the abstract.
        let mut started = false;
        for (i, n) in d.nodes.iter().enumerate() {
            match n {
                Node::PageStart { page: p } => page = *p,
                Node::Heading { spans, .. } => {
                    let t = text(spans).to_lowercase();
                    started |= t.contains("abstract") || t.contains("introduction") || t.contains("summary");
                    in_refs = t.ends_with("references") || t.ends_with("bibliography") || t == "literature cited";
                }
                Node::Paragraph { spans } if text(spans).to_lowercase() == "references" => in_refs = true,
                Node::Paragraph { spans } if !started => {
                    let t = text(spans).to_lowercase();
                    started = t.starts_with("abstract") || t.starts_with("summary");
                }
                Node::Paragraph { spans } if !in_refs => {
                    let t = text(spans);
                    if t.chars().count() < 40 || ends_sentence(&t) || is_caption(&t) || is_reference(&t) {
                        continue;
                    }
                    let next = d.nodes[i + 1..].iter().find(|n| match n {
                        Node::PageStart { .. } | Node::Figure { .. } | Node::Table { .. } | Node::Footnote { .. } => false,
                        Node::Paragraph { spans } => !is_caption(&text(spans)),
                        _ => true,
                    });
                    let (kind, head) = match next {
                        None => ("end of document", String::new()),
                        Some(Node::Formula { .. }) => ("display formula (fine)", String::new()),
                        Some(Node::Heading { spans, .. }) => ("heading", text(spans)),
                        Some(Node::ListItem { .. }) if t.trim_end().ends_with([',', ';']) => ("list (fine)", String::new()),
                        Some(Node::ListItem { spans }) => ("list item", text(spans)),
                        Some(Node::Paragraph { spans }) => {
                            let h = text(spans);
                            let c = h.chars().next().unwrap_or(' ');
                            let k = if c.is_lowercase() {
                                "paragraph, lowercase"
                            } else if c.is_uppercase() {
                                "paragraph, uppercase"
                            } else if c == '[' {
                                "paragraph, ["
                            } else {
                                "paragraph, other"
                            };
                            (k, h)
                        }
                        Some(_) => ("other", String::new()),
                    };
                    *counts.entry(kind).or_default() += 1;
                    if verbose && kind != "display formula (fine)" {
                        let tail: String = t.chars().rev().take(55).collect::<Vec<_>>().into_iter().rev().collect();
                        let head: String = head.chars().take(60).collect();
                        println!("  p{:<3} [{kind}] …{tail} ‖ {head}", page + 1);
                    }
                }
                _ => {}
            }
        }
        let problems: usize = counts.iter().filter(|(k, _)| !k.contains("fine")).map(|(_, v)| v).sum();
        println!("{}: {problems} cut paragraphs {counts:?}", std::path::Path::new(path).file_name().unwrap().to_string_lossy());
        for (k, v) in counts {
            *total.entry(k).or_default() += v;
        }
    }
    let problems: usize = total.iter().filter(|(k, _)| !k.contains("fine")).map(|(_, v)| v).sum();
    println!("TOTAL: {problems} cut paragraphs {total:?}");
}
