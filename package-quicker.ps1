#requires -Version 5.1
<#
.SYNOPSIS
按 Quicker 动作默认值调用 Tools/package.ps1，生成可下载的便携依赖。
.DESCRIPTION
通用打包逻辑在 Tools/package.ps1；本脚本只提供 Quicker 包名、动作版本、
说明文件、本机 VC++ 运行库目录与暂存目录的默认值。参数与旧版保持一致。
.EXAMPLE
.\package-quicker.ps1
.EXAMPLE
.\package-quicker.ps1 -SourceDirectory target/iteration-next/release -PackageVersion 0.4.2
#>
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()][string]$SourceDirectory = 'target/release',
    [ValidateNotNullOrEmpty()][string]$OutputDirectory = 'dist/quicker',
    [ValidatePattern('^[a-z0-9][a-z0-9._-]*$')][string]$PackageName = 'cea.rustshot',
    [ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')]
    [string]$PackageVersion = ((Get-Content -LiteralPath (Join-Path $PSScriptRoot 'quicker/action/data.json') -Raw | ConvertFrom-Json).variables | Where-Object key -eq 'package_version' | ForEach-Object default),
    [ValidateNotNullOrEmpty()][string]$RuntimeDirectory = 'C:\Program Files\Microsoft Visual Studio\18\Community\VC\Redist\MSVC\14.51.36231\x64\Microsoft.VC145.CRT',
    [string]$OcrRuntimeDirectory = '',
    [ValidateNotNullOrEmpty()][string]$ReadmePath = 'quicker/PORTABLE-README.txt',
    [switch]$IncludeProjectNotices
)

$ErrorActionPreference = 'Stop'
$taskArguments = @{
    SourceDirectory = $SourceDirectory
    OutputDirectory = $OutputDirectory
    PackageName = $PackageName
    PackageVersion = $PackageVersion
    RuntimeDirectory = $RuntimeDirectory
    OcrRuntimeDirectory = $OcrRuntimeDirectory
    ReadmePath = $ReadmePath
    StagingDirectory = 'target/qa/quicker-package'
    IncludeProjectNotices = $IncludeProjectNotices
}
& (Join-Path $PSScriptRoot 'Tools/package.ps1') @taskArguments
