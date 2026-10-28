#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ActionId = '',
    [ValidateSet('region', 'tray')][string]$LaunchMode = 'region',
    [string]$SourceDirectory = (Join-Path $PSScriptRoot 'action'),
    [string]$OutputDirectory = (Join-Path (Split-Path $PSScriptRoot -Parent) 'dist/quicker'),
    [switch]$UpdateIcon,
    [switch]$VerifyOnly
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Invoke-NativeMcp.ps1')
. (Join-Path $PSScriptRoot 'ActionSource.ps1')
if (-not [IO.Path]::IsPathRooted($SourceDirectory)) { $SourceDirectory = Join-Path (Split-Path $PSScriptRoot -Parent) $SourceDirectory }
$taskActionSource = Get-RustShotActionSource -SourceDirectory $SourceDirectory
$taskOutput = [IO.Path]::GetFullPath($OutputDirectory)
if ($VerifyOnly) {
    $taskQaRoot = [IO.Path]::GetFullPath((Join-Path (Split-Path $PSScriptRoot -Parent) 'target/qa'))
    $taskQaPrefix = $taskQaRoot.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
    if (-not $taskOutput.StartsWith($taskQaPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'VerifyOnly 必须将 OutputDirectory 指向本项目 target/qa 内，避免测试源副本进入正式分发目录。'
    }
}
if (-not [IO.Path]::IsPathRooted($SourceDirectory)) { $SourceDirectory = Join-Path (Split-Path $PSScriptRoot -Parent) $SourceDirectory }
$taskSource = (Resolve-Path -LiteralPath $SourceDirectory).Path
$taskYaml = [IO.File]::ReadAllText((Join-Path $taskSource 'program.source.yaml'))
$taskYaml = $taskYaml.Replace('    default: region', ('    default: ' + $LaunchMode))
($taskActionSource.Data.variables | Where-Object key -eq 'launch_mode').default = $LaunchMode
if ($VerifyOnly) {
    # 当前原生 MCP 支持 param；保留正常源，用 verify 参数只运行校验。
    $taskCondition = 'condition: ''$={launch_args} != "verify"'''
    if (-not $taskYaml.Contains($taskCondition)) { throw '找不到无启动校验的条件，停止创建动作。' }
}
$taskInfo = [IO.File]::ReadAllText((Join-Path $taskSource 'info.yaml'))
$taskCode = [IO.File]::ReadAllText((Join-Path $taskSource 'files/validate-package.value.txt'))
$taskCommandCode = [IO.File]::ReadAllText((Join-Path $taskSource 'files/resolve-command.value.txt'))
if ($ActionId) {
    $taskParsedId = [guid]::Empty
    if (-not [guid]::TryParse($ActionId, [ref]$taskParsedId)) { throw 'ActionId 必须是 GUID。' }
    $taskOpened = Invoke-RustShotNativeMcp -Tool quicker_open -Arguments @{ actionId = $ActionId }
} else {
    $taskOpened = Invoke-RustShotNativeMcp -Tool quicker_create -Arguments @{
        title = $taskActionSource.Info.title
        description = $taskActionSource.Info.description
    }
}
if (-not $taskOpened.slot -or -not $taskOpened.actionId) { throw 'MCP 未返回明确的动作暂存区；不要重复创建。' }
$taskSlot = $taskOpened.slot
$taskInfoEdits = @()
if ($ActionId -and -not $UpdateIcon) {
    $taskInfoPages = [Collections.Generic.List[string]]::new()
    $taskInfoLine = 1
    do {
        $taskPage = Invoke-RustShotNativeMcp -Tool read_file -Arguments @{ path = "$taskSlot/info.yaml"; lineStart = $taskInfoLine; lineCount = 200 }
        if ([int]$taskPage.lineStart -ne $taskInfoLine -or [int]$taskPage.lineEnd -lt $taskInfoLine) {
            throw '无法完整定位已有 info.yaml；停止更新，保留原动作图标。'
        }
        $taskInfoPages.Add([string]$taskPage.content)
        $taskInfoLine = [int]$taskPage.lineEnd + 1
    } while ($taskPage.more)
    $taskInfoEdits = @(Get-RustShotActionInfoEdits -ExistingYaml ($taskInfoPages -join "`n") -SourceYaml $taskInfo)
}
function Set-RustShotNativeSource([string]$Path, [string]$Content) {
    if (-not $ActionId) {
        $null = Invoke-RustShotNativeMcp -Tool write_file -Arguments @{ path = $Path; content = $Content }
        return
    }
    try {
        $taskCurrent = Invoke-RustShotNativeMcp -Tool read_file -Arguments @{ path = $Path; lineCount = 200 }
    } catch {
        # 旧动作可能尚无新增资源；仅明确的不存在错误允许创建，权限/连接错误仍向外抛。
        if ($_.Exception.Message -notmatch '(?i)file[_ ]not[_ ]found|文件不存在') { throw }
        $null = Invoke-RustShotNativeMcp -Tool write_file -Arguments @{ path = $Path; content = $Content }
        return
    }
    if (($taskCurrent.lineEnd - $taskCurrent.lineStart + 1) -ge 200) { throw '本地模板更新不覆盖超过单页的自定义源码。请用原生 MCP 分页审阅后编辑。' }
    $taskOldLines = @($taskCurrent.content.Replace("`r`n", "`n").TrimEnd([char[]]@("`r", "`n")).Split("`n"))
    $taskNewLines = @($Content.Replace("`r`n", "`n").TrimEnd([char[]]@("`r", "`n")).Split("`n"))
    $taskPrefix = 0
    while ($taskPrefix -lt $taskOldLines.Count -and $taskPrefix -lt $taskNewLines.Count -and
        $taskOldLines[$taskPrefix] -ceq $taskNewLines[$taskPrefix]) { $taskPrefix++ }
    if ($taskPrefix -eq $taskOldLines.Count -and $taskPrefix -eq $taskNewLines.Count) { return }
    $taskSuffix = 0
    while ($taskSuffix -lt ($taskOldLines.Count - $taskPrefix) -and
        $taskSuffix -lt ($taskNewLines.Count - $taskPrefix) -and
        $taskOldLines[$taskOldLines.Count - 1 - $taskSuffix] -ceq $taskNewLines[$taskNewLines.Count - 1 - $taskSuffix]) { $taskSuffix++ }
    # 插入时纳入一行已有上下文，保持 exact oldText 和明确行号。
    if ($taskPrefix -eq ($taskOldLines.Count - $taskSuffix)) {
        if ($taskPrefix -gt 0) { $taskPrefix-- } elseif ($taskSuffix -gt 0) { $taskSuffix-- }
    }
    $taskOldEnd = $taskOldLines.Count - $taskSuffix - 1
    $taskNewEnd = $taskNewLines.Count - $taskSuffix - 1
    $taskOldText = if ($taskOldEnd -ge $taskPrefix) { $taskOldLines[$taskPrefix..$taskOldEnd] -join "`n" } else { '' }
    $taskNewText = if ($taskNewEnd -ge $taskPrefix) { $taskNewLines[$taskPrefix..$taskNewEnd] -join "`n" } else { '' }
    if ([string]::IsNullOrEmpty($taskCurrent.content)) {
        $null = Invoke-RustShotNativeMcp -Tool write_file -Arguments @{ path = $Path; content = $Content }
    } else {
        $null = Invoke-RustShotNativeMcp -Tool edit_file -Arguments @{ path = $Path; lineStart = ($taskPrefix + 1); oldText = $taskOldText; newText = $taskNewText }
    }
}
Set-RustShotNativeSource -Path "$taskSlot/program.source.yaml" -Content $taskYaml
if ($ActionId -and -not $UpdateIcon) {
    foreach ($taskEdit in $taskInfoEdits) {
        $null = Invoke-RustShotNativeMcp -Tool edit_file -Arguments @{ path = "$taskSlot/info.yaml"; lineStart = $taskEdit.lineStart; oldText = $taskEdit.oldText; newText = $taskEdit.newText }
    }
} else {
    Set-RustShotNativeSource -Path "$taskSlot/info.yaml" -Content $taskInfo
}
Set-RustShotNativeSource -Path "$taskSlot/files/validate-package.value.txt" -Content $taskCode
Set-RustShotNativeSource -Path "$taskSlot/files/resolve-command.value.txt" -Content $taskCommandCode
$taskSummary = '使用 V1/V2 共有步骤下载和校验独立 EXE 包，默认直接框选。'
$taskSaved = Invoke-RustShotNativeMcp -Tool quicker_save -Arguments @{ path = $taskSlot; summary = $taskSummary }
$taskApplied = $taskSaved.applied -eq $true
$taskVerifyRun = $null
if ($VerifyOnly) {
    $taskVerifyRun = Invoke-RustShotNativeMcp -Tool quicker_run -Arguments @{ actionId = $taskSaved.actionId; param = 'verify'; yieldMs = 1000 }
    if ($taskVerifyRun.actionId -ne $taskSaved.actionId -or [string]::IsNullOrWhiteSpace($taskVerifyRun.runId) -or
        $taskVerifyRun.stopRequested -or [long]$taskVerifyRun.cursor -ne 0 -or [long]$taskVerifyRun.nextCursor -lt 0) {
        throw 'V2 verify 首次运行身份或游标不符。'
    }
    $taskVerifyChunks = New-Object 'System.Collections.Generic.List[object]'
    $taskVerifyChunks.Add($taskVerifyRun)
    $taskRunId = $taskVerifyRun.runId
    $taskTimer = [Diagnostics.Stopwatch]::StartNew()
    while ($taskVerifyRun.running -or $taskVerifyRun.hasMore) {
        if ($taskTimer.Elapsed.TotalSeconds -ge 60) { throw 'V2 verify 超时，保留 runId，不重复运行动作。' }
        $taskCursor = [long]$taskVerifyRun.nextCursor
        $taskVerifyRun = Invoke-RustShotNativeMcp -Tool quicker_run_output -Arguments @{ runId = $taskRunId; cursor = $taskCursor; yieldMs = 1000 }
        if ($taskVerifyRun.runId -ne $taskRunId -or $taskVerifyRun.actionId -ne $taskSaved.actionId -or $taskVerifyRun.stopRequested -or
            [long]$taskVerifyRun.cursor -ne $taskCursor -or [long]$taskVerifyRun.nextCursor -lt $taskCursor) {
            throw 'V2 verify 输出身份或游标不符。'
        }
        $taskVerifyChunks.Add($taskVerifyRun)
    }
    # MCP 输出按字符游标分块，块边界可能在日志头中间，原样拼回后再解析步骤开始行。
    $taskVerifyLog = ($taskVerifyChunks | ForEach-Object output) -join ''
    # HeadlessForwardingActionLogger.BeginStep / HeadlessActionRunLogBridge.Publish 的固定格式。
    $taskStepPattern = '(?m)^\[info step(?: step=[^\]\r\n]+)? module=(?<module>[^\s\]]+)\] '
    $taskSteps = @([regex]::Matches($taskVerifyLog, $taskStepPattern) | ForEach-Object { $_.Groups['module'].Value })
    $taskForbiddenStep = '(?m)^\[[^\]\r\n]* module=sys:(?:getWindowTitle|sendMessage|run|csscript)\]'
    if ($taskVerifyRun.result -cne 'ok' -or -not $taskVerifyLog.Contains('不符合条件，跳过。') -or
        ($taskSteps -join ',') -cne 'sys:assign,sys:simpleIf,sys:simpleIf,sys:dependencycheck,sys:assign,sys:simpleIf,sys:assign,sys:simpleIf,sys:simpleIf' -or
        $taskVerifyLog -match $taskForbiddenStep) {
        throw 'V2 verify 没有完成标准校验/跳过 EXE，检查原始输出。'
    }
}
# 本机 MCP 的内部 wire 不通过文件访问或反射导出；分发保留源副本与独立 V1 兼容文件。
$taskSourceOutput = Join-Path $taskOutput 'RaiShot.source'
[IO.Directory]::CreateDirectory((Join-Path $taskSourceOutput 'files')) | Out-Null
[IO.File]::WriteAllText((Join-Path $taskSourceOutput 'program.source.yaml'), $taskYaml, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText((Join-Path $taskSourceOutput 'info.yaml'), $taskInfo, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText((Join-Path $taskSourceOutput 'data.json'), ($taskActionSource.Data | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText((Join-Path $taskSourceOutput 'files/validate-package.value.txt'), $taskCode, [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText((Join-Path $taskSourceOutput 'files/resolve-command.value.txt'), $taskCommandCode, [Text.UTF8Encoding]::new($false))
if ($VerifyOnly) {
    [IO.File]::WriteAllText((Join-Path $taskOutput 'verify-output.json'), ($taskVerifyChunks.ToArray() | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
}
$taskReceipt = [pscustomobject]@{
    backend = 'V2-native-mcp'
    action_id = $taskSaved.actionId
    slot = $taskSlot
    edit_version = $taskSaved.editVersion
    location = $taskSaved.location
    applied_to_original = $taskApplied
    launch_mode = $LaunchMode
    verify_only = [bool]$VerifyOnly
    verify_result = $taskVerifyRun
    source_copy = $taskSourceOutput
    dependency_published = $false
    warnings = $taskSaved.warnings
}
[IO.File]::WriteAllText((Join-Path $taskOutput 'RaiShot.v2-local.json'), ($taskReceipt | ConvertTo-Json -Depth 12), [Text.UTF8Encoding]::new($false))
$taskReceipt | ConvertTo-Json -Depth 12
