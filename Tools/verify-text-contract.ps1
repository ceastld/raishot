param(
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug','release')][string]$Configuration = 'release',
    [ValidateSet('enter','escape','held','parameter','ime')][string[]]$Groups = @()
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExe = (Resolve-Path -LiteralPath (Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe")).Path
if (@(Get-CimInstance Win32_Process -Filter "Name='raishot-gpui.exe'" | Where-Object { $_.CommandLine -match '--pin-smoke' }).Count -ne 0) {
    throw '已有贴图专用验收进程，未重复启动。'
}
# 不关闭普通会话，不注入全局输入，不读桌面或剪贴板。
$taskDir = Join-Path $taskRoot ('target/qa/text-contract-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskDir)
$taskReport = Join-Path $taskRoot 'target/qa/text-contract/report.json'
if (Test-Path -LiteralPath $taskReport) { Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'previous-report.json') }

function Get-TaskSourceManifest {
    $taskFiles = @(
        foreach ($taskSourceDirectory in @('src','assets','vendor')) {
            Get-ChildItem -LiteralPath (Join-Path $taskRoot $taskSourceDirectory) -Recurse -File
        }
        foreach ($taskSourceFile in @('Cargo.toml','Cargo.lock','.cargo/config.toml','build.rs','build.ps1','Tools/verify-text-contract.ps1')) {
            Get-Item -LiteralPath (Join-Path $taskRoot $taskSourceFile)
        }
    )
    @($taskFiles | Sort-Object FullName -Unique | ForEach-Object {
        [pscustomobject]@{
            path = $_.FullName.Substring($taskRoot.Length + 1).Replace('\','/')
            sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        }
    }) | ConvertTo-Json -Depth 4 -Compress
}
$taskManifest = Get-TaskSourceManifest
$taskManifestPath = Join-Path $taskDir 'source-hashes.json'
$taskManifest | Set-Content -LiteralPath $taskManifestPath -Encoding utf8
$taskSourceHash = (Get-FileHash -LiteralPath $taskManifestPath -Algorithm SHA256).Hash
$taskBinaryHash = (Get-FileHash -LiteralPath $taskExe -Algorithm SHA256).Hash
$taskVariables = @('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_DATA_DIR','RUSTSHOT_TEXT_CONTRACT_GROUPS')
$taskOldValues = @{}
foreach ($taskVariable in $taskVariables) { $taskOldValues[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable,'Process') }
$taskProcess = $null
$taskSummary = $null
$taskFailure = $null
$taskSourcesUnchanged = $false
$taskBinaryUnchanged = $false
$taskPassed = $false
$taskAllGroups = @(
    'native-text-enter-modifiers-unicode-gpu-export',
    'native-text-escape-edit-empty-undo',
    'native-text-terminal-held-no-host-actions',
    'native-text-parameter-return-keeps-draft',
    'native-text-ime-adapter-native-key-bypass'
)
$taskShortGroups = @('enter','escape','held','parameter','ime')
if (@($Groups | Sort-Object -Unique).Count -ne $Groups.Count) { throw '普通文字专项组参数重复' }
$taskExpectedGroups = @(
    for ($taskGroupIndex = 0; $taskGroupIndex -lt $taskAllGroups.Count; $taskGroupIndex++) {
        if ($Groups.Count -eq 0 -or $Groups -contains $taskShortGroups[$taskGroupIndex]) { $taskAllGroups[$taskGroupIndex] }
    }
)
try {
    $env:RUSTSHOT_PIN_SMOKE_CASE = 'text_contract'
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $env:RUSTSHOT_DATA_DIR = Join-Path $taskRoot 'target/qa/text-contract/settings'
    $env:RUSTSHOT_TEXT_CONTRACT_GROUPS = $taskExpectedGroups -join ','
    $taskStarted = (Get-Date).ToUniversalTime()
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--pin-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskDir 'stdout.log') -RedirectStandardError (Join-Path $taskDir 'stderr.log')
    $taskDeadline = (Get-Date).AddSeconds(120)
    while (-not $taskProcess.WaitForExit(1000)) {
        if ((Get-Date) -ge $taskDeadline) { throw "普通文字合同专项超时，进程仍为 $($taskProcess.Id)，记录 $taskDir" }
    }
    $taskProcess.Refresh()
    # 非零退出也先保留本次原生报告；不能把已通过组和具体错误丢成泛化退出提示。
    $taskFreshReport = (Test-Path -LiteralPath $taskReport) -and (Get-Item -LiteralPath $taskReport).LastWriteTimeUtc -ge $taskStarted
    if ($taskFreshReport) {
        Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'report.json')
        $taskSummary = Get-Content -LiteralPath $taskReport -Raw | ConvertFrom-Json
    }
    $taskBinaryUnchanged = (Get-FileHash -LiteralPath $taskExe -Algorithm SHA256).Hash -eq $taskBinaryHash
    $taskSourcesUnchanged = (Get-TaskSourceManifest) -ceq $taskManifest
    if (-not $taskProcess.HasExited) { throw "普通文字合同专项worker尚未退出：$taskDir" }
    if ($taskProcess.ExitCode -ne 0) {
        $taskNativeError = if ($null -ne $taskSummary) { [string]$taskSummary.error } else { $null }
        if (-not [string]::IsNullOrWhiteSpace($taskNativeError)) { throw "普通文字合同专项未通过：$taskNativeError；worker退出码 $($taskProcess.ExitCode)，证据 $taskDir" }
        $taskReportState = if ($taskFreshReport) { '本次报告没有具体错误' } else { '未生成本次原生报告' }
        throw "普通文字合同专项worker退出码 $($taskProcess.ExitCode)：$taskReportState，证据 $taskDir"
    }
    if (-not $taskFreshReport) { throw "未生成本次普通文字合同专项报告；worker退出码 $($taskProcess.ExitCode)，证据 $taskDir" }
    if ($taskSummary.ok -ne $true) { throw "普通文字合同专项未通过：$($taskSummary.error)" }
    if (@($taskSummary.groups).Count -ne $taskExpectedGroups.Count) { throw '普通文字专项组数不符' }
    if (@($taskSummary.requested_groups).Count -ne $taskExpectedGroups.Count -or @($taskExpectedGroups | Where-Object { $taskSummary.requested_groups -cnotcontains $_ }).Count -ne 0) { throw '普通文字专项请求组不符' }
    $taskSkippedGroups = @($taskAllGroups | Where-Object { $taskExpectedGroups -cnotcontains $_ })
    if (@($taskSummary.skipped_groups).Count -ne $taskSkippedGroups.Count -or @($taskSkippedGroups | Where-Object { $taskSummary.skipped_groups -cnotcontains $_ }).Count -ne 0) { throw '普通文字专项跳过组证据不符' }
    foreach ($taskGroup in $taskExpectedGroups) {
        $taskMatches = @($taskSummary.groups | Where-Object { $_.group -ceq $taskGroup -and $_.ok -eq $true })
        if ($taskMatches.Count -ne 1) { throw "专项组缺失或重复：$taskGroup" }
    }
    foreach ($taskPrivacyFlag in @('clipboard_read','clipboard_written','desktop_read','global_pointer_moved','global_input_injected')) {
        if ($taskSummary.$taskPrivacyFlag -ne $false) { throw "专项越出自有窗口范围或证据缺失：$taskPrivacyFlag" }
    }
    if ($taskSummary.ime_adapter_only -ne $true -or $taskSummary.real_candidate_ui_tested -ne $false -or $taskSummary.watermark_single_line_tested -ne $false) { throw '组合态适配器或水印专项范围声明不符' }
    if ($taskSummary.keyboard_adapter.keys -le 0 -or $taskSummary.keyboard_adapter.keys -ne $taskSummary.keyboard_adapter.restored -or $taskSummary.keyboard_adapter.errors -ne 0 -or [string]::IsNullOrWhiteSpace($taskSummary.keyboard_adapter.scope)) { throw '自有 HWND UI 线程键盘 adapter 未完整保存恢复' }
    if (-not $taskBinaryUnchanged -or -not $taskSourcesUnchanged) { throw '验收期间二进制或源码发生变化，不能关联最终版本' }
    $taskPassed = $true
} catch {
    $taskFailure = $_.Exception.Message
} finally {
    if ($null -ne $taskProcess) { $taskProcess.Refresh() }
    if (Test-Path -LiteralPath $taskReport) {
        if ((Get-Item -LiteralPath $taskReport).LastWriteTimeUtc -ge $taskStarted) {
            Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'report.json') -Force
        }
    }
    $taskWorkerExited = $null -ne $taskProcess -and $taskProcess.HasExited
    $taskExitCode = if ($taskWorkerExited) { $taskProcess.ExitCode } else { $null }
    $taskEvidence = [pscustomobject]@{
        ok = $taskPassed
        groups = if ($null -ne $taskSummary) { @($taskSummary.groups | Where-Object { $_.ok -eq $true } | ForEach-Object { $_.group }) } else { @() }
        requestedGroups = $taskExpectedGroups
        skippedGroups = @($taskAllGroups | Where-Object { $taskExpectedGroups -cnotcontains $_ })
        error = $taskFailure
        nativeError = if ($null -ne $taskSummary) { $taskSummary.error } else { $null }
        finalRelease = $taskPassed -and $Configuration -eq 'release'
        completeContract = $taskPassed -and $taskExpectedGroups.Count -eq $taskAllGroups.Count
        configuration = $Configuration
        binary = [pscustomobject]@{ path=$taskExe; sha256=$taskBinaryHash; unchanged=$taskBinaryUnchanged }
        source = [pscustomobject]@{ manifest=$taskManifestPath; sha256=$taskSourceHash; fileCount=@($taskManifest | ConvertFrom-Json).Count; unchanged=$taskSourcesUnchanged }
        workerExit = [pscustomobject]@{ pid=if ($null -ne $taskProcess) {$taskProcess.Id} else {$null}; exited=$taskWorkerExited; code=$taskExitCode }
        report = Join-Path $taskDir 'report.json'
    }
    $taskEvidence | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $taskDir 'runner-report.json') -Encoding utf8
    foreach ($taskVariable in $taskVariables) { [Environment]::SetEnvironmentVariable($taskVariable,$taskOldValues[$taskVariable],'Process') }
}
$taskEvidence | ConvertTo-Json -Depth 6 -Compress
Write-Output $taskDir
if (-not $taskPassed) { throw $taskFailure }

