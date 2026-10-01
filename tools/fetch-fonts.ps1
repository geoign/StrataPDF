# テキスト表示に同梱するフォント（SIL OFL 1.1）を取得する。
# 入手元は google/fonts の固定コミット。ファイル名を変えるだけで中身は改変しない（OFL の条件）。
#
#   pwsh tools\fetch-fonts.ps1                  assets\fonts に取得（開発用、git の追跡外）
#   pwsh tools\fetch-fonts.ps1 -Dest <dir>      <dir> に取得（配置・パッケージ用）
#
# 取得済みでハッシュが一致するファイルはダウンロードしない。
param([string]$Dest = (Join-Path (Split-Path -Parent $PSScriptRoot) 'assets\fonts'))
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$base = 'https://raw.githubusercontent.com/google/fonts/9710da1eacb3be272583c3224dcb70f9da6eadbb/ofl'
$files = @(
    @{ Src = 'notoserifjp/NotoSerifJP%5Bwght%5D.ttf'; Name = 'NotoSerifJP-VF.ttf';     Sha = '2fd527ba12b6a44ec30d796d633360da0aeba6c5d4af1304ce12bb4dc15a7dfc' }
    @{ Src = 'notosansjp/NotoSansJP%5Bwght%5D.ttf';   Name = 'NotoSansJP-VF.ttf';      Sha = 'c2f3b4d463500a2ddcd3849cded1fceeb9fd6d1c32e6cbecd568453ba50fc68f' }
    @{ Src = 'lineseedjp/LINESeedJP-Regular.ttf';     Name = 'LINESeedJP-Regular.ttf'; Sha = '04a6c0077ddb8ba5af3638bd76b4600708dde7b38df047b40fbe6f3af358d3c3' }
    @{ Src = 'lineseedjp/LINESeedJP-Bold.ttf';        Name = 'LINESeedJP-Bold.ttf';    Sha = '67aec2dc10b3ad210d6f7d53b33bbfea42ea28e32fa3834624eee699a638d5ff' }
    @{ Src = 'notoserifjp/OFL.txt';                   Name = 'OFL-NotoSerifJP.txt';    Sha = '5e0da210fb04058a8c0087985d2d456b931c2579811a49655721d3cf0c36b6d6' }
    @{ Src = 'notosansjp/OFL.txt';                    Name = 'OFL-NotoSansJP.txt';     Sha = '1c05c68c34f9708415aada51f17e1b0092d2cea709bf4a94cd38114f9e73d7d9' }
    @{ Src = 'lineseedjp/OFL.txt';                    Name = 'OFL-LINESeedJP.txt';     Sha = '1cfc3752ddd076bc7d461b6680e42a345f2579f91319fa04192b754b3b993f65' }
)
New-Item -ItemType Directory -Force $Dest | Out-Null
foreach ($f in $files) {
    $path = Join-Path $Dest $f.Name
    if ((Test-Path $path) -and (Get-FileHash $path -Algorithm SHA256).Hash -eq $f.Sha) { continue }
    Write-Host "取得: $($f.Name)"
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$($f.Src)" -OutFile $path
    $h = (Get-FileHash $path -Algorithm SHA256).Hash
    if ($h -ne $f.Sha) {
        Remove-Item $path
        throw "$($f.Name) のハッシュが一致しません（$h）"
    }
}
