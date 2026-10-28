# 把将来公开仓库应包含的文件导出到本地目录并扫描，供维护者审阅。
# 只复制和检查，不创建仓库、不提交、不推送；是否开源由维护者确认后另行操作。
# 用法：powershell -File Tools/export-public.ps1 [-OutDir target/public-export] [-IncludeUntracked]
param(
    [string]$OutDir = 'target/public-export',
    # 同时导出未跟踪但未被 .gitignore 忽略的文件（审阅进行中的工作区时使用）。
    [switch]$IncludeUntracked
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$out = [IO.Path]::GetFullPath((Join-Path $root $OutDir))
$targetRoot = [IO.Path]::GetFullPath((Join-Path $root 'target'))
$insideRepo = $out.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
if ($insideRepo -and -not $out.StartsWith($targetRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "导出目录在仓库内时只能位于 target/ 下：$out"
}

# 不进入公开仓库的路径（前缀匹配，统一用 /）。迁往私有仓库，见 private/references/open-source-readiness-2026-10-12.md。
$privatePrefixes = @(
    'archive/',
    'quicker/',
    'package-quicker.ps1',
    'install-quicker-action.ps1',
    '开发进度.md',
    '继续迭代.md',
    'private/',
    'RustShot.code-workspace',
    '.claude/',
    'Tools/temp/',
    'Tools/publish-ocr-bitiful.py',
    'Tools/test_publish_ocr_bitiful.py',
    'Tools/export-public.ps1',
    'vendor/gpui/examples/'
    'references/brand-naming-and-icon-2026-10-16.md',
    'design/brand-study-2026-10-16/',
    'design/quicker-action-icon-2026-10-18/'
)

Push-Location $root
try {
    $files = @(git -c core.quotepath=false ls-files)
    if ($IncludeUntracked) { $files += @(git -c core.quotepath=false ls-files --others --exclude-standard) }
} finally { Pop-Location }
$files = $files | Where-Object { $_ } | Sort-Object -Unique
$public = @()
$skipped = @()
foreach ($f in $files) {
    if ($privatePrefixes | Where-Object { $f -eq $_ -or $f.StartsWith($_) }) { $skipped += $f } else { $public += $f }
}

if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $out | Out-Null
$missing = @()
foreach ($f in $public) {
    $src = Join-Path $root $f
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { $missing += $f; continue }
    $dst = Join-Path $out $f
    New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
    Copy-Item -LiteralPath $src -Destination $dst
}

# 扫描规则：命中即列出“文件:行”，不打印匹配到的值本身，避免把疑似密钥写进日志。
$rules = [ordered]@{
    'secret'        = '(?i)(BITIFUL_(ACCESS|SECRET)_KEY|aws_secret_access_key|access_?key_?secret)\s*[:=]\s*["'']?[A-Za-z0-9/+]{16,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|AKIA[0-9A-Z]{16}'
    'personal-path' = '(?i)[A-Z]:[\\/]+(source|Users)[\\/]|quicker-workspace'
    'private-ref'   = '\.\./Quicker\b|\.\./qk-ocr-lite|Quicker\.ScreenCapture|QuickerPc'
    # 公开树只描述 RustShot 自身；参考产品名与私有工程名命中即需改写或移入 private/。
    'reference-name' = '截图\s?Pro|参考实现|参考工程|参考来源|qk-ocr|\bqk\b|screenshot-pro'
    'quicker-name'  = '(?i)quicker'
    # 代码与脚本把 target/qa 当作本机 QA 输出目录是正常的；只有文档里链接过去才是公开后的死链。
    'qa-evidence'   = '\]\((\.\./)*target/qa/'
    'archive-link'  = '\]\((\.\./)*archive/'
}
$textExt = '.rs', '.md', '.toml', '.json', '.ps1', '.py', '.cjs', '.js', '.html', '.css', '.yml', '.yaml', '.txt', '.svg', '.hlsl', '.cs'
$hits = [ordered]@{}
foreach ($name in $rules.Keys) { $hits[$name] = New-Object System.Collections.Generic.List[string] }
foreach ($f in $public) {
    if ($textExt -notcontains [IO.Path]::GetExtension($f).ToLowerInvariant()) { continue }
    $path = Join-Path $out $f
    if (-not (Test-Path -LiteralPath $path)) { continue }
    $n = 0
    foreach ($line in [IO.File]::ReadLines($path)) {
        $n++
        foreach ($name in $rules.Keys) {
            if ($line -match $rules[$name]) { $hits[$name].Add("${f}:$n") }
        }
    }
}

# Markdown 相对链接必须指向导出树内存在的文件（锚点和外链不检查）。
$hits['broken-link'] = New-Object System.Collections.Generic.List[string]
foreach ($f in $public | Where-Object { $_ -like '*.md' }) {
    $path = Join-Path $out $f
    $dir = Split-Path $path
    $n = 0
    foreach ($line in [IO.File]::ReadLines($path)) {
        $n++
        foreach ($m in [regex]::Matches($line, '\]\(([^)\s]+)\)')) {
            $link = $m.Groups[1].Value
            if ($link -match '^(<|https?:|mailto:|#)') { continue }
            $target = [Uri]::UnescapeDataString(($link -split '#')[0])
            if (-not $target) { continue }
            $target = $target -replace ':\d+(-\d+)?$', ''
            if (-not (Test-Path -LiteralPath (Join-Path $dir $target))) { $hits['broken-link'].Add("${f}:$n -> $link") }
        }
    }
}

$bytes = (Get-ChildItem -LiteralPath $out -Recurse -File | Measure-Object Length -Sum).Sum
$summary = [ordered]@{
    out_dir = $out
    public_files = $public.Count
    public_mib = [math]::Round($bytes / 1MB, 1)
    skipped_private_files = $skipped.Count
    missing_files = $missing
}
foreach ($name in $hits.Keys) {
    $summary["${name}_hits"] = $hits[$name].Count
}
$report = Join-Path $out '..' | Join-Path -ChildPath ((Split-Path $out -Leaf) + '-report.txt')
$lines = @()
foreach ($name in $hits.Keys) {
    $lines += "== $name ($($hits[$name].Count))"
    $lines += $hits[$name]
}
$lines += "== skipped private ($($skipped.Count))"
$lines += $skipped
Set-Content -LiteralPath $report -Value $lines -Encoding utf8
$summary['report'] = [IO.Path]::GetFullPath($report)
$summary | ConvertTo-Json
if ($hits['secret'].Count -gt 0) { Write-Warning '发现疑似密钥，见报告；公开前必须处理。'; exit 2 }
