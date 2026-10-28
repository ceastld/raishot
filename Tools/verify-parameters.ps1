[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Worker,
    [ValidateSet('capture','pin')][string[]]$CaseFilter=@('capture','pin'),
    [ValidateSet('all','width','slider','wheel','selection','font_effect')][string]$CaptureCase='all'
)
$ErrorActionPreference='Stop'
$taskRoot=(Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker=(Resolve-Path -LiteralPath $Worker).Path
$taskHash=(Get-FileHash -LiteralPath $taskWorker).Hash
if (-not ('RustShotParameterQa' -as [type])) {
    Add-Type @'
using System;using System.Runtime.InteropServices;
public static class RustShotParameterQa {
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
if ([RustShotParameterQa]::CaptureExists()) {throw '已有截图会话，保留用户操作，停止参数专项。'}
$taskKeys=@('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_PARAMETER_QA_CASE')
$taskOld=@{}
foreach($taskKey in $taskKeys){$taskOld[$taskKey]=[Environment]::GetEnvironmentVariable($taskKey,'Process')}
$taskOutput=Join-Path $taskRoot ('target/qa/parameter-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void][IO.Directory]::CreateDirectory($taskOutput)
try {
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_SMOKE_CASE','parameter','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY','1','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PARAMETER_QA_CASE',$CaptureCase,'Process')
    foreach($taskCase in $CaseFilter) {
        if([RustShotParameterQa]::CaptureExists()){throw '有新截图会话，停止后续参数专项。'}
        $taskFolder=Join-Path $taskOutput $taskCase
        [void][IO.Directory]::CreateDirectory($taskFolder)
        $taskStdout=Join-Path $taskFolder 'stdout.jsonl'
        $taskStderr=Join-Path $taskFolder 'stderr.log'
        $taskArgument=if($taskCase -eq 'pin'){'--pin-smoke'}else{'--parameter-smoke'}
        $taskProcess=Start-Process -FilePath $taskWorker -ArgumentList $taskArgument -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        if(-not $taskProcess.WaitForExit(55000)){throw "参数 $taskCase 专项超时，保留自有PID=$($taskProcess.Id)，未强制结束；$taskFolder"}
        $taskProcess.Refresh()
        if($taskProcess.ExitCode -ne 0){throw "参数 $taskCase 专项失败：$taskFolder"}
        $taskCaptureChecks=[ordered]@{width='native_parameter_width_real_keydown_draft_confirm_once_escape_preserves_redo';slider='native_parameter_slider_outside_track_popup_continuity_atomic_undo';wheel='native_parameter_high_resolution_wheel_direction_draft_isolation';selection='gpui_parameter_select_all_resets_native_wheel_remainder_and_primary_style';font_effect='native_parameter_font_and_effect_integer_two_shared_scene_output'}
        $taskExpected=if($taskCase -eq 'capture'){if($CaptureCase -eq 'all'){@($taskCaptureChecks.Values)}else{@($taskCaptureChecks[$CaptureCase])}}else{@('native_parameter_pin_real_keydown_width_two_bars_stable')}
        $taskRows=@(Get-Content -LiteralPath $taskStdout|Where-Object{$_.StartsWith('{')}|ForEach-Object{$_|ConvertFrom-Json})
        foreach($taskName in $taskExpected){if(@($taskRows|Where-Object{$_.test -eq $taskName -and $_.ok -eq $true}).Count -ne 1){throw "参数报告缺少 $taskName"}}
        if(@($taskRows|Where-Object{$_.ok -eq $false}).Count -ne 0){throw '参数报告包含失败'}
        if([RustShotParameterQa]::Windows([uint32]$taskProcess.Id) -ne 0 -or [RustShotParameterQa]::CaptureExists()){throw '参数专项窗口或截图互斥体未回收'}
        if((Get-FileHash -LiteralPath $taskWorker).Hash -ne $taskHash){throw '参数专项期间产物变化'}
        $taskSummary=[pscustomobject]@{ok=$true;case=$taskCase;pid=$taskProcess.Id;exit_code=$taskProcess.ExitCode;worker=$taskWorker;sha256=$taskHash;tests=$taskExpected;log=$taskStdout;windows_recycled=$true;capture_mutex_released=$true}
        $taskSummary|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskFolder 'summary.json') -Encoding utf8
        $taskSummary|ConvertTo-Json -Depth 5 -Compress
    }
} finally {foreach($taskKey in $taskKeys){[Environment]::SetEnvironmentVariable($taskKey,$taskOld[$taskKey],'Process')}}
