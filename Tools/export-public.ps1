# 把将来公开仓库应包含的文件导出到本地目录并扫描，供维护者审阅。
# 只复制和检查，不创建仓库、不提交、不推送；是否开源由维护者确认后另行操作。
# 用法：pwsh -File Tools/export-public.ps1 [-ScanOnly] [-OutDir target/public-export] [-IncludeUntracked]
param(
    [string]$OutDir = 'target/public-export',
    # 同时导出未跟踪但未被 .gitignore 忽略的文件（审阅进行中的工作区时使用）。
    [switch]$IncludeUntracked,
    # 直接审查当前工作区的公开文件，不复制源码；报告写入 target/public-export-report.txt。
    [switch]$ScanOnly
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$out = if ([IO.Path]::IsPathRooted($OutDir)) { [IO.Path]::GetFullPath($OutDir) } else { [IO.Path]::GetFullPath((Join-Path $root $OutDir)) }
$targetRoot = [IO.Path]::GetFullPath((Join-Path $root 'target'))

# 只读取本地普通文件；检查每级目录，避免经链接进入云盘或读取占位文件。
function Assert-LocalPath([string]$Path) {
    $candidate = [IO.Path]::GetFullPath($Path)
    if ($candidate -match '(?i)(^|[\\/])(OneDrive[^\\/]*|Dropbox|iCloud[^\\/]*)([\\/]|$)') {
        throw '路径位于云盘目录'
    }
    while ($candidate) {
        if (Test-Path -LiteralPath $candidate) {
            $item = Get-Item -LiteralPath $candidate -Force
            # Offline、RecallOnOpen、RecallOnDataAccess、ReparsePoint。
            if (([long]$item.Attributes -band (0x1000 -bor 0x40000 -bor 0x400000 -bor 0x400)) -ne 0) {
                throw '路径包含云端占位文件或重解析点'
            }
        }
        $parent = Split-Path -Path $candidate -Parent
        if ($parent -eq $candidate) { break }
        $candidate = $parent
    }
}

if (-not $ScanOnly) {
    $separator = [IO.Path]::DirectorySeparatorChar
    $out = $out.TrimEnd($separator, [IO.Path]::AltDirectorySeparatorChar)
    $insideRepo = $out.StartsWith($root + $separator, [StringComparison]::OrdinalIgnoreCase)
    if (-not $out -or $out -eq [IO.Path]::GetPathRoot($out).TrimEnd($separator) -or
        $out -eq $root -or $root.StartsWith($out + $separator, [StringComparison]::OrdinalIgnoreCase) -or
        $out -eq $targetRoot -or ($insideRepo -and -not $out.StartsWith($targetRoot + $separator, [StringComparison]::OrdinalIgnoreCase))) {
        throw "导出目录不能是仓库根、其祖先或 target 根；仓库内须位于 target 的子目录：$out"
    }
    Assert-LocalPath $out
    if (Test-Path -LiteralPath $out) {
        if (-not (Test-Path -LiteralPath $out -PathType Container) -or
            @(Get-ChildItem -LiteralPath $out -Force | Select-Object -First 1).Count -gt 0) {
            throw "导出目录须为新目录或空目录，不自动删除已有内容：$out"
        }
    }
}

# 公开源码、开发工具和完整项目文档；本机配置与原始个人记录不在公开范围。
$publicPrefixes = @(
    'src/',
    'crates/',
    'assets/',
    'dependencies/',
    'tests/',
    'fixtures/',
    'vendor/',
    'docs/',
    'Tools/',
    'quicker/',
    '.github/'
)
$publicFiles = @(
    '.gitattributes',
    '.gitignore',
    'Cargo.toml',
    'Cargo.lock',
    'build.rs',
    'build.ps1',
    'README.md',
    'README.zh-CN.md',
    'AGENTS.md',
    'CLAUDE.md',
    'LICENSE',
    'THIRD_PARTY_NOTICES.md',
    'CHANGELOG.md',
    'CONTRIBUTING.md',
    'SECURITY.md',
    '.cargo/config.toml',
    '.github/workflows/release.yml',
    '.github/pull_request_template.md',
    'Tools/package.ps1',
    'Tools/render-tray-icon.py',
    'benchmark.ps1',
    'verify.ps1',
    'package-quicker.ps1',
    'install-quicker-action.ps1'
)

Push-Location $root
try {
    $files = @(git -c core.quotepath=false ls-files)
    if ($LASTEXITCODE -ne 0) { throw '无法读取 Git 跟踪文件列表' }
    if ($IncludeUntracked) {
        $files += @(git -c core.quotepath=false ls-files --others --exclude-standard)
        if ($LASTEXITCODE -ne 0) { throw '无法读取 Git 未跟踪文件列表' }
    }
} finally { Pop-Location }
$files = $files | Where-Object { $_ } | Sort-Object -Unique
$public = @()
$skipped = @()
foreach ($f in $files) {
    $included = $publicFiles -contains $f -or @($publicPrefixes | Where-Object {
        $f.StartsWith($_, [StringComparison]::OrdinalIgnoreCase)
    }).Count -gt 0
    $excluded = ($f -notin @('AGENTS.md', 'CLAUDE.md') -and $f -match '(^|/)(AGENTS\.md|CLAUDE\.md|\.claude|\.codex|\.cursor)(/|$)') -or
        $f.StartsWith('vendor/gpui/examples/', [StringComparison]::OrdinalIgnoreCase)
    if ($included -and -not $excluded) { $public += $f } else { $skipped += $f }
}

$missing = @()
$blocked = @()
$available = @()
$bytes = 0L
foreach ($f in $public) {
    $src = Join-Path $root $f
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { $missing += $f; continue }
    try { Assert-LocalPath $src } catch { $blocked += $f; continue }
    $available += $f
    $bytes += (Get-Item -LiteralPath $src).Length
}
if (-not $ScanOnly -and $missing.Count -eq 0 -and $blocked.Count -eq 0) {
    New-Item -ItemType Directory -Force -Path $out | Out-Null
    foreach ($f in $available) {
        $src = Join-Path $root $f
        $dst = Join-Path $out $f
        New-Item -ItemType Directory -Force -Path (Split-Path $dst) | Out-Null
        Copy-Item -LiteralPath $src -Destination $dst
    }
}
# 真实导出检查已复制的文件，避免并行开发使源文件与报告对应的内容不同。
# 预检失败时没有生成导出树，仍检查可安全读取的原文件并报告预检错误。
$scanRoot = if ($ScanOnly -or $missing.Count -gt 0 -or $blocked.Count -gt 0) { $root } else { $out }

# 上游源码与上游文档不作本机内容审查，仍原样导出；单独核对其许可证和通知。
$scanFiles = @($available | Where-Object {
    $_ -notlike 'vendor/*' -or $_ -eq 'vendor/README.md' -or $_ -like 'vendor/gpui/LICENSE*'
})

# 扫描规则：命中即列出“文件:行”，不打印匹配到的值本身，避免把疑似密钥写进日志。
$rules = [ordered]@{
    'secret'        = '(?i:(BITIFUL_(ACCESS|SECRET)_KEY|aws_secret_access_key|access_?key_?secret)\s*[:=]\s*["'']?[A-Za-z0-9/+]{16,})|-----BEGIN [A-Z ]*PRIVATE KEY-----|(?-i:(?<![A-Za-z0-9/+])AKIA[0-9A-Z]{16}(?![A-Za-z0-9/+]))'
    'personal-path' = '(?i)[A-Z]:[\\/]+(source|Users)[\\/]|quicker-workspace'
    'private-ref'   = '\.\./Quicker\b|\.\./qk-ocr-lite|Quicker\.ScreenCapture|QuickerPc'
    # 标记参考工程与可选集成的提及供人工核对；公开研究中的来源名称不等同于凭据。
    'reference-name' = '截图\s?Pro|参考实现|参考工程|参考来源|qk-ocr|\bqk\b|screenshot-pro'
    'quicker-name'  = '(?i)quicker'
    # 代码与脚本把 target/qa 当作本机 QA 输出目录是正常的；只有文档里链接过去才是公开后的死链。
    'qa-evidence'   = '\]\((\.\./)*target/qa/'
    'archive-link'  = '\]\((\.\./)*archive/'
}
$textExt = '.rs', '.md', '.toml', '.json', '.ps1', '.py', '.cjs', '.js', '.html', '.css', '.yml', '.yaml', '.txt', '.svg', '.hlsl', '.cs'
$hits = [ordered]@{}
foreach ($name in $rules.Keys) { $hits[$name] = New-Object System.Collections.Generic.List[string] }
foreach ($f in $scanFiles) {
    if ($textExt -notcontains [IO.Path]::GetExtension($f).ToLowerInvariant()) { continue }
    $path = Join-Path $scanRoot $f
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
foreach ($f in $scanFiles | Where-Object { $_ -like '*.md' }) {
    $path = Join-Path $scanRoot $f
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
            $resolved = [IO.Path]::GetFullPath((Join-Path $dir $target))
            $prefix = $scanRoot + [IO.Path]::DirectorySeparatorChar
            $relative = if ($resolved.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
                $resolved.Substring($prefix.Length).Replace('\', '/')
            } else { $null }
            # 目标须在公开树内；仓库本机存在的私有文件同样属于公开后的断链。
            $present = $relative -and ($available -contains $relative -or @($available | Where-Object { $_.StartsWith($relative.TrimEnd('/') + '/', [StringComparison]::OrdinalIgnoreCase) }).Count -gt 0)
            if (-not $present) { $hits['broken-link'].Add("${f}:$n -> $link") }
        }
    }
}

$summary = [ordered]@{
    scan_only = [bool]$ScanOnly
    out_dir = if ($ScanOnly) { $null } else { $out }
    public_files = $public.Count
    public_mib = [math]::Round($bytes / 1MB, 1)
    skipped_private_files = $skipped.Count
    missing_files = $missing
    blocked_files = $blocked
    third_party_files_not_content_scanned = $available.Count - $scanFiles.Count
}
foreach ($name in $hits.Keys) {
    $summary["${name}_hits"] = $hits[$name].Count
}
$report = if ($ScanOnly) { Join-Path $targetRoot 'public-export-report.txt' } else {
    Join-Path (Split-Path $out -Parent) ((Split-Path $out -Leaf) + '-report.txt')
}
Assert-LocalPath $report
New-Item -ItemType Directory -Force -Path (Split-Path $report -Parent) | Out-Null
$lines = @()
foreach ($name in $hits.Keys) {
    $lines += "== $name ($($hits[$name].Count))"
    $lines += $hits[$name]
}
$lines += "== skipped private ($($skipped.Count))"
$lines += $skipped
$lines += "== missing ($($missing.Count))"
$lines += $missing
$lines += "== blocked ($($blocked.Count))"
$lines += $blocked
Set-Content -LiteralPath $report -Value $lines -Encoding utf8
$summary['report'] = [IO.Path]::GetFullPath($report)
$summary | ConvertTo-Json
if ($hits['secret'].Count -gt 0 -or $missing.Count -gt 0 -or $blocked.Count -gt 0 -or $hits['broken-link'].Count -gt 0) {
    Write-Warning '发现疑似密钥、缺失/占位文件或公开断链，见报告；公开前必须处理。'
    exit 2
}
