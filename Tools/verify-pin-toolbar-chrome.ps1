[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Worker,
    [ValidateSet('chrome')][string[]]$CaseFilter=@('chrome')
)
$ErrorActionPreference='Stop'
$taskRoot=(Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker=(Resolve-Path -LiteralPath $Worker).Path
$taskHash=(Get-FileHash -LiteralPath $taskWorker).Hash
if (-not ('RustShotToolbarChromeQa' -as [type])) {
    Add-Type @'
using System;using System.Runtime.InteropServices;
public static class RustShotToolbarChromeQa {
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
if([RustShotToolbarChromeQa]::CaptureExists()){throw '已有截图会话，保留用户操作，停止贴图一级栏圆角专项。'}
$taskKeys=@('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_PIN_TOOLBAR_CASE')
$taskOld=@{}
foreach($taskKey in $taskKeys){$taskOld[$taskKey]=[Environment]::GetEnvironmentVariable($taskKey,'Process')}
$taskOutput=Join-Path $taskRoot ('target/qa/pin-toolbar-chrome-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void][IO.Directory]::CreateDirectory($taskOutput)
try {
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_SMOKE_CASE','toolbar','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY','1','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_TOOLBAR_CASE','chrome','Process')
    foreach($taskCase in $CaseFilter) {
        if([RustShotToolbarChromeQa]::CaptureExists()){throw '有新截图会话，停止后续贴图一级栏圆角专项。'}
        $taskFolder=Join-Path $taskOutput $taskCase
        [void][IO.Directory]::CreateDirectory($taskFolder)
        $taskStdout=Join-Path $taskFolder 'stdout.jsonl'
        $taskStderr=Join-Path $taskFolder 'stderr.log'
        $taskArgument='--pin-smoke'
        $taskProcess=Start-Process -FilePath $taskWorker -ArgumentList $taskArgument -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        if(-not $taskProcess.WaitForExit(55000)){throw "贴图一级栏圆角 $taskCase 超时，自有PID=$($taskProcess.Id)；$taskFolder"}
        $taskProcess.Refresh()
        if($taskProcess.ExitCode -ne 0){throw "贴图一级栏圆角 $taskCase 失败：$taskFolder"}
        $taskExpected=@('native_pin_toolbar_rounded_composed_corners_themes_resize_stable')
        $taskRows=@(Get-Content -LiteralPath $taskStdout|Where-Object{$_.StartsWith('{')}|ForEach-Object{$_|ConvertFrom-Json})
        foreach($taskName in $taskExpected){if(@($taskRows|Where-Object{$_.test -eq $taskName -and $_.ok -eq $true}).Count -ne 1){throw "参数报告缺少 $taskName"}}
        if(@($taskRows|Where-Object{$_.ok -eq $false}).Count -ne 0){throw '参数报告包含失败'}
        if([RustShotToolbarChromeQa]::Windows([uint32]$taskProcess.Id) -ne 0 -or [RustShotToolbarChromeQa]::CaptureExists()){throw '专项窗口或截图互斥体未回收'}
        if((Get-FileHash -LiteralPath $taskWorker).Hash -ne $taskHash){throw '专项期间产物变化'}
        $taskSummary=[pscustomobject]@{ok=$true;case=$taskCase;pid=$taskProcess.Id;exit_code=$taskProcess.ExitCode;worker=$taskWorker;sha256=$taskHash;tests=$taskExpected;log=$taskStdout;windows_recycled=$true;capture_mutex_released=$true}
        $taskSummary|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskFolder 'summary.json') -Encoding utf8
        $taskSummary|ConvertTo-Json -Depth 5 -Compress
    }
} finally {foreach($taskKey in $taskKeys){[Environment]::SetEnvironmentVariable($taskKey,$taskOld[$taskKey],'Process')}}

