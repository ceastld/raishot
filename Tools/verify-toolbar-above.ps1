param(
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug','release')][string]$Configuration = 'debug'
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExe = Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe"
$taskDir = Join-Path $taskRoot ('target/qa/toolbar-above-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskDir)
$taskVariables = @('RUSTSHOT_DATA_DIR','RUSTSHOT_SMOKE_CASE','RUSTSHOT_TOOLBAR_ABOVE_QA_DIRECTORY','RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskOldValues = @{}
foreach ($taskVariable in $taskVariables) { $taskOldValues[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable,'Process') }
try {
    $env:RUSTSHOT_DATA_DIR = Join-Path $taskDir 'settings'
    $env:RUSTSHOT_SMOKE_CASE = 'toolbar-above'
    $env:RUSTSHOT_TOOLBAR_ABOVE_QA_DIRECTORY = $taskDir
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskDir 'stdout.log') -RedirectStandardError (Join-Path $taskDir 'stderr.log')
    $taskDeadline = (Get-Date).AddSeconds(90)
    while (-not $taskProcess.WaitForExit(1000)) {
        if ((Get-Date) -ge $taskDeadline) { throw "自有工具栏验收进程超时：$($taskProcess.Id)，记录 $taskDir" }
    }
    $taskProcess.Refresh()
    $taskSummaryPath = Join-Path $taskDir 'summary.json'
    if ($taskProcess.ExitCode -ne 0) {
        Get-Content -LiteralPath (Join-Path $taskDir 'stderr.log')
        if (Test-Path -LiteralPath $taskSummaryPath) { Get-Content -LiteralPath $taskSummaryPath }
        throw "上方工具栏验收失败：$taskDir"
    }
    $taskSummary = Get-Content -LiteralPath $taskSummaryPath -Raw | ConvertFrom-Json
    if ($taskSummary.ok -ne $true) { throw "工具栏验收未通过：$taskDir" }
    [pscustomobject]@{path=$taskExe;sha256=(Get-FileHash -LiteralPath $taskExe).Hash;configuration=$Configuration;report=$taskSummaryPath;ok=$true} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskDir 'binary.json') -Encoding utf8
    $taskSummary | ConvertTo-Json -Compress -Depth 10
    Write-Output $taskDir
} finally {
    foreach ($taskVariable in $taskVariables) { [Environment]::SetEnvironmentVariable($taskVariable,$taskOldValues[$taskVariable],'Process') }
}
