param([string]$TargetDirectory='target',[ValidateSet('debug','release')][string]$Configuration='debug')
$ErrorActionPreference='Stop'
$taskRoot=Split-Path -Parent $PSScriptRoot
$taskExe=Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe"
if(@(Get-CimInstance Win32_Process -Filter "Name='raishot-gpui.exe'"|Where-Object {$_.CommandLine -match '--long-memory-smoke'}).Count -ne 0){throw '已有自有长图内存验收，未重复启动'}
$taskFolder=Join-Path $taskRoot ('target/qa/long-memory-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskFolder)
$taskVariables=@('RUSTSHOT_LONG_MEMORY_QA_DIR','RUSTSHOT_DATA_DIR','RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskOld=@{}
foreach($taskVariable in $taskVariables){$taskOld[$taskVariable]=[Environment]::GetEnvironmentVariable($taskVariable,'Process')}
try{
 $env:RUSTSHOT_LONG_MEMORY_QA_DIR=$taskFolder
 $env:RUSTSHOT_DATA_DIR=Join-Path $taskFolder 'settings'
 $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY='1'
 $taskProcess=Start-Process -FilePath $taskExe -ArgumentList '--long-memory-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskFolder 'stdout.log') -RedirectStandardError (Join-Path $taskFolder 'stderr.log')
 $taskDeadline=(Get-Date).AddSeconds(120)
 $taskPeakPrivate=0L;$taskPeakWorking=0L
 while(-not $taskProcess.WaitForExit(200)){
  try{$taskProcess.Refresh();$taskPeakPrivate=[math]::Max($taskPeakPrivate,$taskProcess.PrivateMemorySize64);$taskPeakWorking=[math]::Max($taskPeakWorking,$taskProcess.WorkingSet64)}catch{}
  if((Get-Date) -ge $taskDeadline){throw "自有长图内存检查超时：$($taskProcess.Id)，记录 $taskFolder"}
 }
 $taskProcess.Refresh()
 if($taskProcess.ExitCode -ne 0){Get-Content -LiteralPath (Join-Path $taskFolder 'stderr.log');throw "长图内存检查失败：$taskFolder"}
 $taskReport=Get-Content -Raw -LiteralPath (Join-Path $taskFolder 'report.json')|ConvertFrom-Json
 if(-not $taskReport.ok){throw "长图内存报告未通过：$taskFolder"}
 [pscustomobject]@{ok=$true;path=$taskExe;sha256=(Get-FileHash -LiteralPath $taskExe).Hash;configuration=$Configuration;report_dir=$taskFolder;worker_pid=$taskProcess.Id;worker_exited=$taskProcess.HasExited;external_sample_peak_private_bytes=$taskPeakPrivate;external_sample_peak_working_bytes=$taskPeakWorking}|ConvertTo-Json -Depth 4|Set-Content -LiteralPath (Join-Path $taskFolder 'binary.json') -Encoding utf8
 Write-Output $taskFolder
}finally{foreach($taskVariable in $taskVariables){[Environment]::SetEnvironmentVariable($taskVariable,$taskOld[$taskVariable],'Process')}}
