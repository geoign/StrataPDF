# StrataPDF の配置と、現在のユーザーへの登録（管理者権限は不要）。
#
# 配布版（Releases の zip）: 展開したフォルダーの install.ps1 を実行すると、そのフォルダーの
# StrataPDF.exe をその場で登録する。先にフォルダーを置き場所へ移しておくこと。
#
# ソースから:
#   pwsh tools\install.ps1            ビルドしてから配置
#   pwsh tools\install.ps1 -NoBuild   ビルド済みの実行ファイルを配置
#   配置先の既定は、登録済みならその場所、未登録なら %LOCALAPPDATA%\Programs\StrataPDF。
#
# 登録後、Windows の「設定 > アプリ > 既定のアプリ」で StrataPDF を .pdf の既定に選べる。
param(
    [string]$Dest,
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$progId = 'StrataPDF.Document'
$classes = 'HKCU:\Software\Classes'

$packaged = Test-Path (Join-Path $PSScriptRoot 'StrataPDF.exe')
if ($packaged) {
    # 配布版：その場で登録する。
    if (-not $Dest) { $Dest = $PSScriptRoot }
    $Dest = (Resolve-Path $Dest).Path
    if ($Dest -ne $PSScriptRoot) { throw '配布版は展開したフォルダーで登録する。フォルダーごと移してから実行すること' }
} else {
    if (-not $Dest) {
        # 登録済みの場所を引き継ぐ。
        $cmd = (Get-ItemProperty "$classes\$progId\shell\open\command" -ErrorAction SilentlyContinue).'(default)'
        if ($cmd -match '^"([^"]+)"') { $Dest = Split-Path -Parent $Matches[1] }
        else { $Dest = "$env:LOCALAPPDATA\Programs\StrataPDF" }
    }
    $repo = Split-Path -Parent $PSScriptRoot
    Push-Location $repo
    try {
        if (-not $NoBuild) {
            cargo build --release -p strata-app -p strata-cli
            if ($LASTEXITCODE -ne 0) { throw 'cargo build が失敗しました' }
        }
        $target = Join-Path ((cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory) 'release'
    } finally { Pop-Location }

    # 配置先で動いている StrataPDF を終了してから上書きする。
    Get-Process StrataPDF, StrataPDF-cli -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($Dest) } | Stop-Process -Force
    Start-Sleep -Milliseconds 300

    New-Item -ItemType Directory -Force $Dest | Out-Null
    Copy-Item "$target\strata-app.exe" "$Dest\StrataPDF.exe" -Force
    # ウィンドウを出さない変換（Markdown / HTML）。使い方は CLI.md。
    Copy-Item "$target\strata-cli.exe" "$Dest\StrataPDF-cli.exe" -Force
    Copy-Item "$repo\docs\CLI.md" "$Dest\CLI.md" -Force
    # DirectML.dll はビルド出力ではシンボリックリンクなので、実体をコピーする。
    $dml = Get-Item "$target\DirectML.dll"
    if ($dml.LinkType) { $dml = Get-Item ($dml.Target | Select-Object -First 1) }
    Copy-Item $dml.FullName "$Dest\DirectML.dll" -Force
    # テキスト表示に同梱するフォント（取得済みなら再取得しない）。
    & (Join-Path $PSScriptRoot 'fetch-fonts.ps1') -Dest "$Dest\fonts"
    @"
StrataPDF（ソースからのビルド）

ライセンス: AGPL-3.0-or-later（描画エンジン MuPDF を含む）
ソース: https://github.com/geoign/StrataPDF
ウィンドウを出さない変換（Markdown / HTML）: StrataPDF-cli.exe --help、詳細は CLI.md
登録の解除: pwsh "$repo\tools\uninstall.ps1"
"@ | Set-Content -Encoding utf8 "$Dest\README.txt"
}

$exe = "$Dest\StrataPDF.exe"
$exts = '.pdf', '.epub', '.xps', '.oxps', '.cbz'

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
