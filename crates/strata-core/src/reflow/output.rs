//! Markdown and HTML serialisation of a [`ReflowDoc`].

use super::{Node, ReflowDoc, ReflowImage, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Auto,
    Light,
    Dark,
}

pub struct HtmlOptions<'a> {
    pub theme: Theme,
    /// Show small page-number markers in the margin.
    pub page_markers: bool,
    /// How an image is referenced (URL, relative path or data URI).
    pub image_src: &'a dyn Fn(&ReflowImage) -> String,
    /// Extra CSS appended after the built-in style sheet.
    pub extra_css: &'a str,
    /// Side-by-side translation layout: these nodes get a row with an empty
    /// translation cell (`#tr-<node index>`) filled in later by script.
    pub bilingual: Option<&'a std::collections::HashSet<usize>>,
}

fn esc_html(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

fn esc_md(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '[' | ']' | '`' | '<' | '>' | '#' | '|') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

fn strip_marker(spans: &[Span]) -> Vec<Span> {
    let mut v = spans.to_vec();
    if let Some(first) = v.first_mut() {
        let t = first.text.trim_start();
        let t = t.strip_prefix(['•', '·', '▪', '●', '◦', '‣', '–', '・']).unwrap_or(t);
        first.text = t.trim_start().to_string();
    }
    v
}

pub fn spans_html(spans: &[Span]) -> String {
    spans_html_in(spans, false)
}

/// `vertical`: half-width letters, digits and symbols marked up for vertical writing.
fn spans_html_in(spans: &[Span], vertical: bool) -> String {
    let mut o = String::new();
    for s in spans {
        if s.style.math
            && let Some(m) = latex_to_mathml_inline(&s.text)
        {
            o.push_str(&m);
            continue;
        }
        let mut t = if vertical { super::tate::vertical_html(&s.text) } else { esc_html(&s.text) };
        if s.style.mono {
            t = format!("<code>{t}</code>");
        }
        if s.style.italic {
            t = format!("<em>{t}</em>");
        }
        if s.style.bold {
            t = format!("<strong>{t}</strong>");
        }
        if s.style.sup {
            t = format!("<sup>{t}</sup>");
        } else if s.style.sub {
            t = format!("<sub>{t}</sub>");
        }
        if let Some(l) = &s.link {
            t = format!("<a href=\"{}\">{t}</a>", esc_html(l));
        }
        o.push_str(&t);
    }
    o
}

pub fn spans_md(spans: &[Span]) -> String {
    let mut o = String::new();
    for s in spans {
        let (lead, core, trail) = {
            let t = s.text.as_str();
            let core = t.trim();
            let lead = &t[..t.len() - t.trim_start().len()];
            let trail = &t[t.trim_end().len()..];
            (lead, core, trail)
        };
        if core.is_empty() {
            o.push_str(&s.text);
            continue;
        }
        let mut t = esc_md(core);
        if s.style.math {
            o.push_str(&format!("{lead}${core}${trail}"));
            continue;
        }
        if s.style.mono {
            t = format!("`{}`", core.replace('`', "'"));
        }
        if s.style.italic {
            t = format!("*{t}*");
        }
        if s.style.bold {
            t = format!("**{t}**");
        }
        if s.style.sup {
            t = format!("<sup>{t}</sup>");
        } else if s.style.sub {
            t = format!("<sub>{t}</sub>");
        }
        if let Some(l) = &s.link {
            t = format!("[{t}]({})", l.replace(' ', "%20").replace(')', "%29"));
        }
        o.push_str(lead);
        o.push_str(&t);
        o.push_str(trail);
    }
    // Adjacent emphasis markers of separate spans would read as `****`.
    o.replace("****", "")
}

fn plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect::<String>().trim().to_string()
}

pub fn to_markdown(doc: &ReflowDoc, image_path: &dyn Fn(&ReflowImage) -> String) -> String {
    let mut o = String::new();
    let mut in_list = false;
    for n in &doc.nodes {
        let is_item = matches!(n, Node::ListItem { .. });
        if in_list && !is_item {
            o.push('\n');
        }
        in_list = is_item;
        match n {
            Node::PageStart { page } => o.push_str(&format!("<!-- page {} -->\n\n", page + 1)),
            Node::Heading { level, spans } => o.push_str(&format!("{} {}\n\n", "#".repeat(*level as usize), spans_md(spans).trim())),
            Node::Paragraph { spans } => o.push_str(&format!("{}\n\n", spans_md(spans).trim())),
            Node::ListItem { spans } => o.push_str(&format!("- {}\n", spans_md(&strip_marker(spans)).trim())),
            Node::Figure { image, caption } => {
                let img = &doc.images[*image];
                let alt = plain(caption).chars().take(80).collect::<String>().replace(['[', ']'], "");
                o.push_str(&format!("![{alt}]({})\n\n", image_path(img)));
                if !caption.is_empty() {
                    o.push_str(&format!("*{}*\n\n", spans_md(caption).trim()));
                }
            }
            Node::Table { image, caption, rows } => {
                let img = &doc.images[*image];
                o.push_str(&format!("**{}**\n\n", spans_md(caption).trim()));
                o.push_str(&format!("![table]({})\n\n", image_path(img)));
                if !rows.is_empty() {
                    o.push_str("<details><summary>表のテキスト</summary>\n\n```text\n");
                    for r in rows {
                        o.push_str(r);
                        o.push('\n');
                    }
                    o.push_str("```\n\n</details>\n\n");
                }
            }
            Node::Footnote { spans } => o.push_str(&format!("<small>{}</small>\n\n", spans_md(spans).trim())),
            Node::Html { md, .. } => o.push_str(&format!("{}\n\n", md.trim_end())),
            Node::PageImage { image, reason } => {
                o.push_str(&format!("> [!NOTE]\n> {reason}。OCR するまで画像で表示しています。\n\n![page]({})\n\n", image_path(&doc.images[*image])));
            }
            Node::Formula { image, text, latex, number } => match latex {
                Some(l) => match number {
                    Some(n) => o.push_str(&format!("$$\n{l} \\tag{{{n}}}\n$$\n\n")),
                    None => o.push_str(&format!("$$\n{l}\n$$\n\n")),
                },
                None => o.push_str(&format!("![{}]({})\n\n", text.replace(['[', ']', '\n'], " ").trim(), image_path(&doc.images[*image]))),
            },
        }
    }
    o
}

const CSS: &str = r#"
:root { --bg:#fbfaf7; --fg:#1d1d1f; --muted:#6b6b70; --rule:#dddad2; --link:#1f5fbf; --card:#ffffff; --pm:#b5b1a6; --hover:rgba(0,0,0,.035); }
@media (prefers-color-scheme: dark) { :root:not(.light) { --bg:#1b1c1f; --fg:#e3e1dc; --muted:#9a988f; --rule:#34353a; --link:#8ab4f8; --card:#26272b; --pm:#5d5e63; --hover:rgba(255,255,255,.04); } }
:root.dark { --bg:#1b1c1f; --fg:#e3e1dc; --muted:#9a988f; --rule:#34353a; --link:#8ab4f8; --card:#26272b; --pm:#5d5e63; --hover:rgba(255,255,255,.04); }
/* Fonts; the viewer replaces these with the user's choice. */
:root { --font-body: "Charis SIL", "Cambria", "Georgia", "StrataPDF Noto Serif JP", "Noto Serif JP", "Noto Serif CJK JP", "Source Han Serif JP", "BIZ UDPMincho", "Yu Mincho", serif; --font-ja: "StrataPDF Noto Serif JP", "Noto Serif JP", "Noto Serif CJK JP", "Source Han Serif JP", "BIZ UDPMincho", "Yu Mincho", serif; --font-head: "Segoe UI", "StrataPDF Noto Sans JP", "Noto Sans JP", "Noto Sans CJK JP", "Source Han Sans JP", "BIZ UDPGothic", "Yu Gothic", sans-serif; }
html { background: var(--bg); color: var(--fg); }
body { margin: 0; font-family: var(--font-body); font-size: 17px; line-height: 1.7; }
main { max-width: 46em; margin: 0 auto; padding: 2.5em 1.5em 6em; position: relative; }
h1, h2, h3, h4, h5, h6 { font-family: var(--font-head); line-height: 1.35; margin: 1.6em 0 .6em; }
h1 { font-size: 1.7em; } h2 { font-size: 1.35em; } h3 { font-size: 1.15em; } h4, h5, h6 { font-size: 1em; }
p { margin: 0 0 .9em; text-align: justify; hyphens: auto; }
/* Long URLs and identifiers wrap instead of widening the page. */
p, li, figcaption, h1, h2, h3, h4, h5, h6 { overflow-wrap: anywhere; }
a { color: var(--link); text-decoration: none; } a:hover { text-decoration: underline; }
sup, sub { font-size: .72em; line-height: 0; }
figure { margin: 1.6em 0; text-align: center; }
figure img { max-width: 100%; height: auto; background: #fff; border-radius: 4px; box-shadow: 0 0 0 1px var(--rule); }
figcaption { color: var(--muted); font-size: .9em; text-align: left; margin-top: .5em; }
figure.table figcaption { margin: 0 0 .5em; }
details { text-align: left; font-size: .85em; color: var(--muted); margin-top: .4em; }
details pre { white-space: pre-wrap; background: var(--card); padding: .8em; border-radius: 4px; }
.formula { text-align: center; margin: 1.2em 0; position: relative; overflow-x: auto; }
.formula math { font-size: 1.1em; }
.eqno { position: absolute; right: 0; top: 50%; transform: translateY(-50%); color: var(--muted); }
p.fn { font-size: .82em; color: var(--muted); border-top: 1px solid var(--rule); padding-top: .4em; }
.note { font: 13px "Segoe UI", "Yu Gothic UI", sans-serif; color: var(--muted); border-left: 3px solid #e0a030; padding: .3em .7em; margin-bottom: .6em; text-align: left; }
.formula img { max-width: 100%; }
:root.dark .formula img { filter: invert(.88) hue-rotate(180deg); }
@media (prefers-color-scheme: dark) { :root:not(.light) .formula img { filter: invert(.88) hue-rotate(180deg); } }
ul { padding-left: 1.4em; } li { margin: .2em 0; }
.pm { position: absolute; left: -2.2em; font: 11px "Segoe UI", sans-serif; color: var(--pm); cursor: pointer; user-select: none; }
.pm:hover { color: var(--link); }
.pm-anchor { display: block; height: 0; }
/* Vertical writing (Japanese books). */
body.vertical { overflow-x: auto; overflow-y: hidden; }
body.vertical main { writing-mode: vertical-rl; max-width: none; height: calc(100vh - 5em); margin: 0; padding: 2.5em 3em; font-family: var(--font-ja); line-height: 1.9; }
body.vertical p { text-align: justify; margin: 0; }
/* Blocks of Markdown files. */
.md pre { white-space: pre-wrap; background: var(--card); padding: .8em 1em; border-radius: 4px; box-shadow: 0 0 0 1px var(--rule); line-height: 1.5; }
code { font-family: Consolas, "BIZ UDGothic", monospace; font-size: .9em; }
.md blockquote { margin: 0 0 .9em; padding: .1em 1em; border-left: 3px solid var(--rule); color: var(--muted); }
.md table { border-collapse: collapse; margin: 0 0 .9em; }
.md th, .md td { border: 1px solid var(--rule); padding: .3em .7em; }
.md img { max-width: 100%; height: auto; }
.md hr { border: 0; border-top: 1px solid var(--rule); margin: 1.6em 0; }
html:lang(ja) .md p { text-indent: 0; }
/* Half-width characters by kind (see tate.rs): one em box, or upright and stacked. */
body.vertical .tcy { text-combine-upright: all; }
body.vertical .up { text-orientation: upright; }
html:lang(ja) p { text-indent: 1em; }
html:lang(ja) p.fn { text-indent: 0; }
body.vertical .pm { position: static; display: inline-block; writing-mode: horizontal-tb; margin: 0 .3em; }
body.vertical figure img { max-height: 80vh; }
/* Side-by-side translation. */
main.bi-main { max-width: 96em; }
.bi { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); column-gap: 2.4em; align-items: start; border-radius: 4px; }
.bi > .full { grid-column: 1 / -1; }
.bi:hover { background: var(--hover); }
.bi .tr { font-family: var(--font-ja); line-height: 1.85; }
/* Justifying Japanese with long Latin words stretches the gaps between characters. */
.bi .tr p { text-indent: 1em; text-align: left; } .bi .tr p.fn, .bi .tr p.capt, .bi .tr p.li { text-indent: 0; }
.bi .tr.pending::before { content: "…"; color: var(--muted); }
.bi .tr.failed::before { content: "（訳せませんでした）"; color: var(--muted); font-size: .85em; }
.bi .src p.li { padding-left: 1.2em; text-indent: -1.2em; }
.bi p.capt { color: var(--muted); font-size: .9em; }
"#;

const SCRIPT: &str = r#"
document.addEventListener('click', e => {
  const pm = e.target.closest('.pm');
  if (pm && window.ipc) { window.ipc.postMessage('page:' + pm.dataset.page); return; }
  const a = e.target.closest('a');
  if (!a) return;
  const h = a.getAttribute('href') || '';
  if (h.startsWith('#page-')) {
    const t = document.getElementById(h.slice(1));
    if (t) { e.preventDefault(); t.scrollIntoView({behavior: 'smooth', block: 'start', inline: 'start'}); }
  } else if (window.ipc && /^https?:|^mailto:/.test(h)) {
    e.preventDefault(); window.ipc.postMessage('open:' + h);
  }
});
if (document.body.classList.contains('vertical')) {
  // Vertical text scrolls right-to-left: map the wheel to horizontal movement.
  window.addEventListener('wheel', e => { if (!e.ctrlKey && Math.abs(e.deltaY) > Math.abs(e.deltaX)) { window.scrollBy(-e.deltaY, 0); e.preventDefault(); } }, {passive: false});
}
window.strataSetTheme = t => { const r = document.documentElement; r.classList.remove('light', 'dark'); if (t) r.classList.add(t); };
window.strataGotoPos = (p, y) => {
  let best = null;
  for (const el of document.querySelectorAll('[data-p="' + p + '"]')) { best = el; if (+el.dataset.y >= y - 4) break; }
  if (best) best.scrollIntoView({block: 'start', inline: 'start'}); else window.strataGotoPage(p);
};
window.strataGotoPage = p => { const t = document.getElementById('page-' + p); if (t) t.scrollIntoView({block: 'start', inline: 'start'}); };
"#;

pub fn to_html(doc: &ReflowDoc, o: &HtmlOptions) -> String {
    let lang = if doc.nodes.iter().any(|n| match n {
        Node::Paragraph { spans } => spans.iter().any(|s| s.text.chars().any(|c| matches!(c as u32, 0x3040..=0x30FF))),
        _ => false,
    }) {
        "ja"
    } else {
        "en"
    };
    let root_class = match o.theme {
        Theme::Auto => "",
        Theme::Light => "light",
        Theme::Dark => "dark",
    };
    let mut h = String::new();
    // The translation column is horizontal, so vertical originals are shown horizontally too.
    let vertical = doc.vertical && o.bilingual.is_none();
    h.push_str(&format!(
        "<!doctype html>\n<html lang=\"{lang}\" class=\"{root_class}\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<meta name=\"generator\" content=\"StrataPDF\">\n<title>{}</title>\n<style>{CSS}{}</style>\n</head>\n<body class=\"{}\">\n<main{}>\n",
        esc_html(&doc.title),
        o.extra_css,
        if vertical { "vertical" } else { "" },
        if o.bilingual.is_some() { " class=\"bi-main\"" } else { "" }
    ));
    let mut in_list = false;
    let spans_html = |s: &[Span]| spans_html_in(s, vertical);
    for (ni, n) in doc.nodes.iter().enumerate() {
        let a = doc.anchors.get(ni).map(|(p, y)| format!(" data-p=\"{}\" data-y=\"{:.0}\"", p + 1, y)).unwrap_or_default();
        if let Some(set) = o.bilingual
            && set.contains(&ni)
            && let Some(row) = bilingual_row(doc, ni, n, &a, o)
        {
            if in_list {
                h.push_str("</ul>\n");
                in_list = false;
            }
            h.push_str(&row);
            continue;
        }
        let is_item = matches!(n, Node::ListItem { .. });
        if in_list && !is_item {
            h.push_str("</ul>\n");
        }
        if !in_list && is_item {
            h.push_str("<ul>\n");
        }
        in_list = is_item;
        match n {
            Node::PageStart { page } => {
                let p = page + 1;
                if o.page_markers {
                    h.push_str(&format!("<span class=\"pm\" id=\"page-{p}\" data-page=\"{p}\" title=\"PDF の {p} ページを開く\">{p}</span>\n"));
                } else {
                    h.push_str(&format!("<span class=\"pm-anchor\" id=\"page-{p}\"></span>\n"));
                }
            }
            Node::Heading { level, spans } => h.push_str(&format!("<h{level}{a}>{}</h{level}>\n", spans_html(spans).trim())),
            Node::Paragraph { spans } => h.push_str(&format!("<p{a}>{}</p>\n", spans_html(spans).trim())),
            Node::ListItem { spans } => h.push_str(&format!("<li{a}>{}</li>\n", spans_html(&strip_marker(spans)).trim())),
            Node::Figure { image, caption } => {
                let img = &doc.images[*image];
                h.push_str(&format!("<figure{a}><img src=\"{}\" width=\"{}\" alt=\"\" loading=\"lazy\">", esc_html(&(o.image_src)(img)), img.width / 2));
                if !caption.is_empty() {
                    h.push_str(&format!("<figcaption>{}</figcaption>", spans_html(caption)));
                }
                h.push_str("</figure>\n");
            }
            Node::Table { image, caption, rows } => {
                let img = &doc.images[*image];
                h.push_str(&format!("<figure class=\"table\"><figcaption>{}</figcaption><img src=\"{}\" width=\"{}\" alt=\"\" loading=\"lazy\">", spans_html(caption), esc_html(&(o.image_src)(img)), img.width / 2));
                if !rows.is_empty() {
                    h.push_str("<details><summary>表のテキスト</summary><pre>");
                    h.push_str(&esc_html(&rows.join("\n")));
                    h.push_str("</pre></details>");
                }
                h.push_str("</figure>\n");
            }
            Node::Footnote { spans } => h.push_str(&format!("<p class=\"fn\">{}</p>\n", spans_html(spans).trim())),
            Node::Html { html, .. } => h.push_str(&format!("<div class=\"md\"{a}>{}</div>\n", with_images(html, doc, o))),
            Node::PageImage { image, reason } => {
                let img = &doc.images[*image];
                h.push_str(&format!(
                    "<figure class=\"pageimg\"><div class=\"note\">{}。OCR するまで画像で表示しています。</div><img src=\"{}\" width=\"{}\" alt=\"\" loading=\"lazy\"></figure>\n",
                    esc_html(reason),
                    esc_html(&(o.image_src)(img)),
                    img.width / 2
                ));
            }
            Node::Formula { image, text, latex, number } => {
                let img = &doc.images[*image];
                let num = number.as_ref().map(|n| format!("<span class=\"eqno\">({})</span>", esc_html(n))).unwrap_or_default();
                match latex.as_ref().and_then(|l| latex_to_mathml(l).map(|m| (l, m))) {
                    Some((l, m)) => h.push_str(&format!(
                        "<div class=\"formula\" data-latex=\"{}\" title=\"LaTeX: {}\">{m}{num}</div>\n",
                        esc_html(l),
                        esc_html(l)
                    )),
                    None => h.push_str(&format!("<div class=\"formula\"><img src=\"{}\" width=\"{}\" alt=\"{}\"></div>\n", esc_html(&(o.image_src)(img)), img.width / 3, esc_html(text))),
                }
            }
        }
    }
    if in_list {
        h.push_str("</ul>\n");
    }
    h.push_str("</main>\n<script>");
    h.push_str(SCRIPT);
    h.push_str("</script>\n</body>\n</html>\n");
    h
}

/// `strata-img:N` in a Markdown block replaced by the image's address.
fn with_images(html: &str, doc: &ReflowDoc, o: &HtmlOptions) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find("strata-img:") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + "strata-img:".len()..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        match tail[..digits].parse::<usize>().ok().and_then(|n| doc.images.get(n)) {
            Some(img) => out.push_str(&esc_html(&(o.image_src)(img))),
            None => out.push_str(&rest[i..i + "strata-img:".len() + digits]),
        }
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

/// A side-by-side row: the original on the left, an empty translation cell on the right.
fn bilingual_row(doc: &ReflowDoc, ni: usize, n: &Node, a: &str, o: &HtmlOptions) -> Option<String> {
    let (full, src, tag) = match n {
        Node::Heading { level, spans } => (String::new(), format!("<h{level}>{}</h{level}>", spans_html(spans).trim()), format!("h{level}")),
        Node::Paragraph { spans } => (String::new(), format!("<p>{}</p>", spans_html(spans).trim()), "p".into()),
        Node::ListItem { spans } => (String::new(), format!("<p class=\"li\">• {}</p>", spans_html(&strip_marker(spans)).trim()), "p.li".into()),
        Node::Footnote { spans } => (String::new(), format!("<p class=\"fn\">{}</p>", spans_html(spans).trim()), "p.fn".into()),
        Node::Figure { image, caption } | Node::Table { image, caption, .. } if !caption.is_empty() => {
            let img = &doc.images[*image];
            let full = format!("<figure class=\"full\"><img src=\"{}\" width=\"{}\" alt=\"\" loading=\"lazy\"></figure>", esc_html(&(o.image_src)(img)), img.width / 2);
            (full, format!("<p class=\"capt\">{}</p>", spans_html(caption).trim()), "p.capt".into())
        }
        _ => return None,
    };
    Some(format!("<div class=\"bi\"{a}>{full}<div class=\"src\">{src}</div><div class=\"tr pending\" id=\"tr-{ni}\" data-tag=\"{tag}\"></div></div>\n"))
}

/// LaTeX to MathML Core (rendered natively by Chromium/WebView2).
pub fn latex_to_mathml(latex: &str) -> Option<String> {
    mathml(latex, true)
}

/// LaTeX to MathML set in the line of text (Markdown `$...$`).
pub fn latex_to_mathml_inline(latex: &str) -> Option<String> {
    mathml(latex, false)
}

fn mathml(latex: &str, block: bool) -> Option<String> {
    use math_core::{LatexToMathML, MathCoreConfig, MathDisplay};
    thread_local! {
        static CONV: Option<LatexToMathML> = LatexToMathML::new(MathCoreConfig::default()).ok();
    }
    let display = if block { MathDisplay::Block } else { MathDisplay::Inline };
    CONV.with(|c| c.as_ref()?.convert_with_local_state(latex, display).ok().map(|r| r.mathml))
}
