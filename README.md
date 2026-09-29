# StrataPDF

Windows 向けの PDF ビューア。Rust 製で、描画と解析のエンジンは [MuPDF](https://mupdf.com/)。
学術論文と日本語の本を読むことを主な用途とする。

## 主な機能

- **表示**：タブと分割表示、連続スクロール、単ページ・見開き（右綴じ対応）、縮小時の複数見開き、サムネイル、目次、全文検索、リンク
- **開ける文書**：PDF（壊れた xref やページツリーの修復、全種類の暗号）、EPUB、XPS、CBZ
- **テキスト表示（リフロー）**：段組み・図・ページ境界をまたいで段落をつなぎ直し、ヘッダ・フッタを除いた本文を表示する。
  見出し・キャプション・図中の文字・柱の判別には、PyMuPDF Layout のレイアウト解析モデル（CPU で動く小さなグラフニューラルネットワーク）を使う。
  縦書き、ダークモード、文字サイズの変更、Markdown / HTML への書き出しに対応
- **OCR**：国立国会図書館の NDLOCR-Lite を Rust に移植したもの（日本語・縦書き対応）。
  結果は選択・検索・テキスト表示に反映され、検索可能 PDF として書き出せる。表示数式は LaTeX に変換する（Pix2Text MFR）
- **注釈**：ハイライト・下線・取り消し線・図形・手書き・テキストボックス・付箋。取り消しとやり直し、上書き保存（差分追記）
- **その他**：表の取り出し（CSV / TSV）、画像の保存、印刷、縦書きテキストのコピー
- **翻訳（実験的機能）**：テキスト表示の見開き対訳。送信先は Gemini、OpenAI、Anthropic、OpenRouter、
  OpenAI 互換サーバー（ローカルの llama.cpp や LM Studio も可）から選ぶ

設計方針：

- **あらゆる PDF を開く。** 壊れたファイル、全種類の暗号、数百 MB のファイルに対応する。
- **軽快な UI。** 描画は並列のタイル描画とし、UI スレッドは描画を待たない。
- **ユーザー中心。** 暗号化 PDF の権限フラグ（印刷不可・コピー不可など）は無視する。
  復号さえできれば、エンジンでできる操作はすべて許可する。
- **日本語第一。** 縦書き、見開きの右綴じ、日本語 OCR を一級の機能として扱う。

## 導入

[Releases](https://github.com/geoign/StrataPDF/releases) から zip を取得し、好きな場所に展開する。
`StrataPDF.exe` をそのまま起動できる。動作環境は Windows 10 / 11（x64）。
テキスト表示には Microsoft Edge WebView2 ランタイムを使う（Windows 11 には標準で入っている）。

実行ファイルには署名がないので、初回起動時に SmartScreen の警告が出る。「詳細情報 > 実行」で起動できる。

関連付けの候補とスタートメニューに登録するには、展開したフォルダーで次を実行する（管理者権限は不要）。
フォルダーはその場で登録されるので、先に置き場所へ移しておくこと。

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1     # 登録
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1   # 登録の解除
```

登録後、「設定 > アプリ > 既定のアプリ > StrataPDF」で .pdf の既定に選べる。Windows の仕様上、
既定のアプリをプログラムから直接切り替えることはできない。

OCR と数式のモデルは、初めて使うときに GitHub と Hugging Face から取得する。
モデル・キャッシュ・設定は `%LOCALAPPDATA%\StrataPDF` に置かれる。

## 翻訳機能について

翻訳は実験的機能であり、論文の訳として品質を保証しない。使うには、送信先ごとの API キーを利用者が用意する。
キーは Windows の資格情報マネージャーに保存する。

翻訳を始めると、文書の本文が選んだ送信先に送られる。送信前に確認のダイアログを出すので、
秘密を含む文書を送ってよいかは利用者が判断すること。

## ソースからのビルド

前提は次の三つ。

- Rust stable（1.95 以上）
- Visual Studio 2022 Build Tools（MSVC、msbuild）
- LLVM（bindgen が使う libclang）

```powershell
cargo build --release -p strata-app
```

MuPDF の C ソースは初回だけコンパイルされ、数分かかる。ビルド出力の置き場所などを手元の環境に合わせるには、
`.cargo/config.toml`（リポジトリには含めない）に書く。例：

```toml
[build]
target-dir = "C:/tmp/cargo-target/StrataPDF"

[env]
LIBCLANG_PATH = "C:/Program Files/LLVM/bin"
```

ビルドしたものを配置して登録するには次を使う。配置先の既定は、登録済みならその場所、未登録なら
`%LOCALAPPDATA%\Programs\StrataPDF`。

```powershell
pwsh tools\install.ps1           # ビルドして配置し、登録
pwsh tools\install.ps1 -NoBuild  # ビルド済みのものを配置
pwsh tools\uninstall.ps1         # 登録を解除（-RemoveFiles で配置ファイルも削除）
```

## 構成

| パス | 内容 |
|---|---|
| `crates/strata-core` | MuPDF のラッパー。文書スレッド、タイル描画プール、構造化テキスト、リフロー、注釈、表の取り出し |
| `crates/strata-app` | GUI 本体（egui と wgpu、テキスト表示は WebView2）。タブ、分割表示、見開き |
| `crates/strata-ocr` | OCR（NDLOCR-Lite の移植）と数式認識（Pix2Text MFR） |
| `crates/strata-translate` | 翻訳の送信先の実装、キャッシュ |
| `vendor/mupdf` | `mupdf` クレートのパッチ版。差分は `vendor/PATCHES.md` |
| `docs/` | 設計（`ARCHITECTURE.md`）、開発計画（`ROADMAP.md`）、翻訳ビューの仕様（`translation-view.md`） |

## ライセンス

StrataPDF は [GNU Affero General Public License v3.0](LICENSE) 以降（AGPL-3.0-or-later）で公開する。
描画エンジンの MuPDF（Artifex Software、AGPL-3.0）を静的リンクしているためである。

- OCR の実装は [NDLOCR-Lite](https://github.com/ndl-lab/ndlocr-lite)（国立国会図書館、CC BY 4.0）の推論処理を Rust に移植したもの。
  モデルも同じ配布元から取得する
- 数式認識のモデルは [Pix2Text MFR 1.5](https://huggingface.co/breezedeus/pix2text-mfr-1.5)（MIT）
- レイアウト解析は [PyMuPDF Layout](https://github.com/ArtifexSoftware/pymupdf_layout)（Artifex、AGPL-3.0）の
  特徴量計算（C）とモデル（ONNX）をそのまま組み込み、Python の処理部分を Rust に移植したもの（`vendor/pymupdf_layout`）
- 配布版の zip には、依存する Rust クレートのライセンス表記（`THIRD-PARTY-NOTICES.html`）と、
  ONNX Runtime の DirectML 実行に使う `DirectML.dll`（Microsoft、再配布可能なランタイム）を同梱する
