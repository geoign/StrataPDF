//! Headless conversion (`StrataPDF.exe --headless convert ...`, normally reached
//! through the console program StrataPDF-cli.exe): the text view's Markdown /
//! HTML export without a window.
//!
//! The conversion is the same as the app's "Markdown で保存" / "HTML で保存":
//! the reflow of `strata-core` (paragraphs joined across columns and pages,
//! running heads dropped), with optional OCR and formula recognition.
//! Messages are ASCII English so that a console or a pipe in any code page
//! shows them; file names keep their own characters.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use base64::Engine as _;
use strata_core::Document;
use strata_core::ocr::{Device, OcrEngine, OcrEvent, OcrScope, models};
use strata_core::reflow::output::{self, HtmlOptions, Theme};
use strata_core::reflow::{Node, ReflowDoc, ReflowEvent, ReflowImage, ReflowOptions};
use strata_ocr::formula::FormulaEngine;

const USAGE: &str = "\
StrataPDF-cli - convert PDF and other documents to Markdown and/or HTML without a window.

USAGE:
    StrataPDF-cli convert <INPUT>... [OPTIONS]
    StrataPDF-cli --help | --version

INPUT:
    PDF, EPUB, XPS/OXPS, CBZ, FB2, Markdown (.md) or text (.txt). Several inputs
    are converted one after another. Wildcards (* and ?) in the file name part are
    expanded here, so they work in PowerShell and cmd as well.

OUTPUT (per input <stem>, in --out-dir or next to the input):
    <stem>.md         Markdown. Images go to <stem>_files\\ and are linked relatively.
    <stem>.html       Single-file HTML with images embedded as data URIs
                      (or linked from <stem>_files\\ with --html-images files).
    Existing files are overwritten. The paths written are printed on stdout, one per
    line; progress and warnings go to stderr.

OPTIONS:
    -t, --to <FORMATS>        md, html, or both: md,html (also \"both\" or \"all\").
                              May be repeated. Default: md
    -o, --out-dir <DIR>       Output folder, created if missing. Default: the input's folder
        --stdout              Write the result to stdout instead of a file (one input and
                              one format only; images are not written)
        --html-images <MODE>  embed (default) or files
        --theme <THEME>       HTML colours: auto (default, follows the OS), light, dark
        --page-markers        HTML: show page numbers in the margin
        --ocr <SCOPE>         off (default), needed (pages without a text layer),
                              scans (also replace other programs' OCR text), all
        --ocr-device <DEV>    gpu (default, DirectML; falls back to CPU) or cpu
        --latex               Convert display formulas to LaTeX (Pix2Text MFR)
        --no-layout           Do not use the layout model (heuristics only)
        --keep-headers        Keep running heads, footers and page numbers
        --keep-ruby           Keep furigana (ruby) lines of Japanese text
        --password <PW>       Password of an encrypted PDF (or env STRATAPDF_PASSWORD)
    -q, --quiet               No progress messages
    -h, --help                Show this help
    -V, --version             Show the version

NOTES:
    - OCR (NDLOCR-Lite, Japanese and English, vertical text) and --latex download their
      models on first use into %LOCALAPPDATA%\\StrataPDF\\data\\models. OCR results are
      cached per file and shared with the StrataPDF app; text the app already OCRed is
      used even with --ocr off.
    - Without --ocr, scanned pages come out as page images; a warning names how many.
    - Markdown marks page starts as <!-- page N --> comments.
    - With --stdout, set UTF-8 output in PowerShell first:
      [Console]::OutputEncoding = [Text.Encoding]::UTF8
    - StrataPDF-cli.exe is a small console front end: it runs StrataPDF.exe --headless
      from its own folder and waits for it. Calling StrataPDF.exe --headless directly
      also works, but PowerShell and cmd do not wait for that GUI program.

EXAMPLES:
    StrataPDF-cli convert paper.pdf                      # paper.md + paper_files\\
    StrataPDF-cli convert paper.pdf --to md,html         # both formats at once
    StrataPDF-cli convert *.pdf --to both -o out         # a folder of PDFs
    StrataPDF-cli convert scan.pdf --ocr needed          # OCR pages without text first
    StrataPDF-cli convert paper.pdf --stdout             # Markdown on stdout

EXIT STATUS:
    0 all inputs converted, 1 some input failed, 2 usage error.

MANUAL:
    CLI.md next to this program (docs/CLI.md in https://github.com/geoign/StrataPDF).
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Md,
    Html,
}

struct Args {
    inputs: Vec<PathBuf>,
    formats: Vec<Format>,
    out_dir: Option<PathBuf>,
    stdout: bool,
    html_embed: bool,
    theme: Theme,
    page_markers: bool,
    ocr: Option<OcrScope>,
    device: Device,
    latex: bool,
    layout: bool,
    keep_headers: bool,
    keep_ruby: bool,
    password: Option<String>,
    quiet: bool,
}

/// Run the command line `argv` (without the program name); returns the exit code.
pub fn run(argv: Vec<OsString>) -> i32 {
    let args = match parse(argv) {
        Ok(Some(a)) => a,
        Ok(None) => return 0,
        Err(e) => {
            eprintln!("error: {e}\nRun 'StrataPDF-cli --help' for usage.");
            return 2;
        }
    };
    strata_core::fonts::install();
    let mut engines = Engines::default();
    let mut written = HashSet::new();
    let mut failed = 0;
    for input in &args.inputs {
        if let Err(e) = convert(input, &args, &mut engines, &mut written) {
            eprintln!("error: {}: {e}", input.display());
            failed += 1;
        }
    }
    if failed > 0 {
        if args.inputs.len() > 1 {
            eprintln!("{failed} of {} inputs failed", args.inputs.len());
        }
        return 1;
    }
    0
}

// ------------------------------------------------------------------ arguments

/// `Ok(None)`: help or version was printed.
fn parse(argv: Vec<OsString>) -> Result<Option<Args>, String> {
    let mut it = argv.into_iter().peekable();
    let mut a = Args {
        inputs: Vec::new(),
        formats: Vec::new(),
        out_dir: None,
        stdout: false,
        html_embed: true,
        theme: Theme::Auto,
        page_markers: false,
        ocr: None,
        device: Device::Gpu,
        latex: false,
        layout: true,
        keep_headers: false,
        keep_ruby: false,
        password: std::env::var("STRATAPDF_PASSWORD").ok(),
        quiet: false,
    };
    let mut command = false;
    let mut patterns = Vec::new();
    while let Some(arg) = it.next() {
        let Some(s) = arg.to_str().map(str::to_string) else {
            patterns.push(PathBuf::from(arg));
            continue;
        };
        // --opt=value
        let (name, inline) = match s.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (s.clone(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            it.next().and_then(|v| v.into_string().ok()).ok_or_else(|| format!("{name} needs {what}"))
        };
        match name.as_str() {
            "-h" | "--help" | "/?" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "help" if !command && patterns.is_empty() => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("StrataPDF-cli {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "convert" | "export" if !command && patterns.is_empty() => command = true,
            "-t" | "--to" | "--format" => {
                for f in value("a format list")?.split(',').map(|f| f.trim().to_ascii_lowercase()) {
                    match f.as_str() {
                        "md" | "markdown" => a.formats.push(Format::Md),
                        "html" | "htm" => a.formats.push(Format::Html),
                        "both" | "all" => a.formats.extend([Format::Md, Format::Html]),
                        "" => {}
                        _ => return Err(format!("unknown format '{f}' (md, html, both)")),
                    }
                }
            }
            "--md" | "--markdown" => a.formats.push(Format::Md),
            "--html" => a.formats.push(Format::Html),
            "-o" | "--out-dir" | "--output-dir" => a.out_dir = Some(PathBuf::from(value("a folder")?)),
            "--stdout" => a.stdout = true,
            "--html-images" => {
                a.html_embed = match value("embed or files")?.as_str() {
                    "embed" => true,
                    "files" => false,
                    v => return Err(format!("--html-images: unknown mode '{v}' (embed, files)")),
                }
            }
            "--theme" => {
                a.theme = match value("auto, light or dark")?.as_str() {
                    "auto" => Theme::Auto,
                    "light" => Theme::Light,
                    "dark" => Theme::Dark,
                    v => return Err(format!("--theme: unknown theme '{v}' (auto, light, dark)")),
                }
            }
            "--page-markers" => a.page_markers = true,
            "--ocr" => {
                a.ocr = match value("a scope")?.as_str() {
                    "off" | "none" => None,
                    "needed" | "auto" => Some(OcrScope::Needed),
                    "scans" => Some(OcrScope::Scans),
                    "all" => Some(OcrScope::All),
                    v => return Err(format!("--ocr: unknown scope '{v}' (off, needed, scans, all)")),
                }
            }
            "--ocr-device" => {
                a.device = match value("gpu or cpu")?.as_str() {
                    "gpu" => Device::Gpu,
                    "cpu" => Device::Cpu,
                    v => return Err(format!("--ocr-device: unknown device '{v}' (gpu, cpu)")),
                }
            }
            "--latex" => a.latex = true,
            "--no-layout" => a.layout = false,
            "--keep-headers" => a.keep_headers = true,
            "--keep-ruby" => a.keep_ruby = true,
            "--password" => a.password = Some(value("a password")?),
            "-q" | "--quiet" => a.quiet = true,
            "--" => patterns.extend(it.by_ref().map(PathBuf::from)),
            _ if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option '{s}'")),
            _ => patterns.push(PathBuf::from(arg)),
        }
    }
    if patterns.is_empty() {
        if !command {
            print!("{USAGE}");
            return Ok(None);
        }
        return Err("no input file".into());
    }
    for p in patterns {
        a.inputs.extend(expand(&p)?);
    }
    if a.formats.is_empty() {
        a.formats.push(Format::Md);
    }
    let mut seen = Vec::new();
    a.formats.retain(|f| {
        let new = !seen.contains(f);
        seen.push(*f);
        new
    });
    if a.stdout && (a.inputs.len() != 1 || a.formats.len() != 1) {
        return Err("--stdout takes one input and one format".into());
    }
    Ok(Some(a))
}

/// Expand `*` and `?` in the file name part (the shells of Windows leave them).
fn expand(p: &Path) -> Result<Vec<PathBuf>, String> {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !name.contains(['*', '?']) {
        if p.is_dir() {
            return Err(format!("{} is a folder; give files, e.g. {}", p.display(), p.join("*.pdf").display()));
        }
        return Ok(vec![p.to_path_buf()]);
    }
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let pat: Vec<char> = name.to_lowercase().chars().collect();
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| e.file_name().to_str().is_some_and(|n| wildcard(&pat, &n.to_lowercase().chars().collect::<Vec<_>>())))
        .map(|e| if p.parent().is_some_and(|d| !d.as_os_str().is_empty()) { e.path() } else { PathBuf::from(e.file_name()) })
        .collect();
    if out.is_empty() {
        return Err(format!("no file matches {}", p.display()));
    }
    out.sort();
    Ok(out)
}

fn wildcard(p: &[char], s: &[char]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some('*'), _) => wildcard(&p[1..], s) || (!s.is_empty() && wildcard(p, &s[1..])),
        (Some('?'), Some(_)) => wildcard(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => wildcard(&p[1..], &s[1..]),
        _ => false,
    }
}

// ------------------------------------------------------------------ engines

/// Models loaded on first need and kept for the following inputs.
#[derive(Default)]
struct Engines {
    ocr: Option<Arc<dyn OcrEngine>>,
    formula: Option<Arc<dyn FormulaEngine>>,
}

impl Engines {
    fn ocr(&mut self, device: Device, quiet: bool) -> Result<Arc<dyn OcrEngine>, String> {
        if let Some(e) = &self.ocr {
            return Ok(e.clone());
        }
        let set = install_set("ndlocr-lite", quiet)?;
        let e: Arc<dyn OcrEngine> = Arc::new(strata_ocr::ndl::NdlOcr::load(&set, device).map_err(|e| format!("OCR model: {e}"))?);
        self.ocr = Some(e.clone());
        Ok(e)
    }

    fn formula(&mut self, quiet: bool) -> Result<Arc<dyn FormulaEngine>, String> {
        if let Some(e) = &self.formula {
            return Ok(e.clone());
        }
        let set = install_set("pix2text-mfr", quiet)?;
        let e: Arc<dyn FormulaEngine> = Arc::new(strata_ocr::formula::Pix2TextMfr::load(&set).map_err(|e| format!("formula model: {e}"))?);
        self.formula = Some(e.clone());
        Ok(e)
    }
}

fn install_set(id: &str, quiet: bool) -> Result<models::ModelSet, String> {
    let set = models::set(id).ok_or_else(|| format!("model set {id} is unknown"))?;
    if !set.is_installed() {
        if !quiet {
            eprintln!("downloading model {} ({:.0} MB) to {}", set.id, set.total_size() as f64 / 1e6, set.dir().display());
        }
        let tty = !quiet && std::io::stderr().is_terminal();
        let progress = |done: u64, total: u64| {
            if tty {
                eprint!("\r  {:.0} / {:.0} MB", done as f64 / 1e6, total as f64 / 1e6);
            }
        };
        models::install(&set, &progress, &AtomicBool::new(false)).map_err(|e| format!("model download failed: {e}"))?;
        if tty {
            eprintln!();
        }
    }
    Ok(set)
}

// ------------------------------------------------------------------ conversion

fn convert(input: &Path, a: &Args, engines: &mut Engines, written: &mut HashSet<PathBuf>) -> Result<(), String> {
    let tty = !a.quiet && std::io::stderr().is_terminal();
    let doc = Document::open(input, a.password.clone(), Arc::new(|| {})).map_err(|e| match e {
        strata_core::OpenError::PasswordRequired => "password required (--password or env STRATAPDF_PASSWORD)".to_string(),
        e => e.to_string(),
    })?;
    if !a.quiet {
        eprintln!("{}: {} pages", input.display(), doc.info().page_count);
    }
    let t = std::time::Instant::now();

    if let Some(scope) = a.ocr {
        let engine = engines.ocr(a.device, a.quiet)?;
        for ev in doc.run_ocr(scope, engine, Arc::new(AtomicBool::new(false))) {
            match ev {
                OcrEvent::Progress { done, total } if tty => eprint!("\r  OCR {done}/{total} pages"),
                OcrEvent::Done { pages } => {
                    if tty {
                        eprint!("\r");
                    }
                    if !a.quiet {
                        eprintln!("  OCR: {pages} pages");
                    }
                    break;
                }
                OcrEvent::Error(e) => eprintln!("\n  OCR error: {e}"),
                _ => {}
            }
        }
    }

    let formula = if a.latex { Some(engines.formula(a.quiet)?) } else { None };
    let opts = ReflowOptions { strip_headers: !a.keep_headers, drop_ruby: !a.keep_ruby, formula, layout: a.layout, ..ReflowOptions::default() };
    let mut result = None;
    for ev in doc.reflow(opts, Arc::new(AtomicBool::new(false))) {
        match ev {
            ReflowEvent::Progress { done, total } if tty => eprint!("\r  reading {done}/{total} pages"),
            ReflowEvent::Progress { .. } => {}
            ReflowEvent::Done(d) => {
                result = Some(d);
                break;
            }
            ReflowEvent::Error(e) => return Err(e),
        }
    }
    if tty {
        eprint!("\r                              \r");
    }
    let d = result.ok_or("conversion stopped without a result")?;

    let page_images = d.nodes.iter().filter(|n| matches!(n, Node::PageImage { .. })).count();
    if page_images > 0 && a.ocr.is_none() {
        eprintln!("  warning: {page_images} pages have no usable text layer and are kept as images; add --ocr needed to read them");
    }

    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
    let dir = match &a.out_dir {
        Some(d) => d.clone(),
        None => input.parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from(".")),
    };
    let files_name = format!("{stem}_files");
    let rel = |im: &ReflowImage| output::image_link(&files_name, &im.id);

    if a.stdout {
        let text = match a.formats[0] {
            Format::Md => output::to_markdown(&d, &rel),
            Format::Html => html(&d, a, &rel),
        };
        let mut out = std::io::stdout().lock();
        out.write_all(text.as_bytes()).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
        if !d.images.is_empty() && !a.quiet {
            eprintln!("  note: {} images not written (--stdout); they are linked as {files_name}/...", d.images.len());
        }
        return Ok(());
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut targets = Vec::new();
    for f in &a.formats {
        let path = dir.join(format!("{stem}.{}", if *f == Format::Md { "md" } else { "html" }));
        if !written.insert(normalize(&path)) {
            return Err(format!("{} was already written by an earlier input with the same name", path.display()));
        }
        targets.push((*f, path));
    }
    let needs_files = targets.iter().any(|(f, _)| *f == Format::Md || !a.html_embed);
    let used = d.used_images();
    if needs_files && used.iter().any(|u| *u) {
        let images = dir.join(&files_name);
        std::fs::create_dir_all(&images).map_err(|e| format!("{}: {e}", images.display()))?;
        for (im, _) in d.images.iter().zip(&used).filter(|(_, u)| **u) {
            std::fs::write(images.join(&im.id), &im.png).map_err(|e| format!("{}: {e}", images.display()))?;
        }
    }
    let mut stdout = std::io::stdout().lock();
    for (f, path) in targets {
        let text = match f {
            Format::Md => output::to_markdown(&d, &rel),
            Format::Html if a.html_embed => html(&d, a, &|im| format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&im.png))),
            Format::Html => html(&d, a, &rel),
        };
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        let _ = writeln!(stdout, "{}", path.display());
    }
    if !a.quiet {
        eprintln!("  done in {:.1} s: {} blocks, {} images{}", t.elapsed().as_secs_f32(), d.nodes.len(), used.iter().filter(|u| **u).count(), if d.vertical { ", vertical text" } else { "" });
    }
    Ok(())
}

fn html(d: &ReflowDoc, a: &Args, src: &dyn Fn(&ReflowImage) -> String) -> String {
    output::to_html(d, &HtmlOptions { theme: a.theme, page_markers: a.page_markers, image_src: src, extra_css: "", bilingual: None })
}

/// Key for "same output file": the absolute path, case-folded (Windows file names).
fn normalize(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    PathBuf::from(abs.to_string_lossy().to_lowercase())
}
