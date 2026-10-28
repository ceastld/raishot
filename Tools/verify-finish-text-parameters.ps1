[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Worker,
    [ValidateSet('capture','pin')][string[]]$CaseFilter=@('capture','pin')
)
$ErrorActionPreference='Stop'
$taskRoot=(Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker=(Resolve-Path -LiteralPath $Worker).Path
$taskHash=(Get-FileHash -LiteralPath $taskWorker).Hash
if (-not ('RustShotFinishTextQa' -as [type])) {
    Add-Type @'
using System;using System.Runtime.InteropServices;
public static class RustShotFinishTextQa {
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
if([RustShotFinishTextQa]::CaptureExists()){throw '已有截图会话，保留用户操作，停止文字/选区参数专项。'}
$taskKeys=@('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_PARAMETER_CAPTURE_CASE')
$taskOld=@{}
foreach($taskKey in $taskKeys){$taskOld[$taskKey]=[Environment]::GetEnvironmentVariable($taskKey,'Process')}
$taskOutput=Join-Path $taskRoot ('target/qa/finish-text-parameters-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void][IO.Directory]::CreateDirectory($taskOutput)
try {
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_SMOKE_CASE','finish_text_parameter','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY','1','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PARAMETER_CAPTURE_CASE','finish_text','Process')
    foreach($taskCase in $CaseFilter) {
        if([RustShotFinishTextQa]::CaptureExists()){throw '有新截图会话，停止后续文字/选区参数专项。'}
        $taskFolder=Join-Path $taskOutput $taskCase
        [void][IO.Directory]::CreateDirectory($taskFolder)
        $taskStdout=Join-Path $taskFolder 'stdout.jsonl'
        $taskStderr=Join-Path $taskFolder 'stderr.log'
        $taskArgument=if($taskCase -eq 'capture'){'--parameter-smoke'}else{'--pin-smoke'}
        $taskProcess=Start-Process -FilePath $taskWorker -ArgumentList $taskArgument -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        if(-not $taskProcess.WaitForExit(55000)){throw "文字/选区参数 $taskCase 超时，自有PID=$($taskProcess.Id)；$taskFolder"}
        $taskProcess.Refresh()
        if($taskProcess.ExitCode -ne 0){throw "文字/选区参数 $taskCase 失败：$taskFolder"}
        $taskExpected=if($taskCase -eq 'capture'){
            @('native_finish_text_exact_parent_draft_caret_cancel_undo','native_finish_parameters_exact_cancel_transparent_export')
        }else{@('native_finish_text_pin_parent_two_bars_host_input')}
        $taskRows=@(Get-Content -LiteralPath $taskStdout|Where-Object{$_.StartsWith('{')}|ForEach-Object{$_|ConvertFrom-Json})
        foreach($taskName in $taskExpected){if(@($taskRows|Where-Object{$_.test -eq $taskName -and $_.ok -eq $true}).Count -ne 1){throw "参数报告缺少 $taskName"}}
        if(@($taskRows|Where-Object{$_.ok -eq $false}).Count -ne 0){throw '参数报告包含失败'}
        if([RustShotFinishTextQa]::Windows([uint32]$taskProcess.Id) -ne 0 -or [RustShotFinishTextQa]::CaptureExists()){throw '专项窗口或截图互斥体未回收'}
        if((Get-FileHash -LiteralPath $taskWorker).Hash -ne $taskHash){throw '专项期间产物变化'}
        $taskSummary=[pscustomobject]@{ok=$true;case=$taskCase;pid=$taskProcess.Id;exit_code=$taskProcess.ExitCode;worker=$taskWorker;sha256=$taskHash;tests=$taskExpected;log=$taskStdout;windows_recycled=$true;capture_mutex_released=$true}
        $taskSummary|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskFolder 'summary.json') -Encoding utf8
        $taskSummary|ConvertTo-Json -Depth 5 -Compress
    }
} finally {foreach($taskKey in $taskKeys){[Environment]::SetEnvironmentVariable($taskKey,$taskOld[$taskKey],'Process')}}
