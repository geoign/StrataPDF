# StrataPDF

Windows 向けの PDF ビューア兼エディタ。Rust 製。描画と解析のエンジンは MuPDF。

> [!WARNING]
> **このリポジトリは私的利用に限る。配布・公開・第三者への提供をしないこと。**
>
> StrataPDF は MuPDF（AGPL-3.0）を静的リンクしている。バイナリをひとたび他者に渡したり、
> ネットワーク越しに使わせたりすると、AGPL によりソース全体（本リポジトリを含む）を AGPL で
> 公開する義務が生じる。義務を負わずに配布するには、Artifex 社の商用ライセンスが要る。
>
> 配布を検討する時点で、次のどちらかを先に決めること。
> 1. 本リポジトリを AGPL-3.0 で公開する。
> 2. Artifex の商用ライセンスを取得するか、描画エンジンを PDFium（BSD）に差し替える。
>    差し替える場合の影響範囲は `docs/ARCHITECTURE.md` の「エンジン依存の境界」を参照。
>
> 本リポジトリの `Cargo.toml` の `license` は、上記の事情を示すために `AGPL-3.0-or-later` としてある。
> `publish = false` によって crates.io への誤公開も防いでいる。

## 設計方針

- **あらゆる PDF を開く。** 壊れた xref の修復、全種類の暗号、数百 MB のファイルに対応する。
- **軽快な UI。** 描画は並列のタイル描画とし、UI スレッドは描画を待たない。メモリは多めに使う。
- **ユーザー中心。** 暗号化 PDF の権限フラグ（印刷不可・コピー不可など）は無視する。
  復号さえできれば、エンジンでできる操作はすべて許可する。
- **日本語第一。** 縦書き、見開きの右綴じ、日本語 OCR を一級の機能として扱う。

## ビルド

前提は Rust stable（1.95 以上）、Visual Studio 2022 Build Tools（MSVC、msbuild）、LLVM（bindgen 用の libclang）の三つ。

```powershell
cargo build --release -p strata-app
```

ビルド成果物は OneDrive を圧迫しないよう `C:\tmp\cargo-target\StrataPDF` に出力する
（`.cargo/config.toml` で指定）。MuPDF の C ソースは初回だけコンパイルされ、数分かかる。

## インストール（現在のユーザーのみ、管理者権限不要）

```powershell
pwsh tools\install.ps1          # ビルドして OneDrive\Apps\StrataPDF に配置し、関連付けの候補に登録
pwsh tools\install.ps1 -NoBuild # ビルド済みのものを配置
pwsh tools\uninstall.ps1        # 登録を解除（-RemoveFiles で配置ファイルも削除）
```

登録後、「設定 > アプリ > 既定のアプリ > StrataPDF」で .pdf の既定に選べる。Windows の仕様上、
既定のアプリをプログラムから直接切り替えることはできない。

## 構成

| パス | 内容 |
|---|---|
| `crates/strata-core` | MuPDF のラッパー。文書スレッド、タイル描画プール、構造化テキスト、フォント解決 |
| `crates/strata-app` | GUI 本体（egui と wgpu）。タブ、分割表示、見開き |
| `vendor/mupdf` | `mupdf` クレートのパッチ版。差分は `vendor/PATCHES.md` |
| `docs/` | 設計（`ARCHITECTURE.md`）と開発計画・拡張予定（`ROADMAP.md`） |
