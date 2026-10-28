#requires -Version 7.0
<#
.SYNOPSIS
准备 RaiShot Quicker 依赖；仅显式 -Upload 时上传，默认私有。
.DESCRIPTION
只打包已有 Release 产物，不构建、不改版本、不更新或发布动作。
-PreparedZip 可复用已准备包；两条入口都完整核验 ZIP，再生成 .sha256。
.EXAMPLE
pwsh ./quicker/Publish-Dependency.ps1 -PackageVersion 0.4.9
.EXAMPLE
pwsh ./quicker/Publish-Dependency.ps1 -PackageVersion 0.4.9 -PreparedZip target/package.zip -Upload -Public
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')]
    [string]$PackageVersion,
    [string]$SourceDirectory = 'target/release',
    [string]$OutputDirectory,
    [string]$RuntimeDirectory,
    [string]$OcrRuntimeDirectory,
    [string]$QuickerCli,
    [switch]$Upload,
    [switch]$Public,
    [string]$PreparedZip
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.Net.Http
$publishRoot = Split-Path -Parent $PSScriptRoot

function Resolve-PublishPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path) -or $Path -match '^[A-Za-z]:[^\\/]') {
        throw '请指定明确的文件或目录路径。'
    }
    if (![IO.Path]::IsPathRooted($Path)) { $Path = Join-Path $publishRoot $Path }
    return [IO.Path]::GetFullPath($Path)
}

function Assert-PublishLocalPath([string]$Path) {
    for ($candidate = $Path; $candidate; $candidate = [IO.Path]::GetDirectoryName($candidate)) {
        if (Test-Path -LiteralPath $candidate) {
            # 不读取云端占位文件，也不通过重解析路径写入或清理。
            if (([IO.File]::GetAttributes($candidate) -band 0x441400) -ne 0) {
                throw "请使用普通本地路径：$candidate"
            }
        }
    }
}

function Get-PublishStreamHash([IO.Stream]$Stream) {
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($hasher.ComputeHash($Stream)).Replace('-', '').ToLowerInvariant() }
    finally { $hasher.Dispose() }
}

function Test-PreparedPackage([IO.Stream]$Stream) {
    $archive = [IO.Compression.ZipArchive]::new($Stream, [IO.Compression.ZipArchiveMode]::Read, $true)
    try {
        $entries = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($entry in $archive.Entries) {
            # 当前包只含根目录文件；拒绝目录、路径穿越、ADS 及大小写重复项。
            if ($entry.FullName -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$' -or
                $entry.FullName.EndsWith('.') -or !$entries.TryAdd($entry.FullName, $entry)) {
                throw "ZIP 条目重复或路径无效：$($entry.FullName)"
            }
        }
        if (!$entries.ContainsKey('manifest.json') -or $entries['manifest.json'].Length -gt 1048576) {
            throw '缺少有效的 manifest.json。'
        }
        $reader = [IO.StreamReader]::new($entries['manifest.json'].Open(), [Text.UTF8Encoding]::new($false, $true), $true)
        try { $manifest = $reader.ReadToEnd() | ConvertFrom-Json }
        finally { $reader.Dispose() }
        if ($manifest.schema_version -ne 1 -or $manifest.package_name -cne 'cea.rustshot' -or
            $manifest.package_version -cne $PackageVersion -or $manifest.architecture -cne 'windows-x64') {
            throw '依赖清单的格式、包名、版本或架构不符。'
        }
        $records = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($file in $manifest.files) {
            if ($file.path -isnot [string] -or $file.path -ieq 'manifest.json' -or
                !$entries.ContainsKey($file.path) -or !$records.TryAdd($file.path, $file) -or
                $file.sha256 -isnot [string] -or $file.sha256 -notmatch '^[a-fA-F0-9]{64}$' -or
                ($file.bytes -isnot [long] -and $file.bytes -isnot [int]) -or $file.bytes -lt 0) {
                throw '文件清单存在缺失、重复或无效记录。'
            }
            $entry = $entries[$file.path]
            $payload = $entry.Open()
            try {
                if ($entry.Length -ne $file.bytes -or (Get-PublishStreamHash $payload) -ine $file.sha256) {
                    throw "文件大小或 SHA-256 不符：$($file.path)"
                }
            } finally { $payload.Dispose() }
        }
        if ($entries.Count -ne $records.Count + 1) { throw 'ZIP 包含未列入清单的文件。' }
        $vcFiles = @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'msvcp140_1.dll')
        $ocr = Get-Content -LiteralPath (Join-Path $publishRoot 'dependencies/ppocr-cpu.json') -Raw | ConvertFrom-Json
        foreach ($name in @('raishot.exe', 'raishot-gpui.exe', 'raishot-cli.exe', 'LICENSE', 'THIRD_PARTY_NOTICES.md') + $vcFiles) {
            if (!$records.ContainsKey($name)) { throw "依赖包缺少必需文件：$name" }
        }
        foreach ($group in @(@{ actual=@($manifest.runtime.files); expected=$vcFiles },
                @{ actual=@($manifest.ocr_runtime.files); expected=@($ocr.runtime.files.name) })) {
            $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
            foreach ($name in $group.actual) {
                if ($name -isnot [string] -or !$records.ContainsKey($name) -or !$seen.Add($name)) {
                    throw '运行库文件引用缺失或重复。'
                }
            }
            foreach ($name in $group.expected) {
                if (!$seen.Contains($name)) { throw "运行库清单缺少必需文件：$name" }
            }
        }
        foreach ($file in $ocr.runtime.files) {
            $record = $records[$file.name]
            if ($record.bytes -ne $file.bytes -or $record.sha256 -ine $file.sha256) {
                throw "ONNX Runtime 与固定清单不符：$($file.name)"
            }
        }
        return $manifest
    } finally { $archive.Dispose(); $Stream.Position = 0 }
}

function Resolve-QuickerCli {
    if ($QuickerCli) { $path = Resolve-PublishPath $QuickerCli }
    else {
        $command = Get-Command quicker -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        $path = if ($command) { $command.Source } else { Join-Path $env:LOCALAPPDATA 'quicker-cli/bin/quicker.exe' }
    }
    Assert-PublishLocalPath $path
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw '请安装 quicker CLI 并登录，或传入 -QuickerCli。' }
    return $path
}

function Invoke-PublishCli([string]$Cli, [string[]]$Arguments) {
    $process = [Diagnostics.Process]::new()
    try {
        $process.StartInfo = [Diagnostics.ProcessStartInfo]::new($Cli)
        $process.StartInfo.UseShellExecute = $false
        $process.StartInfo.CreateNoWindow = $true
        $process.StartInfo.RedirectStandardOutput = $true
        $process.StartInfo.RedirectStandardError = $true
        foreach ($argument in $Arguments) { $process.StartInfo.ArgumentList.Add($argument) }
        [void]$process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(180000)) {
            $process.Kill($true)
            throw 'quicker CLI 超时，结果未知；请先核对线上版本，不要直接重试上传。'
        }
        $errorText = $stderr.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) {
            throw ('quicker CLI 失败（' + ($Arguments[0..1] -join ' ') + '，退出码 ' + $process.ExitCode + '）：' + $errorText.Trim() + ' 未自动重试。')
        }
        return $stdout.GetAwaiter().GetResult()
    } finally { $process.Dispose() }
}

function Find-PublishedPackage([string]$Cli) {
    $response = (Invoke-PublishCli $Cli @('dep', 'find', '--name', 'cea.rustshot', '--version', $PackageVersion)) | ConvertFrom-Json
    if ($response.isSuccess -eq $true) {
        if ($response.data.name -cne 'cea.rustshot' -or $response.data.version -cne $PackageVersion -or
            @($response.data.downloadLinks).Count -eq 0) {
            throw '查询未返回目标精确版本或下载地址；较新版不能证明目标版本已存在。'
        }
        return $response.data
    }
    if ($response.isSuccess -eq $false -and $response.errorCode -eq 1 -and $null -eq $response.data -and
        @('未找到符合条件的包', '未找到符合版本要求的包。') -ccontains $response.message) { return $null }
    throw '无法确认目标版本是否存在，停止上传。'
}

function Test-PublishedPackage([object]$Package, [long]$Bytes, [string]$Sha256) {
    $uri = [Uri]::new([string]$Package.downloadLinks[0], [UriKind]::Absolute)
    if ($uri.Scheme -cne 'https') { throw '公开下载地址必须使用 HTTPS。' }
    $downloadRoot = Resolve-PublishPath 'target/quicker-publish'
    Assert-PublishLocalPath $downloadRoot
    [void][IO.Directory]::CreateDirectory($downloadRoot)
    $download = Join-Path $downloadRoot ([Guid]::NewGuid().ToString('N') + '.zip')
    # 全新 HttpClient 不带登录、Cookie 或 Authorization，验证匿名完整下载。
    $client = [Net.Http.HttpClient]::new()
    $timeout = [Threading.CancellationTokenSource]::new([TimeSpan]::FromMinutes(3))
    $response = $null
    try {
        $response = $client.GetAsync($uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $timeout.Token).GetAwaiter().GetResult()
        [void]$response.EnsureSuccessStatusCode()
        $downloadStream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $output = [IO.File]::Open($download, [IO.FileMode]::CreateNew)
        try { [void]$downloadStream.CopyToAsync($output, 81920, $timeout.Token).GetAwaiter().GetResult() }
        finally { $output.Dispose(); $downloadStream.Dispose() }
        if ((Get-Item -LiteralPath $download).Length -ne $Bytes -or
            (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash -ine $Sha256) {
            throw '线上完整 ZIP 的大小或 SHA-256 不同，拒绝覆盖或继续发布。'
        }
        return $uri.AbsoluteUri
    } finally {
        if ($response) { $response.Dispose() }
        $timeout.Dispose()
        $client.Dispose()
        if ([IO.File]::Exists($download)) { [IO.File]::Delete($download) }
    }
}

if ($Public -and !$Upload) { throw '-Public 必须与 -Upload 同用；默认仅准备，上传默认私有。' }
if (!$OutputDirectory) { $OutputDirectory = "dist/quicker/releases/$PackageVersion" }
if ($PreparedZip) { $zipPath = Resolve-PublishPath $PreparedZip }
else {
    $arguments = @{ PackageVersion=$PackageVersion; SourceDirectory=$SourceDirectory
        OutputDirectory=$OutputDirectory; IncludeProjectNotices=$true }
    if ($RuntimeDirectory) { $arguments.RuntimeDirectory = $RuntimeDirectory }
    if ($OcrRuntimeDirectory) { $arguments.OcrRuntimeDirectory = $OcrRuntimeDirectory }
    $prepared = & (Join-Path $publishRoot 'package-quicker.ps1') @arguments
    $zipPath = [string]$prepared.zip_path
}
Assert-PublishLocalPath $zipPath
$zipStream = [IO.File]::Open($zipPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
try {
    $manifest = Test-PreparedPackage $zipStream
    $sha256 = Get-PublishStreamHash $zipStream
    $checksumPath = $zipPath + '.sha256'
    Assert-PublishLocalPath $checksumPath
    $checksum = "$sha256  $([IO.Path]::GetFileName($zipPath))"
    if ([IO.File]::Exists($checksumPath)) {
        if ([IO.File]::ReadAllText($checksumPath).Trim() -cne $checksum) { throw '已有 .sha256 内容不同，未覆盖。' }
    } else { [IO.File]::WriteAllText($checksumPath, "$checksum`n", [Text.UTF8Encoding]::new($false)) }
    $status = 'prepared'
    $downloadUrl = $null
    if ($Upload) {
        $cli = Resolve-QuickerCli
        $existing = Find-PublishedPackage $cli
        if ($existing) {
            $downloadUrl = Test-PublishedPackage $existing $zipStream.Length $sha256
            $status = 'already-present'
        } else {
            $uploadArguments = @('dep', 'upload', '--file', $zipPath, '--name', 'cea.rustshot',
                '--version', $PackageVersion, '--description', 'RaiShot 独立截图程序。')
            if ($Public) { $uploadArguments += '--public' }
            [void](Invoke-PublishCli $cli $uploadArguments)
            $existing = Find-PublishedPackage $cli
            if (!$existing) { throw '上传结果尚未确认；未自动重试，请先查询线上状态。' }
            $downloadUrl = Test-PublishedPackage $existing $zipStream.Length $sha256
            $status = 'uploaded'
        }
    }
    [pscustomobject]@{ status=$status; package_name='cea.rustshot'; package_version=$PackageVersion
        product_version=$manifest.product_version; zip_path=$zipPath; bytes=$zipStream.Length
        sha256=$sha256; checksum_path=$checksumPath; public_requested=[bool]$Public; download_url=$downloadUrl }
} finally { $zipStream.Dispose() }
