#requires -Version 5.1
<#
.SYNOPSIS
将已有的 Windows x64 Release 产物打包为便携 ZIP。
.DESCRIPTION
只读取产物，不构建、不运行程序、不上传。ZIP 根目录放置三个 EXE、
app-local VC++ 运行库（vcruntime140／vcruntime140_1／msvcp140／msvcp140_1）、
ONNX Runtime（按 dependencies/ppocr-cpu.json 固定 bytes／SHA 核对）和 manifest.json；
只有 OCR 模型与字典在运行时按需下载。相同包版本只允许复用相同内容，
内容变化必须提高 PackageVersion。相对路径都相对仓库根目录。
.EXAMPLE
pwsh Tools/package.ps1 -IncludeProjectNotices
.EXAMPLE
pwsh Tools/package.ps1 -SourceDirectory target/release -PackageVersion 0.4.2 -RuntimeDirectory <VC CRT 目录>
#>
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()][string]$SourceDirectory = 'target/release',
    [ValidateNotNullOrEmpty()][string]$OutputDirectory = 'dist/package',
    [ValidatePattern('^[a-z0-9][a-z0-9._-]*$')][string]$PackageName = 'rustshot',
    # 留空时使用 Cargo.toml 的程序版本。
    [ValidatePattern('^((0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))?$')]
    [string]$PackageVersion = '',
    # app-local VC++ 运行库目录；留空时用 vswhere 查找本机 Visual Studio 的 x64 CRT。
    [string]$RuntimeDirectory = '',
    # 已按固定清单核对的 ONNX Runtime 文件目录；默认取本机 OCR 缓存（运行一次 OCR 即生成）。
    [string]$OcrRuntimeDirectory = '',
    [ValidateNotNullOrEmpty()][string]$ReadmePath = 'assets/PORTABLE-README.txt',
    [ValidateNotNullOrEmpty()][string]$StagingDirectory = 'target/package-staging',
    [switch]$IncludeProjectNotices
)

$ErrorActionPreference = 'Stop'
# 对外品牌：与 src/brand.rs、Cargo.toml 的 [[bin]] 一致；内部代号 rustshot 不随品牌改名。
$taskBrand = 'RaiShot'
# 按最近曾用名优先，避免先选到更早品牌遗留的不完整缓存。
$taskLegacyBrands = @('RabShot', 'Kirie', 'RustShot')
$taskRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression

function Get-TaskFullPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    if ($Path -match '^[A-Za-z]:[^\\/]') {
        throw "不接受依赖当前目录的驱动器相对路径：$Path"
    }
    if (-not [System.IO.Path]::IsPathRooted($Path)) {
        $Path = Join-Path $taskRoot $Path
    }
    return [System.IO.Path]::GetFullPath($Path)
}

function Assert-NoReparseAncestor {
    param([Parameter(Mandatory = $true)][string]$Path)
    # 写入和清理前拒绝符号链接、junction 等重解析路径，避免越出指定目录。
    $taskCurrent = $Path
    while (-not [string]::IsNullOrEmpty($taskCurrent)) {
        if (Test-Path -LiteralPath $taskCurrent) {
            $taskItem = Get-Item -LiteralPath $taskCurrent -Force
            if (($taskItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "路径包含重解析点，请指定普通目录：$taskCurrent"
            }
        }
        $taskParent = [System.IO.Directory]::GetParent($taskCurrent)
        if ($null -eq $taskParent) { break }
        $taskCurrent = $taskParent.FullName
    }
}

function Assert-X64Pe {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][bool]$IsDll
    )
    $taskStream = [System.IO.File]::OpenRead($Path)
    $taskReader = New-Object System.IO.BinaryReader($taskStream)
    try {
        if ($taskStream.Length -lt 64 -or $taskReader.ReadUInt16() -ne 0x5A4D) {
            throw "不是有效的 PE 文件：$Path"
        }
        $taskStream.Position = 0x3C
        $taskPeOffset = $taskReader.ReadInt32()
        if ($taskPeOffset -lt 64 -or $taskPeOffset -gt ($taskStream.Length - 26)) {
            throw "PE 头偏移无效：$Path"
        }
        $taskStream.Position = $taskPeOffset
        if ($taskReader.ReadUInt32() -ne 0x00004550 -or $taskReader.ReadUInt16() -ne 0x8664) {
            throw "产物必须是 Windows x64 PE：$Path"
        }
        $taskStream.Position = $taskPeOffset + 20
        $taskOptionalHeaderSize = $taskReader.ReadUInt16()
        $taskCharacteristics = $taskReader.ReadUInt16()
        if ($taskOptionalHeaderSize -lt 112 -or
            ($taskPeOffset + 24 + $taskOptionalHeaderSize) -gt $taskStream.Length -or
            $taskReader.ReadUInt16() -ne 0x020B -or
            ($taskCharacteristics -band 0x0002) -eq 0) {
            throw "缺少有效的 x64 可执行 PE 头：$Path"
        }
        if ((($taskCharacteristics -band 0x2000) -ne 0) -ne $IsDll) {
            throw "PE 文件类型与扩展名不符：$Path"
        }
    } finally {
        $taskReader.Dispose()
        $taskStream.Dispose()
    }
}

function Get-TaskStreamHash {
    param([Parameter(Mandatory = $true)][System.IO.Stream]$Stream)
    $taskHasher = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([System.BitConverter]::ToString($taskHasher.ComputeHash($Stream))).Replace('-', '').ToLowerInvariant()
    } finally {
        $taskHasher.Dispose()
    }
}

function Get-TaskFileRecord {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$PackagePath
    )
    $taskStream = [System.IO.File]::OpenRead($Path)
    try {
        return [ordered]@{
            path = $PackagePath
            bytes = $taskStream.Length
            sha256 = Get-TaskStreamHash -Stream $taskStream
        }
    } finally {
        $taskStream.Dispose()
    }
}

function Test-TaskJsonEquivalent {
    param([object]$Left, [object]$Right)
    # JSON 对象字段顺序和排版不影响内容；数组顺序、字段名、值与新增项仍严格比较。
    if ($null -eq $Left -or $null -eq $Right) {
        return ($null -eq $Left -and $null -eq $Right)
    }
    if ($Left -is [string] -or $Right -is [string]) {
        return ($Left -is [string] -and $Right -is [string] -and
            [string]::Equals($Left, $Right, [System.StringComparison]::Ordinal))
    }
    if ($Left -is [bool] -or $Right -is [bool]) {
        return ($Left -is [bool] -and $Right -is [bool] -and $Left -eq $Right)
    }
    $taskNumberTypes = @('System.Byte', 'System.SByte', 'System.Int16', 'System.UInt16',
        'System.Int32', 'System.UInt32', 'System.Int64', 'System.UInt64',
        'System.Single', 'System.Double', 'System.Decimal')
    $taskLeftNumber = $taskNumberTypes -contains $Left.GetType().FullName
    $taskRightNumber = $taskNumberTypes -contains $Right.GetType().FullName
    if ($taskLeftNumber -or $taskRightNumber) {
        if (-not $taskLeftNumber -or -not $taskRightNumber) { return $false }
        # PS 5.1 与 7 的 JSON 整数类型不同；清单内的版本和文件大小按数值比较。
        try { return ([decimal]$Left -eq [decimal]$Right) }
        catch { return $false }
    }
    if ($Left -is [System.Array] -or $Right -is [System.Array]) {
        if (-not ($Left -is [System.Array]) -or -not ($Right -is [System.Array]) -or
            $Left.Count -ne $Right.Count) { return $false }
        for ($taskIndex = 0; $taskIndex -lt $Left.Count; $taskIndex++) {
            if (-not (Test-TaskJsonEquivalent -Left $Left[$taskIndex] -Right $Right[$taskIndex])) {
                return $false
            }
        }
        return $true
    }
    if ($Left -is [System.Management.Automation.PSCustomObject] -and
        $Right -is [System.Management.Automation.PSCustomObject]) {
        $taskLeftProperties = @($Left.PSObject.Properties)
        $taskRightProperties = @($Right.PSObject.Properties)
        if ($taskLeftProperties.Count -ne $taskRightProperties.Count) { return $false }
        foreach ($taskLeftProperty in $taskLeftProperties) {
            $taskMatches = @($taskRightProperties | Where-Object {
                [string]::Equals($_.Name, $taskLeftProperty.Name, [System.StringComparison]::Ordinal)
            })
            if ($taskMatches.Count -ne 1 -or
                -not (Test-TaskJsonEquivalent -Left $taskLeftProperty.Value -Right $taskMatches[0].Value)) {
                return $false
            }
        }
        return $true
    }
    return $false
}

function Test-TaskZipContent {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][System.Collections.Generic.Dictionary[string,object]]$ExpectedFiles,
        [Parameter(Mandatory = $true)][object]$ExpectedManifest
    )
    $taskStream = $null
    $taskArchive = $null
    try {
        $taskStream = [System.IO.File]::OpenRead($Path)
        $taskArchive = New-Object System.IO.Compression.ZipArchive($taskStream, [System.IO.Compression.ZipArchiveMode]::Read, $false)
        if ($taskArchive.Entries.Count -ne $ExpectedFiles.Count) { return $false }
        $taskSeen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
        foreach ($taskEntry in $taskArchive.Entries) {
            # 不解压已有包；完整比对根目录文件名，拒绝重复条目与额外目录。
            if (-not $ExpectedFiles.ContainsKey($taskEntry.FullName) -or -not $taskSeen.Add($taskEntry.FullName)) {
                return $false
            }
            $taskExpected = $ExpectedFiles[$taskEntry.FullName]
            if ($taskEntry.FullName -ceq 'manifest.json') {
                $taskManifestStream = $taskEntry.Open()
                $taskManifestReader = $null
                try {
                    $taskStrictUtf8 = New-Object System.Text.UTF8Encoding($false, $true)
                    $taskManifestReader = New-Object System.IO.StreamReader($taskManifestStream, $taskStrictUtf8, $true)
                    $taskExistingManifest = $taskManifestReader.ReadToEnd() | ConvertFrom-Json
                    if (-not (Test-TaskJsonEquivalent -Left $taskExistingManifest -Right $ExpectedManifest)) {
                        return $false
                    }
                } finally {
                    if ($null -ne $taskManifestReader) { $taskManifestReader.Dispose() }
                    $taskManifestStream.Dispose()
                }
                continue
            }
            if ($taskEntry.Length -ne $taskExpected.bytes) { return $false }
            $taskEntryStream = $taskEntry.Open()
            try {
                if ((Get-TaskStreamHash -Stream $taskEntryStream) -ne $taskExpected.sha256) { return $false }
            } finally {
                $taskEntryStream.Dispose()
            }
        }
        return $true
    } catch {
        # 已有文件无法读取或不是 ZIP，同样禁止覆盖。
        Write-Verbose "已有 ZIP 无法通过内容核对：$($_.Exception.Message)"
        return $false
    } finally {
        if ($null -ne $taskArchive) { $taskArchive.Dispose() }
        if ($null -ne $taskStream) { $taskStream.Dispose() }
    }
}

$taskSource = Get-TaskFullPath -Path $SourceDirectory
$taskOutput = Get-TaskFullPath -Path $OutputDirectory
if ([string]::IsNullOrWhiteSpace($RuntimeDirectory)) {
    $taskVswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $taskVswhere -PathType Leaf)) { throw '未找到 vswhere.exe；请用 -RuntimeDirectory 指定 VC++ x64 CRT 目录。' }
    $taskVsRoot = & $taskVswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath | Select-Object -First 1
    $taskCrt = if ($taskVsRoot) { Get-ChildItem -Path (Join-Path $taskVsRoot 'VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT') -Directory -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1 }
    if ($null -eq $taskCrt) { throw '未找到 Visual Studio 的 VC++ x64 CRT；请用 -RuntimeDirectory 指定。' }
    $RuntimeDirectory = $taskCrt.FullName
}
$taskRuntime = Get-TaskFullPath -Path $RuntimeDirectory
$taskQaBase = Get-TaskFullPath -Path $StagingDirectory
$taskStage = Join-Path $taskQaBase ([Guid]::NewGuid().ToString('N'))
$taskOwnedFiles = New-Object 'System.Collections.Generic.List[string]'
$taskStageCreated = $false


$taskCargoPath = Join-Path $taskRoot 'Cargo.toml'
$taskCargoText = [System.IO.File]::ReadAllText($taskCargoPath)
$taskPackageSection = [regex]::Match($taskCargoText, '(?ms)^\[package\]\s*\r?\n(?<body>.*?)(?=^\[|\z)')
$taskProductVersionMatch = [regex]::Match($taskPackageSection.Groups['body'].Value, '(?m)^version\s*=\s*"(?<version>[^"\r\n]+)"\s*$')
if (-not $taskPackageSection.Success -or -not $taskProductVersionMatch.Success) {
    throw '无法从 Cargo.toml 的 [package] 读取产品版本。'
}
$taskProductVersion = $taskProductVersionMatch.Groups['version'].Value
if ([string]::IsNullOrWhiteSpace($PackageVersion)) { $PackageVersion = $taskProductVersion }
# 包版本为三段；分发 ZIP 文件名使用四段 Windows 版本形式。
try { $taskParsedVersion = [System.Version]::Parse("$PackageVersion.0") }
catch { throw "包版本超出有效范围：$PackageVersion" }
$taskZipName = "$taskBrand.$($taskParsedVersion.ToString(4)).zip"
$taskFinalZip = Join-Path $taskOutput $taskZipName
$taskVcRuntimeFiles = @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'msvcp140_1.dll')
$taskInputs = @(
    @{ name = 'raishot.exe'; path = (Join-Path $taskSource 'raishot.exe'); dll = $false; pe = $true },
    @{ name = 'raishot-gpui.exe'; path = (Join-Path $taskSource 'raishot-gpui.exe'); dll = $false; pe = $true },
    @{ name = 'raishot-cli.exe'; path = (Join-Path $taskSource 'raishot-cli.exe'); dll = $false; pe = $true }
)
foreach ($taskVcName in $taskVcRuntimeFiles) {
    $taskInputs += @{ name = $taskVcName; path = (Join-Path $taskRuntime $taskVcName); dll = $true; pe = $true }
}

# ONNX Runtime 与固定 OCR 清单逐字节一致才打包；程序在 EXE 同目录找到一致副本时不再下载。
$taskOcrManifest = [System.IO.File]::ReadAllText((Join-Path $taskRoot 'dependencies/ppocr-cpu.json')) | ConvertFrom-Json
if ([string]::IsNullOrWhiteSpace($OcrRuntimeDirectory)) {
    $taskOcrRelative = "ocr\{0}\{1}" -f $taskOcrManifest.cache_revision, $taskOcrManifest.runtime.directory
    $OcrRuntimeDirectory = Join-Path (Join-Path $env:LOCALAPPDATA $taskBrand) $taskOcrRelative
    foreach ($taskLegacy in $taskLegacyBrands) {
        # 程序首次以新品牌启动前，缓存仍在曾用名目录下。
        $taskLegacyDirectory = Join-Path (Join-Path $env:LOCALAPPDATA $taskLegacy) $taskOcrRelative
        if (-not (Test-Path -LiteralPath $OcrRuntimeDirectory) -and (Test-Path -LiteralPath $taskLegacyDirectory)) {
            $OcrRuntimeDirectory = $taskLegacyDirectory
        }
    }
}
$taskOcrRuntime = Get-TaskFullPath -Path $OcrRuntimeDirectory
foreach ($taskOcrFile in $taskOcrManifest.runtime.files) {
    $taskOcrPath = Join-Path $taskOcrRuntime $taskOcrFile.name
    if (-not (Test-Path -LiteralPath $taskOcrPath -PathType Leaf)) {
        throw "缺少 ONNX Runtime 文件：$taskOcrPath。请先运行一次 OCR 生成缓存，或用 -OcrRuntimeDirectory 指定已解压目录。"
    }
    $taskOcrRecord = Get-TaskFileRecord -Path $taskOcrPath -PackagePath $taskOcrFile.name
    if ($taskOcrRecord.bytes -ne [int64]$taskOcrFile.bytes -or $taskOcrRecord.sha256 -ne $taskOcrFile.sha256.ToLowerInvariant()) {
        throw "ONNX Runtime 文件与固定清单不符：$taskOcrPath"
    }
    $taskIsDll = $taskOcrFile.name.EndsWith('.dll', [System.StringComparison]::OrdinalIgnoreCase)
    $taskInputs += @{ name = $taskOcrFile.name; path = $taskOcrPath; dll = $taskIsDll; pe = $taskIsDll }
}
$taskReadmePath = Get-TaskFullPath -Path $ReadmePath
if (Test-Path -LiteralPath $taskReadmePath) {
    $taskInputs += @{ name = 'README.txt'; path = $taskReadmePath; dll = $false; pe = $false }
}
if ($IncludeProjectNotices) {
    foreach ($taskNoticeName in @('LICENSE', 'THIRD_PARTY_NOTICES.md')) {
        $taskInputs += @{ name = $taskNoticeName; path = (Join-Path $taskRoot $taskNoticeName); dll = $false; pe = $false }
    }
    $taskInputs += @{ name = 'LICENSE-GPUI-APACHE.txt'; path = (Join-Path $taskRoot 'vendor/gpui/LICENSE-APACHE'); dll = $false; pe = $false }
    $taskInputs += @{ name = 'LICENSE-OmniTrigger.txt'; path = (Join-Path $taskRoot 'vendor/omnitrigger/LICENSE'); dll = $false; pe = $false }
}

foreach ($taskInput in $taskInputs) {
    if (-not (Test-Path -LiteralPath $taskInput.path -PathType Leaf)) {
        throw "缺少打包文件：$($taskInput.path)。请先准备已有 Release 产物与 app-local VC 运行库。"
    }
    Assert-NoReparseAncestor -Path $taskInput.path
    if ($taskInput.pe) { Assert-X64Pe -Path $taskInput.path -IsDll $taskInput.dll }
}
Assert-NoReparseAncestor -Path $taskOutput
Assert-NoReparseAncestor -Path $taskQaBase
if ((Test-Path -LiteralPath $taskOutput) -and -not (Test-Path -LiteralPath $taskOutput -PathType Container)) {
    throw "输出目录路径已被文件占用：$taskOutput"
}

try {
    # 只创建本次 GUID 目录；清理只删除本次明确创建的文件，不递归删除用户文件。
    [void][System.IO.Directory]::CreateDirectory($taskQaBase)
    if (Test-Path -LiteralPath $taskStage) { throw "临时目录意外存在：$taskStage" }
    [void][System.IO.Directory]::CreateDirectory($taskStage)
    $taskStageCreated = $true
    $taskRecords = @()
    $taskExpected = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    foreach ($taskInput in $taskInputs) {
        $taskStagedPath = Join-Path $taskStage $taskInput.name
        [System.IO.File]::Copy($taskInput.path, $taskStagedPath, $false)
        $taskOwnedFiles.Add($taskStagedPath)
        if ($taskInput.pe) { Assert-X64Pe -Path $taskStagedPath -IsDll $taskInput.dll }
        $taskRecord = Get-TaskFileRecord -Path $taskStagedPath -PackagePath $taskInput.name
        $taskRecords += $taskRecord
        $taskExpected.Add($taskInput.name, $taskRecord)
    }

    # 校验实际将被分发的副本，不能仅凭 DLL 名称或开发机安装状态判定依赖有效。
    foreach ($taskVcName in $taskVcRuntimeFiles) {
        $taskRuntimeSignature = Get-AuthenticodeSignature -LiteralPath (Join-Path $taskStage $taskVcName)
        if ($taskRuntimeSignature.Status -ne 'Valid') {
            throw "VC 运行库 $taskVcName 的 Authenticode 必须为 Valid，当前为 $($taskRuntimeSignature.Status)。"
        }
    }
    $taskStagedRuntime = Join-Path $taskStage 'vcruntime140.dll'
    $taskRuntimeVersion = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($taskStagedRuntime).FileVersion
    if ([string]::IsNullOrWhiteSpace($taskRuntimeVersion)) { throw 'VC 运行库缺少文件版本。' }
    $taskManifest = [ordered]@{
        schema_version = 1
        package_name = $PackageName
        package_version = $PackageVersion
        product_version = $taskProductVersion
        architecture = 'windows-x64'
        files = $taskRecords
        runtime = [ordered]@{
            source = 'Microsoft Visual C++ Redistributable (app-local)'
            version = $taskRuntimeVersion
            files = $taskVcRuntimeFiles
            authenticode_status = 'Valid'
        }
        ocr_runtime = [ordered]@{
            source = 'ONNX Runtime (bundled; models download on demand)'
            directory = $taskOcrManifest.runtime.directory
            files = @($taskOcrManifest.runtime.files | ForEach-Object { $_.name })
        }
    }
    $taskManifestPath = Join-Path $taskStage 'manifest.json'
    $taskUtf8 = New-Object System.Text.UTF8Encoding($false)
    $taskManifestJson = ($taskManifest | ConvertTo-Json -Depth 8) + "`n"
    $taskExpectedManifest = $taskManifestJson | ConvertFrom-Json
    [System.IO.File]::WriteAllText($taskManifestPath, $taskManifestJson, $taskUtf8)
    $taskOwnedFiles.Add($taskManifestPath)
    $taskExpected.Add('manifest.json', (Get-TaskFileRecord -Path $taskManifestPath -PackagePath 'manifest.json'))

    $taskReused = $false
    if (Test-Path -LiteralPath $taskFinalZip) {
        Assert-NoReparseAncestor -Path $taskFinalZip
        if (-not (Test-Path -LiteralPath $taskFinalZip -PathType Leaf) -or
            -not (Test-TaskZipContent -Path $taskFinalZip -ExpectedFiles $taskExpected -ExpectedManifest $taskExpectedManifest)) {
            throw "已有同版本包与本次内容不同，禁止覆盖：$taskFinalZip。请提高 PackageVersion。"
        }
        $taskReused = $true
    } else {
        $taskTemporaryZip = Join-Path $taskStage 'package.zip'
        $taskZipStream = [System.IO.File]::Open($taskTemporaryZip, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
        $taskOwnedFiles.Add($taskTemporaryZip)
        $taskZipArchive = $null
        try {
            $taskZipArchive = New-Object System.IO.Compression.ZipArchive($taskZipStream, [System.IO.Compression.ZipArchiveMode]::Create, $true)
            foreach ($taskName in (@($taskInputs | ForEach-Object { $_.name }) + @('manifest.json'))) {
                $taskEntry = $taskZipArchive.CreateEntry($taskName, [System.IO.Compression.CompressionLevel]::Optimal)
                $taskEntry.LastWriteTime = [System.DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [System.TimeSpan]::Zero)
                $taskEntryStream = $taskEntry.Open()
                $taskPayloadStream = [System.IO.File]::OpenRead((Join-Path $taskStage $taskName))
                try { $taskPayloadStream.CopyTo($taskEntryStream) }
                finally { $taskPayloadStream.Dispose(); $taskEntryStream.Dispose() }
            }
        } finally {
            if ($null -ne $taskZipArchive) { $taskZipArchive.Dispose() }
            $taskZipStream.Dispose()
        }
        if (-not (Test-TaskZipContent -Path $taskTemporaryZip -ExpectedFiles $taskExpected -ExpectedManifest $taskExpectedManifest)) {
            throw '新 ZIP 内容核对失败，未输出分发包。'
        }
        Assert-NoReparseAncestor -Path $taskOutput
        [void][System.IO.Directory]::CreateDirectory($taskOutput)
        try {
            # 两参数 Move 不覆盖已有文件，避免并发打包覆盖同版本。
            [System.IO.File]::Move($taskTemporaryZip, $taskFinalZip)
        } catch {
            Assert-NoReparseAncestor -Path $taskFinalZip
            if ((Test-Path -LiteralPath $taskFinalZip -PathType Leaf) -and
                (Test-TaskZipContent -Path $taskFinalZip -ExpectedFiles $taskExpected -ExpectedManifest $taskExpectedManifest)) {
                $taskReused = $true
            } else {
                throw "写入 ZIP 失败；不会覆盖已有文件：$taskFinalZip。$($_.Exception.Message)"
            }
        }
    }
    $taskZipRecord = Get-TaskFileRecord -Path $taskFinalZip -PackagePath $taskZipName
    [pscustomobject]@{
        zip_path = $taskFinalZip
        bytes = $taskZipRecord.bytes
        sha256 = $taskZipRecord.sha256
        package_name = $PackageName
        package_version = $PackageVersion
        product_version = $taskProductVersion
        reused = $taskReused
    }
} finally {
    if ($taskStageCreated) {
        try {
            $taskStageFull = [System.IO.Path]::GetFullPath($taskStage)
            $taskSafePrefix = $taskQaBase.TrimEnd([char[]]@('\', '/')) + [System.IO.Path]::DirectorySeparatorChar
            if (-not $taskStageFull.StartsWith($taskSafePrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
                throw '临时目录越出暂存根目录，停止清理。'
            }
            Assert-NoReparseAncestor -Path $taskStageFull
            foreach ($taskOwnedPath in $taskOwnedFiles) {
                if ([System.IO.File]::Exists($taskOwnedPath)) {
                    Assert-NoReparseAncestor -Path $taskOwnedPath
                    [System.IO.File]::Delete($taskOwnedPath)
                }
            }
            # 非递归删除；若出现未记录文件，保留整个目录并提示，不删除用户文件。
            [System.IO.Directory]::Delete($taskStageFull, $false)
        } catch {
            Write-Warning "本次临时目录未完全清理，保留于 $taskStage。$($_.Exception.Message)"
        }
    }
}
