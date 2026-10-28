[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$Worker,[switch]$Output)
$ErrorActionPreference='Stop'
$taskRoot=(Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker=(Resolve-Path -LiteralPath $Worker).Path
$taskHash=(Get-FileHash -LiteralPath $taskWorker).Hash
if (-not ('RustShotSelectionEffectQa' -as [type])) {
    Add-Type @'
using System;using System.Runtime.InteropServices;
public static class RustShotSelectionEffectQa {
 delegate bool Callback(IntPtr h,IntPtr d);
 [DllImport("user32.dll")]static extern bool EnumWindows(Callback c,IntPtr d);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]static extern IntPtr OpenMutex(uint a,bool i,string n);
 [DllImport("kernel32.dll")]static extern bool CloseHandle(IntPtr h);
 public static int Windows(uint pid){int count=0;EnumWindows((h,d)=>{uint p;GetWindowThreadProcessId(h,out p);if(p==pid)count++;return true;},IntPtr.Zero);return count;}
 public static bool CaptureExists(){IntPtr h=OpenMutex(0x00100000,false,@"Local\RustShot.MVP.Capture");if(h!=IntPtr.Zero){CloseHandle(h);return true;}int e=Marshal.GetLastWin32Error();if(e==2)return false;throw new System.ComponentModel.Win32Exception(e);}
}
'@
}
if([RustShotSelectionEffectQa]::CaptureExists()){throw '已有截图会话，保留用户操作，停止选区效果专项。'}
$taskKeys=@('RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_PARAMETER_CAPTURE_CASE','RUSTSHOT_SELECTION_EFFECT_QA_CASE')
$taskOld=@{}
foreach($taskKey in $taskKeys){$taskOld[$taskKey]=[Environment]::GetEnvironmentVariable($taskKey,'Process')}
$taskOutput=Join-Path $taskRoot ('target/qa/selection-effect-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void][IO.Directory]::CreateDirectory($taskOutput)
try {
    [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY','1','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PARAMETER_CAPTURE_CASE','selection_effect','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_SELECTION_EFFECT_QA_CASE',$(if($Output){'output'}else{'all'}),'Process')
    $taskStdout=Join-Path $taskOutput 'stdout.jsonl'
    $taskStderr=Join-Path $taskOutput 'stderr.log'
    $taskProcess=Start-Process -FilePath $taskWorker -ArgumentList '--parameter-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
    if(-not $taskProcess.WaitForExit(55000)){throw "选区效果专项超时，自有PID=$($taskProcess.Id)；$taskOutput"}
    $taskProcess.Refresh()
    if($taskProcess.ExitCode -ne 0){throw "选区效果专项失败：$taskOutput"}
    $taskExpected=if($Output){@('native_selection_effect_output_document_stability')}else{@('native_selection_effect_modes_intensity_draft_cancel','native_selection_effect_slider_wheel_advanced','native_selection_effect_output_document_stability')}
    $taskRows=@(Get-Content -LiteralPath $taskStdout|Where-Object{$_.StartsWith('{')}|ForEach-Object{$_|ConvertFrom-Json})
    foreach($taskName in $taskExpected){if(@($taskRows|Where-Object{$_.test -eq $taskName -and $_.ok -eq $true}).Count -ne 1){throw "选区效果报告缺少 $taskName"}}
    if(@($taskRows|Where-Object{$_.test -like 'native_selection_effect_*'}).Count -ne @($taskExpected).Count){throw '选区效果报告数量与所选验收范围不一致'}
    if(@($taskRows|Where-Object{$_.ok -eq $false}).Count -ne 0){throw '选区效果报告包含失败'}
    if([RustShotSelectionEffectQa]::Windows([uint32]$taskProcess.Id) -ne 0 -or [RustShotSelectionEffectQa]::CaptureExists()){throw '专项窗口或截图互斥体未回收'}
    if((Get-FileHash -LiteralPath $taskWorker).Hash -ne $taskHash){throw '专项期间产物变化'}
    $taskSummary=[pscustomobject]@{ok=$true;case=$(if($Output){'Output'}else{'All'});pid=$taskProcess.Id;exit_code=$taskProcess.ExitCode;worker=$taskWorker;sha256=$taskHash;tests=@($taskExpected);log=$taskStdout;windows_recycled=$true;capture_mutex_released=$true}
    $taskSummary|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskOutput 'summary.json') -Encoding utf8
    $taskSummary|ConvertTo-Json -Depth 5 -Compress
} finally {foreach($taskKey in $taskKeys){[Environment]::SetEnvironmentVariable($taskKey,$taskOld[$taskKey],'Process')}}
