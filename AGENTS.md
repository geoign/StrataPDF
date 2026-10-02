# Notes for AI agents

## Converting documents with StrataPDF (headless)

StrataPDF has a command-line converter, `StrataPDF-cli.exe`, that turns PDF, EPUB,
XPS/OXPS, CBZ, FB2, Markdown and text files into Markdown and/or HTML without
opening a window. It is the same conversion as the app's text view: paragraphs are
rejoined across columns and pages, running heads and page numbers are dropped,
figures and tables are saved as images, and formulas can become LaTeX.

Where it is:

- Installed copy: next to `StrataPDF.exe` (find it from the registry value
  `HKCU\Software\Classes\StrataPDF.Document\shell\open\command`; the default install
  folder is `%LOCALAPPDATA%\Programs\StrataPDF`). `CLI.md` there is the manual.
- Built from this repository: `cargo build --release -p strata-cli` gives
  `target\release\strata-cli.exe` (same program, unrenamed).

```powershell
StrataPDF-cli.exe --help                                # full usage
StrataPDF-cli.exe convert paper.pdf                     # -> paper.md + paper_files\
StrataPDF-cli.exe convert paper.pdf --to md,html        # Markdown and HTML at once
StrataPDF-cli.exe convert *.pdf --to both -o out        # many files (wildcards expanded by the program)
StrataPDF-cli.exe convert scan.pdf --ocr needed         # OCR pages without a text layer (Japanese OK)
StrataPDF-cli.exe convert paper.pdf --latex             # display formulas as LaTeX
StrataPDF-cli.exe convert paper.pdf --stdout -q         # Markdown on stdout
```

Things to know:

- Call `StrataPDF-cli.exe`, not `StrataPDF.exe`. `StrataPDF.exe --headless ...` forwards to
  the CLI, but it is a GUI program: PowerShell and cmd do not wait for it.
- Written paths are printed on stdout, one per line; progress and warnings on stderr.
  Exit status: 0 ok, 1 some input failed, 2 usage error.
- Without `--ocr`, scanned pages stay images and stderr warns how many. OCR and `--latex`
  download their models on first use (into `%LOCALAPPDATA%\StrataPDF\data\models`).
- With `--stdout` in PowerShell, set `[Console]::OutputEncoding = [Text.Encoding]::UTF8`
  first, or Japanese text is garbled. Writing files avoids the issue.

The full manual (Japanese) is [docs/CLI.md](docs/CLI.md).

## Working on the code

See [README.md](README.md) (build, layout of the crates) and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The CLI is `crates/strata-cli`; the
conversion itself is `strata-core::reflow` (`reflow::output::to_markdown` / `to_html`).
