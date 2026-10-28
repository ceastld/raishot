param(
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug','release')][string]$Configuration = 'release'
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExe = (Resolve-Path -LiteralPath (Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe")).Path
if (@(Get-CimInstance Win32_Process -Filter "Name='raishot-gpui.exe'" | Where-Object { $_.CommandLine -match '--pin-smoke' }).Count -ne 0) {
    throw '已有贴图专用验收进程，未重复启动。'
}
# 不关闭普通会话，不注入全局输入，不读桌面或剪贴板。
$taskDir = Join-Path $taskRoot ('target/qa/local-fill-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskDir)
$taskReport = Join-Path $taskRoot 'target/qa/local-fill/report.json'
if (Test-Path -LiteralPath $taskReport) { Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'previous-report.json') }

function Get-TaskSourceManifest {
    $taskFiles = @(
        foreach ($taskSourceDirectory in @('src','assets','vendor')) {
            Get-ChildItem -LiteralPath (Join-Path $taskRoot $taskSourceDirectory) -Recurse -File
        }
        foreach ($taskSourceFile in @('Cargo.toml','Cargo.lock','.cargo/config.toml','build.rs','build.ps1','Tools/verify-local-fill.ps1')) {
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
$taskVariables = @('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_DATA_DIR')
$taskOldValues = @{}
foreach ($taskVariable in $taskVariables) { $taskOldValues[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable,'Process') }
$taskProcess = $null
$taskSummary = $null
$taskFailure = $null
$taskSourcesUnchanged = $false
$taskBinaryUnchanged = $false
$taskPassed = $false
$taskExpectedGroups = @(
    'native-local-fill-effect-two-bars-width-wheel',
    'native-local-fill-stroke-preview-commit-history-gpu-export',
    'native-local-fill-smart-effect-production-button',
    'native-local-fill-batch-independent-roi-frozen-baseline',
    'native-local-fill-fractional-tile-nearest-gpu-export',
    'native-local-fill-zero-neighborhood-failure-transaction-gates',
    'native-local-fill-budget-failure-transaction-gates'
)
try {
    $env:RUSTSHOT_PIN_SMOKE_CASE = 'local_fill'
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $env:RUSTSHOT_DATA_DIR = Join-Path $taskRoot 'target/qa/local-fill/settings'
    $taskStarted = (Get-Date).ToUniversalTime()
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--pin-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskDir 'stdout.log') -RedirectStandardError (Join-Path $taskDir 'stderr.log')
    $taskDeadline = (Get-Date).AddSeconds(90)
    while (-not $taskProcess.WaitForExit(1000)) {
        if ((Get-Date) -ge $taskDeadline) { throw "自有填充专项超时，进程仍为 $($taskProcess.Id)，记录 $taskDir" }
    }
    $taskProcess.Refresh()
    $taskBinaryUnchanged = (Get-FileHash -LiteralPath $taskExe -Algorithm SHA256).Hash -eq $taskBinaryHash
    $taskSourcesUnchanged = (Get-TaskSourceManifest) -ceq $taskManifest
    if (-not $taskProcess.HasExited -or $taskProcess.ExitCode -ne 0) { throw "填充专项worker未成功退出：$taskDir" }
    if (-not (Test-Path -LiteralPath $taskReport) -or (Get-Item -LiteralPath $taskReport).LastWriteTimeUtc -lt $taskStarted) { throw '未生成本次填充专项报告' }
    Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'report.json')
    $taskSummary = Get-Content -LiteralPath $taskReport -Raw | ConvertFrom-Json
    if ($taskSummary.ok -ne $true) { throw "填充专项未通过：$($taskSummary.error)" }
    foreach ($taskGroup in $taskExpectedGroups) {
        $taskMatches = @($taskSummary.groups | Where-Object { $_.group -ceq $taskGroup -and $_.ok -eq $true })
        if ($taskMatches.Count -ne 1) { throw "专项组缺失或重复：$taskGroup" }
    }
    foreach ($taskPrivacyFlag in @('clipboard_read','clipboard_written','desktop_read','global_pointer_moved','global_input_injected')) {
        if ($taskSummary.$taskPrivacyFlag -ne $false) { throw "专项越出自有窗口范围或证据缺失：$taskPrivacyFlag" }
    }
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
        groups = if ($null -ne $taskSummary) { @($taskSummary.groups.group) } else { @() }
        error = $taskFailure
        finalRelease = $taskPassed -and $Configuration -eq 'release'
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
