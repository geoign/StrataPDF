# StrataPDF の登録を解除する。-RemoveFiles で配置したファイルも削除する。
# ユーザーデータ（%LOCALAPPDATA%\StrataPDF の OCR モデル・キャッシュ）は残す。
param(
    [string]$Dest,
    [switch]$RemoveFiles
)
$ErrorActionPreference = 'Continue'
$classes = 'HKCU:\Software\Classes'
$progId = 'StrataPDF.Document'
if (-not $Dest) {
    $cmd = (Get-ItemProperty "$classes\$progId\shell\open\command" -ErrorAction SilentlyContinue).'(default)'
    if ($cmd -match '^"([^"]+)"') { $Dest = Split-Path -Parent $Matches[1] }
}

Remove-Item "$classes\$progId" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item "$classes\Applications\StrataPDF.exe" -Recurse -Force -ErrorAction SilentlyContinue
foreach ($e in '.pdf', '.epub', '.xps', '.oxps', '.cbz') {
    Remove-ItemProperty "$classes\$e\OpenWithProgids" -Name $progId -ErrorAction SilentlyContinue
}
Remove-Item 'HKCU:\Software\StrataPDF' -Recurse -Force -ErrorAction SilentlyContinue
Remove-ItemProperty 'HKCU:\Software\RegisteredApplications' -Name 'StrataPDF' -ErrorAction SilentlyContinue
Remove-Item "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\StrataPDF.lnk" -Force -ErrorAction SilentlyContinue

if ($RemoveFiles -and $Dest) {
    Get-Process StrataPDF -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($Dest) } | Stop-Process -Force
    Remove-Item $Dest -Recurse -Force -ErrorAction SilentlyContinue
}

Add-Type -Namespace Win32 -Name Shell -MemberDefinition '[DllImport("shell32.dll")] public static extern void SHChangeNotify(int e, uint f, System.IntPtr a, System.IntPtr b);'
[Win32.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
Write-Host '登録を解除しました'
