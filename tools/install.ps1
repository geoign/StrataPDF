# StrataPDF の配置と、現在のユーザーへの登録（管理者権限は不要）。
#   pwsh tools\install.ps1            ビルドしてから配置
#   pwsh tools\install.ps1 -NoBuild   ビルド済みの実行ファイルを配置
# 登録後、Windows の「設定 > アプリ > 既定のアプリ」で StrataPDF を .pdf の既定に選べる。
param(
    [string]$Dest = "$env:USERPROFILE\OneDrive\Apps\StrataPDF",
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$target = 'C:\tmp\cargo-target\StrataPDF\release'

if (-not $NoBuild) {
    Push-Location $repo
    try {
        cargo build --release -p strata-app
        if ($LASTEXITCODE -ne 0) { throw 'cargo build が失敗しました' }
    } finally { Pop-Location }
}

# 配置先で動いている StrataPDF を終了してから上書きする。
Get-Process StrataPDF -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($Dest) } | Stop-Process -Force
Start-Sleep -Milliseconds 300

New-Item -ItemType Directory -Force $Dest | Out-Null
Copy-Item "$target\strata-app.exe" "$Dest\StrataPDF.exe" -Force
# DirectML.dll はビルド出力ではシンボリックリンクなので、実体をコピーする。
$dml = Get-Item "$target\DirectML.dll"
if ($dml.LinkType) { $dml = Get-Item ($dml.Target | Select-Object -First 1) }
Copy-Item $dml.FullName "$Dest\DirectML.dll" -Force
# llama.cpp（ローカル翻訳モデル）の共有ライブラリと GPU バックエンド。
foreach ($n in 'llama.dll', 'llama-common.dll', 'ggml.dll', 'ggml-base.dll') {
    if (Test-Path "$target\$n") { Copy-Item "$target\$n" "$Dest\$n" -Force }
}
if (Test-Path "$target\ggml-backends") {
    New-Item -ItemType Directory -Force "$Dest\ggml-backends" | Out-Null
    Copy-Item "$target\ggml-backends\*.dll" "$Dest\ggml-backends\" -Force
}
@"
StrataPDF（私的利用に限る）

MuPDF（AGPL-3.0）を含むため、このフォルダの内容を他者に配布しないこと。
ソース: $repo
登録の解除: pwsh "$repo\tools\uninstall.ps1"
"@ | Set-Content -Encoding utf8 "$Dest\README.txt"

$exe = "$Dest\StrataPDF.exe"
$progId = 'StrataPDF.Document'
$exts = '.pdf', '.epub', '.xps', '.oxps', '.cbz'
$classes = 'HKCU:\Software\Classes'

function Set-Default([string]$path, [string]$value) {
    if (-not (Test-Path $path)) { New-Item $path -Force | Out-Null }
    Set-ItemProperty -Path $path -Name '(default)' -Value $value
}

# ProgID
Set-Default "$classes\$progId" 'PDF 文書 (StrataPDF)'
Set-Default "$classes\$progId\DefaultIcon" "`"$exe`",0"
Set-Default "$classes\$progId\shell\open\command" "`"$exe`" `"%1`""

# 「プログラムから開く」の候補
foreach ($e in $exts) {
    $k = "$classes\$e\OpenWithProgids"
    if (-not (Test-Path $k)) { New-Item $k -Force | Out-Null }
    New-ItemProperty -Path $k -Name $progId -PropertyType String -Value '' -Force | Out-Null
}
Set-Default "$classes\Applications\StrataPDF.exe\shell\open\command" "`"$exe`" `"%1`""
Set-ItemProperty "$classes\Applications\StrataPDF.exe" -Name 'FriendlyAppName' -Value 'StrataPDF'
$st = "$classes\Applications\StrataPDF.exe\SupportedTypes"
if (-not (Test-Path $st)) { New-Item $st -Force | Out-Null }
foreach ($e in $exts) { New-ItemProperty -Path $st -Name $e -PropertyType String -Value '' -Force | Out-Null }

# 「既定のアプリ」への登録（Capabilities と RegisteredApplications）
$cap = 'HKCU:\Software\StrataPDF\Capabilities'
if (-not (Test-Path "$cap\FileAssociations")) { New-Item "$cap\FileAssociations" -Force | Out-Null }
Set-ItemProperty $cap -Name 'ApplicationName' -Value 'StrataPDF'
Set-ItemProperty $cap -Name 'ApplicationDescription' -Value 'PDF ビューア（縦書き・OCR・リフロー表示）'
Set-ItemProperty $cap -Name 'ApplicationIcon' -Value "`"$exe`",0"
foreach ($e in $exts) { Set-ItemProperty "$cap\FileAssociations" -Name $e -Value $progId }
$reg = 'HKCU:\Software\RegisteredApplications'
if (-not (Test-Path $reg)) { New-Item $reg -Force | Out-Null }
Set-ItemProperty $reg -Name 'StrataPDF' -Value 'Software\StrataPDF\Capabilities'

# スタートメニュー
$lnk = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\StrataPDF.lnk"
$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($lnk)
$sc.TargetPath = $exe
$sc.WorkingDirectory = $Dest
$sc.IconLocation = "$exe,0"
$sc.Description = 'StrataPDF'
$sc.Save()

# エクスプローラーに関連付けの変更を知らせる。
Add-Type -Namespace Win32 -Name Shell -MemberDefinition '[DllImport("shell32.dll")] public static extern void SHChangeNotify(int e, uint f, System.IntPtr a, System.IntPtr b);'
[Win32.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)

Write-Host "配置しました: $exe"
Write-Host '既定の PDF アプリにするには: 設定 > アプリ > 既定のアプリ > StrataPDF で .pdf を選ぶ'
Write-Host '（または PDF を右クリック > プログラムから開く > 別のプログラムを選択 > StrataPDF > 常に使う）'
