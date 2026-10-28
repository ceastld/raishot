param(
    [string]$TargetDirectory='target/next',
    [ValidateSet('release')][string]$Configuration='release',
    [ValidateSet('move','resize','cancel-failed-stale','auto-target','auto-speed','auto-popup')][string[]]$Groups=@('move','resize','cancel-failed-stale','auto-target')
)
$ErrorActionPreference='Stop'
$taskRoot=Split-Path -Parent $PSScriptRoot
$taskExe=(Resolve-Path -LiteralPath (Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe")).Path
if($Groups.Count-eq 0 -or @($Groups|Select-Object -Unique).Count-ne $Groups.Count){throw 'ROI验收组为空或重复'}
if(@(Get-CimInstance Win32_Process -Filter "Name='raishot-gpui.exe'"|Where-Object {$_.CommandLine -match '--long-region-smoke'}).Count-ne 0){throw '已有自有ROI验收进程，未重复启动'}
$taskFolder=Join-Path $taskRoot ('target/qa/long-region-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskFolder)
function Get-TaskSources {
    @(@(foreach($taskSourceDir in @('src','assets','vendor')){Get-ChildItem -LiteralPath (Join-Path $taskRoot $taskSourceDir) -Recurse -File}; foreach($taskSourceFile in @('Cargo.toml','Cargo.lock','build.rs','build.ps1','.cargo/config.toml','Tools/verify-long-region.ps1')){Get-Item -LiteralPath (Join-Path $taskRoot $taskSourceFile)})|Sort-Object FullName -Unique|ForEach-Object{[pscustomobject]@{path=$_.FullName.Substring($taskRoot.Length+1).Replace('\','/');sha256=(Get-FileHash -LiteralPath $_.FullName).Hash}})|ConvertTo-Json -Depth 4
}
$taskManifest=Get-TaskSources
$taskManifest|Set-Content -LiteralPath (Join-Path $taskFolder 'source-hashes.json') -Encoding utf8
$taskHash=(Get-FileHash -LiteralPath $taskExe).Hash
$taskVariables=@('RUSTSHOT_LONG_REGION_QA_DIR','RUSTSHOT_LONG_REGION_GROUPS','RUSTSHOT_DATA_DIR','RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskOld=@{}
foreach($taskVariable in $taskVariables){$taskOld[$taskVariable]=[Environment]::GetEnvironmentVariable($taskVariable,'Process')}
$taskPeakPrivate=0L;$taskPeakWorking=0L;$taskSamples=[System.Collections.Generic.List[object]]::new()
$taskProcess=$null
try{
    $env:RUSTSHOT_LONG_REGION_QA_DIR=$taskFolder
    $env:RUSTSHOT_LONG_REGION_GROUPS=$Groups -join ','
    $env:RUSTSHOT_DATA_DIR=Join-Path $taskFolder 'settings'
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY='1'
    $taskProcess=Start-Process -FilePath $taskExe -ArgumentList '--long-region-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskFolder 'stdout.log') -RedirectStandardError (Join-Path $taskFolder 'stderr.log')
    $taskDeadline=(Get-Date).AddSeconds(900)
    while(-not $taskProcess.WaitForExit(100)){
        try{$taskProcess.Refresh();$taskPeakPrivate=[math]::Max($taskPeakPrivate,$taskProcess.PrivateMemorySize64);$taskPeakWorking=[math]::Max($taskPeakWorking,$taskProcess.WorkingSet64);$taskSamples.Add([pscustomobject]@{utc=(Get-Date).ToUniversalTime().ToString('o');private_bytes=$taskProcess.PrivateMemorySize64;working_set_bytes=$taskProcess.WorkingSet64})}catch{}
        if((Get-Date)-ge $taskDeadline){throw "自有ROI验收超时：$($taskProcess.Id)，记录 $taskFolder"}
    }
    $taskProcess.Refresh()
    $taskReport=Get-Content -Raw -LiteralPath (Join-Path $taskFolder 'report.json')|ConvertFrom-Json
    if($taskProcess.ExitCode-ne 0 -or -not $taskReport.ok){throw "ROI验收失败：$($taskReport.error)；worker $($taskProcess.Id) exit $($taskProcess.ExitCode)；$taskFolder"}
    if($taskReport.suite-ne 'long-region' -or $taskReport.pid-ne $taskProcess.Id -or $taskReport.groups.Count-ne $Groups.Count -or @($taskReport.groups|Where-Object {-not $_.ok}).Count-ne 0){throw 'ROI报告归属或成功组不完整'}
    if($taskReport.requested_groups.Count-ne $Groups.Count -or @($Groups|Where-Object {$taskReport.requested_groups -cnotcontains $_ -or $taskReport.groups.name -cnotcontains $_}).Count-ne 0){throw 'ROI报告请求组不符'}
    if($taskReport.real_dxgi_capture-ne $true -or $taskReport.full_desktop_freeze_used-ne $false -or $taskReport.clipboard_untouched-ne $true -or $taskReport.global_input_sent-ne $false){throw 'ROI验收不是自有范围实际DXGI'}
    if($taskReport.capture_threads_after_close-ne 0 -or $taskReport.consumer_sessions_after_close-ne 0 -or $taskReport.owned_windows_destroyed-ne $true){throw 'ROI验收自有线程/窗口未清理'}
    if((Get-FileHash -LiteralPath $taskExe).Hash-ne $taskHash -or (Get-TaskSources)-cne $taskManifest){throw '验收期间来源/程序变化'}
    [pscustomobject]@{ok=$true;binary=[pscustomobject]@{path=$taskExe;sha256=$taskHash};configuration=$Configuration;finalRelease=$true;source_manifest=(Join-Path $taskFolder 'source-hashes.json');report=(Join-Path $taskFolder 'report.json');worker_pid=$taskProcess.Id;worker_exited=$taskProcess.HasExited;worker_exit_code=$taskProcess.ExitCode;sample_interval_ms=100;external_sample_peak_private_bytes=$taskPeakPrivate;external_sample_peak_working_bytes=$taskPeakWorking;real_dxgi_capture=$true}|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskFolder 'runner-report.json') -Encoding utf8
    Write-Output $taskFolder
}finally{
    if($null-ne $taskProcess){
        $taskProcess.Refresh();$taskForced=$false
        if(-not $taskProcess.HasExited){
            # 只结束本调用持有的自有QA child，不按进程名称关闭普通截图会话。
            Stop-Process -InputObject $taskProcess -Force -ErrorAction SilentlyContinue
            [void]$taskProcess.WaitForExit(5000)
            $taskProcess.Refresh();$taskForced=$true
        }
        [pscustomobject]@{pid=$taskProcess.Id;exited=$taskProcess.HasExited;forced_cleanup=$taskForced;path=$taskExe}|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $taskFolder 'worker-cleanup.json') -Encoding utf8
    }
    $taskSamples|ConvertTo-Json -Depth 3|Set-Content -LiteralPath (Join-Path $taskFolder 'process-memory-samples.json') -Encoding utf8
    foreach($taskVariable in $taskVariables){[Environment]::SetEnvironmentVariable($taskVariable,$taskOld[$taskVariable],'Process')}
}
