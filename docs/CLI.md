# StrataPDF-cli：ウィンドウを出さない変換

`StrataPDF-cli.exe` は、PDF などの文書を Markdown と HTML に変換するコンソール用プログラムである。
アプリのテキスト表示にある「Markdown で保存」「HTML で保存」と同じ処理を、ウィンドウを開かずに行う。
段組み・図・ページ境界をまたいで段落をつなぎ直し、柱とページ番号を除いた本文を書き出す。

配布版の zip と `tools\install.ps1` の配置先では、`StrataPDF.exe` と同じフォルダーに置かれる。
スクリプトやエージェントから使うときは、このファイルを直接呼ぶ。
変換の処理そのものは `StrataPDF.exe` が持っており、`StrataPDF-cli.exe` は同じフォルダーの
`StrataPDF.exe` を起動して終了を待つだけの小さなプログラムである。二つは同じフォルダーに置いておくこと。

```powershell
StrataPDF-cli.exe convert paper.pdf                    # paper.md と paper_files\
StrataPDF-cli.exe convert paper.pdf --to md,html       # Markdown と HTML を同時に
StrataPDF-cli.exe convert *.pdf --to both -o out       # フォルダー内の PDF をまとめて
StrataPDF-cli.exe convert scan.pdf --ocr needed        # 文字の層がないページを OCR してから
StrataPDF-cli.exe convert paper.pdf --stdout           # Markdown を標準出力へ
StrataPDF-cli.exe --help                               # 全オプション
```

## 入力

PDF、EPUB、XPS / OXPS、CBZ、FB2、Markdown（.md）、テキスト（.txt）を受け付ける。
暗号化 PDF のパスワードは `--password` か環境変数 `STRATAPDF_PASSWORD` で渡す。

入力は複数並べられ、順に変換する。ファイル名の部分の `*` と `?` はプログラム側で展開するので、
ワイルドカードを展開しない PowerShell と cmd でも `*.pdf` と書ける。フォルダーを直接渡すとエラーになる。

## 出力

入力のファイル名から拡張子を除いた部分を `<stem>` とすると、出力は次のとおり。
置き場所は `-o` / `--out-dir` で指定したフォルダー（なければ作る）で、指定がなければ入力と同じフォルダーである。

| 形式 | ファイル | 画像 |
|---|---|---|
| Markdown（`md`） | `<stem>.md` | `<stem>_files\` に PNG で保存し、相対パスで参照する |
| HTML（`html`） | `<stem>.html` | 既定では data URI で埋め込み、1 ファイルで完結する。`--html-images files` にすると `<stem>_files\` を参照する |

形式は `-t` / `--to` で選ぶ。既定は `md` で、`md,html`、`both`、`all` のいずれかで両方を書き出す。
`--to` は繰り返してもよい。同じ名前のファイルがあれば上書きする。

Markdown では、ページの始まりを `<!-- page N -->` というコメントで示す。
数式は `--latex` を付けると LaTeX（`$$ ... $$`）になり、付けなければ画像のまま残る。

書き出したファイルのパスは標準出力に 1 行ずつ出る。進行状況と警告は標準エラー出力に出る。
`-q` で進行状況を消せる。

`--stdout` を付けると、ファイルを作らずに結果を標準出力に出す。入力と形式はそれぞれ一つに限る。
画像は保存せず、リンクだけが残る。PowerShell で受け取るときは、日本語が化けないように先に
`[Console]::OutputEncoding = [Text.Encoding]::UTF8` を実行しておく。

## オプション

| オプション | 内容 |
|---|---|
| `-t`, `--to <形式>` | `md`、`html`、`md,html`（`both`、`all` も可）。既定は `md` |
| `-o`, `--out-dir <フォルダー>` | 出力先。既定は入力と同じフォルダー |
| `--stdout` | 標準出力へ書く（入力一つ・形式一つ） |
| `--html-images <embed\|files>` | HTML の画像の扱い。既定は `embed` |
| `--theme <auto\|light\|dark>` | HTML の配色。既定の `auto` は OS の設定に従う |
| `--page-markers` | HTML の余白にページ番号を出す |
| `--ocr <off\|needed\|scans\|all>` | OCR の範囲。既定は `off` |
| `--ocr-device <gpu\|cpu>` | OCR を動かす装置。既定の `gpu` は DirectML を使い、使えなければ CPU に戻る |
| `--latex` | 表示数式を LaTeX に変換する（Pix2Text MFR） |
| `--no-layout` | レイアウト解析モデルを使わず、規則だけで組む |
| `--keep-headers` | 柱・フッター・ページ番号を残す |
| `--keep-ruby` | 日本語のルビの行を残す |
| `--password <PW>` | 暗号化 PDF のパスワード |
| `-q`, `--quiet` | 進行状況を出さない |

## OCR と数式のモデル

`--ocr` の範囲は三つある。`needed` は文字の層がないページだけを読む。`scans` は他のプログラムが付けた
OCR の文字も置き換える。`all` はすべてのページを読む。OCR は NDLOCR-Lite の移植で、日本語と縦書きに対応する。

`--ocr off` のままスキャンのページがあると、そのページは画像として残り、警告がページ数を示す。
ただし、アプリで OCR 済みのファイルなら、その結果を `--ocr off` でも使う。OCR の結果はファイルごとに
`%LOCALAPPDATA%\StrataPDF\data\ocr` に保存され、アプリと CLI で共有する。

OCR と `--latex` のモデルは、初めて使うときに GitHub と Hugging Face から
`%LOCALAPPDATA%\StrataPDF\data\models` に取得する。取得中は標準エラー出力に大きさを示す。

## 終了コード

| コード | 意味 |
|---|---|
| 0 | すべての入力を変換した |
| 1 | 変換に失敗した入力がある（ほかの入力は続けて変換する） |
| 2 | オプションの誤り |

## StrataPDF.exe から呼ぶ場合

`StrataPDF.exe --headless convert ...`、`StrataPDF.exe convert ...`、`StrataPDF.exe --help` は、
ウィンドウを開かずにその場で変換やヘルプの表示をする。`StrataPDF-cli.exe` が内部で呼んでいるのもこの形である。
ただし `StrataPDF.exe` はウィンドウ用のプログラムなので、PowerShell と cmd はその終了を待たずに次へ進み、
終了コードも受け取れない。Git Bash は終了を待つ。スクリプトからは `StrataPDF-cli.exe` を使うこと。

`StrataPDF-cli.exe` を強制終了すると（Ctrl+C を含む）、変換中の `StrataPDF.exe` も一緒に終わる。

## ビルド

```powershell
cargo build --release -p strata-app -p strata-cli
```

`target\release` に `strata-app.exe` と `strata-cli.exe` ができる。`strata-cli.exe` は同じフォルダーの
`StrataPDF.exe` か `strata-app.exe` を起動するので、ビルド出力のままでも動く。配置では二つをそれぞれ
`StrataPDF.exe` と `StrataPDF-cli.exe` に改名する。GPU で OCR するには、同じフォルダーに `DirectML.dll` が要る
（ビルド出力には ort が置く）。
