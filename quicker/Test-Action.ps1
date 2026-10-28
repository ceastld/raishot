#requires -Version 7.0
<#
.SYNOPSIS
离线编译并验证 Quicker 动作的参数路由和依赖包校验表达式。
.DESCRIPTION
不调用 Quicker，不运行 RaiShot，不构建 Rust 工程。测试夹具仅写入
target/quicker-action-tests 下本次新建的目录，保留夹具以便检查失败。
#>
[CmdletBinding()]
param(
    [string]$NewtonsoftPath = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$taskRepo = Split-Path $PSScriptRoot -Parent
$taskFiles = Join-Path $PSScriptRoot 'action/files'
$script:taskPassed = 0
. (Join-Path $PSScriptRoot 'ActionSource.ps1')
$taskSource = Get-RustShotActionSource
$taskConditions = [Collections.Generic.List[string]]::new()

function Get-ConditionCases([object[]]$Steps) {
    foreach ($taskStep in $Steps) {
        if ($taskStep.key -ne 'sys:simpleIf') { continue }
        if ($taskStep.PSObject.Properties['elseSteps'] -and @($taskStep.elseSteps).Count -gt 0) {
            throw 'sys:simpleIf 不执行 elseSteps；请使用独立的反条件分支。'
        }
        $taskCondition = [string]$taskStep.inputs.condition
        if (-not $taskCondition.StartsWith('$=', [StringComparison]::Ordinal)) { throw '启动条件缺少表达式前缀。' }
        $taskIndex = $taskConditions.Count
        $taskConditions.Add($taskCondition)
        $taskBody = $taskCondition.Substring(2).Replace('{launch_args}', 'launchArgs').Replace('{resident_handle}', 'residentHandle')
        "case ${taskIndex}: return ($taskBody);"
        if ($taskStep.PSObject.Properties['ifSteps']) { Get-ConditionCases -Steps $taskStep.ifSteps }
    }
}
$taskConditionCases = (Get-ConditionCases -Steps $taskSource.Data.steps) -join [Environment]::NewLine

if ([string]::IsNullOrWhiteSpace($NewtonsoftPath)) {
    $taskQkrpc = Get-Command qkrpc -ErrorAction SilentlyContinue
    if ($null -ne $taskQkrpc) {
        $taskCandidate = Join-Path (Split-Path $taskQkrpc.Source -Parent) 'Newtonsoft.Json.dll'
        if (Test-Path -LiteralPath $taskCandidate -PathType Leaf) { $NewtonsoftPath = $taskCandidate }
    }
    if ([string]::IsNullOrWhiteSpace($NewtonsoftPath)) {
        $taskLoadedJson = [AppDomain]::CurrentDomain.GetAssemblies() |
            Where-Object { $_.GetName().Name -eq 'Newtonsoft.Json' } | Select-Object -First 1
        if ($null -ne $taskLoadedJson) { $NewtonsoftPath = $taskLoadedJson.Location }
    }
}
if ([string]::IsNullOrWhiteSpace($NewtonsoftPath) -or -not (Test-Path -LiteralPath $NewtonsoftPath -PathType Leaf)) {
    throw '找不到 Newtonsoft.Json.dll；请用 -NewtonsoftPath 指定路径。'
}
$NewtonsoftPath = (Resolve-Path -LiteralPath $NewtonsoftPath).Path

function Read-ActionExpression([string]$Name, [System.Collections.IDictionary]$Replacements) {
    $taskBody = [IO.File]::ReadAllText((Join-Path $taskFiles $Name)).Trim()
    if (-not $taskBody.StartsWith('$=', [StringComparison]::Ordinal)) { throw "$Name 缺少表达式前缀。" }
    $taskBody = $taskBody.Substring(2)
    foreach ($taskKey in $Replacements.Keys) { $taskBody = $taskBody.Replace($taskKey, $Replacements[$taskKey]) }
    return $taskBody
}

$taskValidateBody = Read-ActionExpression 'validate-package.value.txt' @{
    '{package_path}' = 'packagePath'
    '{package_name}' = 'packageName'
    '{package_version}' = 'packageVersion'
}
$taskResolveBody = Read-ActionExpression 'resolve-command.value.txt' @{
    '{quicker_in_param}' = 'input'
    '{launch_mode}' = 'defaultMode'
}
$taskClassName = 'RustShotActionTests_' + [guid]::NewGuid().ToString('N')
$taskCode = @"
using System;
using System.IO;
using System.Linq;
using Newtonsoft.Json.Linq;
public static class $taskClassName
{
    public static string Validate(string packagePath, string packageName, string packageVersion)
    {
$taskValidateBody
    }
    public static string Resolve(string input, string defaultMode)
    {
$taskResolveBody
    }
    public static bool Condition(int index, string launchArgs, string residentHandle)
    {
        switch (index)
        {
$taskConditionCases
            default: throw new InvalidOperationException("Unknown launch condition.");
        }
    }
}
"@
$taskReferences = @((Get-ChildItem -LiteralPath (Join-Path $PSHOME 'ref') -Filter '*.dll' -File).FullName) + @($NewtonsoftPath)
$taskExpressionType = Add-Type -TypeDefinition $taskCode -ReferencedAssemblies $taskReferences -PassThru
$taskResolveMethod = $taskExpressionType.GetMethod('Resolve')

function Assert-Equal([string]$Name, [string]$Expected, [string]$Actual) {
    if ($Actual -cne $Expected) { throw "$Name 失败：预期 '$Expected'，实际 '$Actual'。" }
    $script:taskPassed++
    Write-Host "通过：$Name"
}

function Assert-Rejected([string]$Name, [scriptblock]$Action, [string]$Message) {
    try { $null = & $Action }
    catch {
        if (-not $_.Exception.ToString().Contains($Message, [StringComparison]::Ordinal)) {
            throw "$Name 未按预期原因拒绝：$($_.Exception.Message)"
        }
        $script:taskPassed++
        Write-Host "通过：$Name"
        return
    }
    throw "$Name 失败：本应拒绝输入。"
}

Assert-Equal '动作标题使用小写 raishot' 'raishot' $taskSource.Info.title
$taskColdSteps = $taskSource.Data.steps[2].ifSteps
Assert-Equal 'V1 启动 RaiShot' '$=Path.Combine({package_path}, "raishot.exe")' $taskColdSteps[1].inputs.input
Assert-Equal 'V2 启动表达式与 V1 一致' 'True' ([string]$taskSource.ProgramYaml.Contains("      input: '" + $taskColdSteps[1].inputs.input + "'"))
$taskLookup = $taskSource.Data.steps[1].ifSteps[0]
Assert-Equal '常驻查询不读取进程路径' 'handle' ($taskLookup.outputs.PSObject.Properties.Name -join ',')
Assert-Equal '动作不保存常驻路径变量' '0' ([string]@($taskSource.Data.variables | Where-Object key -eq resident_path).Count)
$taskYamlConditions = @([regex]::Matches($taskSource.ProgramYaml, "(?m)^ +condition: '([^']*)'\r?$") | ForEach-Object { $_.Groups[1].Value })
Assert-Equal 'V1/V2 分支条件一致' ($taskConditions -join "`n") ($taskYamlConditions -join "`n")

# 按简单如果的真实契约执行控制流；不调用依赖下载、窗口消息或 EXE。
function Get-LaunchEffects([object[]]$Steps, [string]$LaunchArgs, [string]$ResidentHandle) {
    foreach ($taskStep in $Steps) {
        if ($taskStep.key -eq 'sys:simpleIf') {
            $taskIndex = $taskConditions.IndexOf([string]$taskStep.inputs.condition)
            if ($taskExpressionType::Condition($taskIndex, $LaunchArgs, $ResidentHandle)) {
                Get-LaunchEffects -Steps $taskStep.ifSteps -LaunchArgs $LaunchArgs -ResidentHandle $ResidentHandle
            }
        } elseif ($taskStep.key -in @('sys:dependencycheck', 'sys:getWindowTitle', 'sys:run', 'sys:sendMessage')) {
            $taskStep.key
        } elseif ($taskStep.key -eq 'sys:assign' -and $taskStep.inputs.PSObject.Properties['input.file'] -and
            $taskStep.inputs.'input.file' -eq 'files/validate-package.value.txt') {
            'validate-package'
        }
    }
}
foreach ($taskCase in @(
    @('无托盘区域截图下载依赖并启动', '--region', '', 'sys:getWindowTitle,sys:dependencycheck,sys:run'),
    @('无托盘启动托盘下载依赖并启动', '--tray', '0', 'sys:getWindowTitle,sys:dependencycheck,sys:run'),
    @('常驻区域截图只查询窗口和发消息', '--region', '123', 'sys:getWindowTitle,sys:sendMessage'),
    @('常驻托盘入口不碰依赖且不启动程序', '--tray', '123', 'sys:getWindowTitle'),
    @('无效句柄不拦截冷启动', '--region', '0', 'sys:getWindowTitle,sys:dependencycheck,sys:run'),
    @('null 句柄不拦截冷启动', '--region', $null, 'sys:getWindowTitle,sys:dependencycheck,sys:run'),
    @('无托盘校验不查窗口且完整校验', 'verify', '', 'sys:dependencycheck,validate-package'),
    @('有托盘校验仍下载校验且不发消息', 'verify', '123', 'sys:dependencycheck,validate-package')
)) {
    $taskEffects = @(Get-LaunchEffects -Steps $taskSource.Data.steps -LaunchArgs $taskCase[1] -ResidentHandle $taskCase[2])
    Assert-Equal $taskCase[0] $taskCase[3] ($taskEffects -join ',')
}

foreach ($taskCase in @(
    @('region', 'region', 'region', '--region'),
    @('tray 大小写与空白', ' TRaY ', 'region', '--tray'),
    @('verify 大小写与空白', "`tVeRiFy`r`n", 'tray', 'verify'),
    @('region 大小写与空白', ' ReGiOn ', 'region', '--region'),
    @('显式 region 覆盖 tray 默认', 'region', 'tray', '--region'),
    @('空参数使用 tray 默认', '', 'tray', '--tray'),
    @('空白参数使用默认并归一化', " `t", ' TRAY ', '--tray'),
    @('null 参数使用 region 默认', $null, 'region', '--region'),
    @('null 参数和默认使用 region', $null, $null, '--region'),
    @('显式命令优先于无效默认', 'region', 'unknown', '--region')
)) {
    # 反射保留真正的 null；PowerShell 直接绑定 string 参数会把 null 转为空字符串。
    Assert-Equal $taskCase[0] $taskCase[3] ($taskResolveMethod.Invoke($null, [object[]]@($taskCase[1], $taskCase[2])))
}
Assert-Rejected '未知参数拒绝' { $taskExpressionType::Resolve('unknown', 'region') } '动作参数只支持'
Assert-Rejected '未知默认拒绝' { $taskExpressionType::Resolve('', 'unknown') } '动作参数只支持'

$taskFixtureBase = Join-Path $taskRepo 'target/quicker-action-tests'
$taskFixtureRoot = Join-Path $taskFixtureBase ([guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $taskFixtureRoot) { throw '本次夹具目录意外存在。' }
$null = [IO.Directory]::CreateDirectory($taskFixtureRoot)

function Write-FixtureManifest([object]$Fixture) {
    [IO.File]::WriteAllText((Join-Path $Fixture.Path 'manifest.json'),
        ($Fixture.Manifest | ConvertTo-Json -Depth 12), [Text.UTF8Encoding]::new($false))
}

function New-PackageFixture([string]$Name, [switch]$Legacy) {
    $taskPath = Join-Path $taskFixtureRoot $Name
    $null = [IO.Directory]::CreateDirectory($taskPath)
    $taskNames = @('raishot.exe', 'raishot-gpui.exe', 'raishot-cli.exe', 'vcruntime140.dll')
    if (-not $Legacy) { $taskNames += 'onnxruntime.dll' }
    $taskRecords = foreach ($taskName in $taskNames) {
        $taskBytes = [Text.Encoding]::UTF8.GetBytes('offline fixture: ' + $taskName)
        [IO.File]::WriteAllBytes((Join-Path $taskPath $taskName), $taskBytes)
        $taskSha = [Security.Cryptography.SHA256]::Create()
        try { $taskHash = [BitConverter]::ToString($taskSha.ComputeHash($taskBytes)).Replace('-', '').ToLowerInvariant() }
        finally { $taskSha.Dispose() }
        [ordered]@{ path = $taskName; bytes = $taskBytes.LongLength; sha256 = $taskHash }
    }
    $taskManifest = [ordered]@{
        schema_version = 1
        package_name = 'cea.rustshot'
        package_version = '0.4.8'
        architecture = 'windows-x64'
        files = @($taskRecords)
    }
    if (-not $Legacy) {
        $taskManifest.runtime = [ordered]@{ files = @('vcruntime140.dll') }
        $taskManifest.ocr_runtime = [ordered]@{ files = @('onnxruntime.dll') }
    }
    $taskFixture = [pscustomobject]@{ Path = $taskPath; Manifest = $taskManifest }
    Write-FixtureManifest $taskFixture
    return $taskFixture
}

function Invoke-FixtureValidation([object]$Fixture) {
    return $taskExpressionType::Validate($Fixture.Path, 'cea.rustshot', '0.4.8')
}

$taskFixture = New-PackageFixture 'complete'
Assert-Equal '全部文件与运行库声明通过' (Join-Path $taskFixture.Path 'raishot.exe') (Invoke-FixtureValidation $taskFixture)
$taskFixture = New-PackageFixture 'legacy-four-files' -Legacy
Assert-Equal '兼容四文件清单结构' (Join-Path $taskFixture.Path 'raishot.exe') (Invoke-FixtureValidation $taskFixture)

$taskFixture = New-PackageFixture 'old-exe-name'
Move-Item -LiteralPath (Join-Path $taskFixture.Path 'raishot.exe') -Destination (Join-Path $taskFixture.Path 'rabshot.exe')
$taskFixture.Manifest.files[0].path = 'rabshot.exe'
Write-FixtureManifest $taskFixture
Assert-Rejected '旧名 EXE 不能充当 RaiShot 程序' { Invoke-FixtureValidation $taskFixture } '清单缺少必需文件'

$taskFixture = New-PackageFixture 'missing-extra-file'
[IO.File]::Delete((Join-Path $taskFixture.Path 'onnxruntime.dll'))
Assert-Rejected '额外 ORT 文件缺失拒绝' { Invoke-FixtureValidation $taskFixture } '依赖文件缺少或大小不符'

$taskFixture = New-PackageFixture 'wrong-extra-size'
[IO.File]::AppendAllText((Join-Path $taskFixture.Path 'onnxruntime.dll'), 'damaged')
Assert-Rejected '额外 ORT 文件大小损坏拒绝' { Invoke-FixtureValidation $taskFixture } '依赖文件缺少或大小不符'

$taskFixture = New-PackageFixture 'wrong-extra-hash'
$taskOrtPath = Join-Path $taskFixture.Path 'onnxruntime.dll'
$taskChangedBytes = [IO.File]::ReadAllBytes($taskOrtPath)
$taskChangedBytes[0] = $taskChangedBytes[0] -bxor 1
[IO.File]::WriteAllBytes($taskOrtPath, $taskChangedBytes)
Assert-Rejected '额外 ORT 文件同大小错误拒绝' { Invoke-FixtureValidation $taskFixture } '依赖文件校验失败'

$taskFixture = New-PackageFixture 'path-traversal'
$taskFixture.Manifest.files[0].path = '../raishot.exe'
Write-FixtureManifest $taskFixture
Assert-Rejected '路径穿越拒绝' { Invoke-FixtureValidation $taskFixture } '只允许包根目录文件名'

$taskFixture = New-PackageFixture 'case-duplicate'
$taskFixture.Manifest.files += [ordered]@{
    path = 'RAISHOT.EXE'
    bytes = $taskFixture.Manifest.files[0].bytes
    sha256 = $taskFixture.Manifest.files[0].sha256
}
Write-FixtureManifest $taskFixture
Assert-Rejected '大小写重复文件名拒绝' { Invoke-FixtureValidation $taskFixture } '重复文件名'

$taskFixture = New-PackageFixture 'bad-sha'
$taskFixture.Manifest.files[0].sha256 = 'invalid'
Write-FixtureManifest $taskFixture
Assert-Rejected '非法 SHA256 拒绝' { Invoke-FixtureValidation $taskFixture } 'SHA256 格式不正确'

$taskFixture = New-PackageFixture 'negative-size'
$taskFixture.Manifest.files[0].bytes = -1
Write-FixtureManifest $taskFixture
Assert-Rejected '负文件大小拒绝' { Invoke-FixtureValidation $taskFixture } '文件大小不正确'

$taskFixture = New-PackageFixture 'missing-runtime-declaration'
$taskFixture.Manifest.runtime.files += 'missing-runtime.dll'
Write-FixtureManifest $taskFixture
Assert-Rejected 'VC 运行库声明遗漏拒绝' { Invoke-FixtureValidation $taskFixture } '缺少已声明的运行库文件'

$taskFixture = New-PackageFixture 'missing-ocr-declaration'
$taskFixture.Manifest.files = @($taskFixture.Manifest.files | Where-Object { $_.path -ne 'onnxruntime.dll' })
Write-FixtureManifest $taskFixture
Assert-Rejected 'OCR 运行库声明遗漏拒绝' { Invoke-FixtureValidation $taskFixture } '缺少已声明的运行库文件'

Write-Host "离线测试全部通过：$script:taskPassed 项。夹具目录：$taskFixtureRoot"
