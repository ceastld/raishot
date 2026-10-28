param(
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug','release')][string]$Configuration = 'debug',
    [string]$Fixture = $env:RUSTSHOT_LONG_FIXTURE
)
$ErrorActionPreference = 'Stop'
$taskFixture = if ([string]::IsNullOrEmpty($Fixture)) { 'noise' } else { $Fixture }
if ($taskFixture -notin @('noise','sparse-page')) { throw "未知长截图验收场景：$taskFixture" }
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExe = Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe"
if (@(Get-Process -Name 'raishot-gpui' -ErrorAction SilentlyContinue).Count -ne 0) {
    throw '已有截图、贴图或设置会话，保留当前会话，未启动验收。'
}
$taskDir = Join-Path $taskRoot ('target/qa/long-workflow-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskDir)
$taskVariables = @('RUSTSHOT_DATA_DIR','RUSTSHOT_LONG_WORKFLOW_REPORT','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_LONG_FIXTURE')
$taskOldValues = @{}
foreach ($taskVariable in $taskVariables) {
    $taskOldValues[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable,'Process')
}
try {
    $env:RUSTSHOT_DATA_DIR = Join-Path $taskDir 'settings'
    $env:RUSTSHOT_LONG_WORKFLOW_REPORT = Join-Path $taskDir 'summary.json'
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $env:RUSTSHOT_LONG_FIXTURE = $taskFixture
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--long-workflow-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskDir 'stdout.log') -RedirectStandardError (Join-Path $taskDir 'stderr.log')
    $taskDeadline = (Get-Date).AddSeconds(90)
    while (-not $taskProcess.WaitForExit(1000)) {
        if ((Get-Date) -ge $taskDeadline) { throw "自有长截图验收进程超时：$($taskProcess.Id)，记录 $taskDir" }
    }
    $taskProcess.Refresh()
    if ($taskProcess.ExitCode -ne 0) {
        Get-Content -LiteralPath (Join-Path $taskDir 'stderr.log')
        if (Test-Path -LiteralPath $env:RUSTSHOT_LONG_WORKFLOW_REPORT) { Get-Content -LiteralPath $env:RUSTSHOT_LONG_WORKFLOW_REPORT }
        throw "长截图工作流验收失败：$taskDir"
    }
    $taskSummary = Get-Content -LiteralPath $env:RUSTSHOT_LONG_WORKFLOW_REPORT -Raw | ConvertFrom-Json
    if ($taskSummary.ok -ne $true) { throw "长截图验收未通过：$taskDir" }
    [pscustomobject]@{path=$taskExe;sha256=(Get-FileHash -LiteralPath $taskExe).Hash;configuration=$Configuration;fixture=$taskFixture;report=$env:RUSTSHOT_LONG_WORKFLOW_REPORT;ok=$true} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskDir 'binary.json') -Encoding utf8
    [pscustomobject]@{ok=$taskSummary.ok;fixture=$taskSummary.fixture;continuous_steps=@($taskSummary.continuous_scroll_steps).Count;dimensions=@($taskSummary.width,$taskSummary.height);undo=$taskSummary.undo_after_continuous_scroll.ok;exact_pixels=$taskSummary.exact_pixels;workers=$taskSummary.workers} | ConvertTo-Json -Depth 4 -Compress
    Write-Output $taskDir
} finally {
    foreach ($taskVariable in $taskVariables) { [Environment]::SetEnvironmentVariable($taskVariable,$taskOldValues[$taskVariable],'Process') }
}
