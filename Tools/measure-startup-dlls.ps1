[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$EagerWorker,
    [Parameter(Mandatory = $true)][string]$DelayedWorker,
    [ValidateRange(1, 5)][int]$Count = 3,
    [string]$Report = ''
)
$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorkers = @{
    eager = (Resolve-Path -LiteralPath $EagerWorker).Path
    delayed = (Resolve-Path -LiteralPath $DelayedWorker).Path
}
$taskHashes = @{}
foreach ($taskMode in @('eager', 'delayed')) {
    if ([IO.Path]::GetExtension($taskWorkers[$taskMode]) -ine '.exe') { throw 'DLL 启动对照只接受两份 GPUI 工作进程 exe。' }
    $taskHashes[$taskMode] = (Get-FileHash -LiteralPath $taskWorkers[$taskMode] -Algorithm SHA256).Hash
}
if ($taskHashes.eager -eq $taskHashes.delayed) { throw '两份 exe 哈希相同，无法验证 DLL 延迟加载对照。' }
$taskProcessNames = @('raishot-gpui', [IO.Path]::GetFileNameWithoutExtension($taskWorkers.eager),
    [IO.Path]::GetFileNameWithoutExtension($taskWorkers.delayed)) | Select-Object -Unique

if (-not ('RustShotStartupDllQaNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class RustShotStartupDllQaNative {
    [StructLayout(LayoutKind.Sequential)] struct LastInput { public uint Size,Tick; }
    delegate bool EnumProc(IntPtr window,IntPtr data);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]
    static extern IntPtr OpenMutex(uint access,bool inherit,string name);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("user32.dll",SetLastError=true)] static extern bool GetLastInputInfo(ref LastInput info);
    [DllImport("user32.dll",SetLastError=true)] static extern bool EnumWindows(EnumProc callback,IntPtr data);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window,out uint pid);
    [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
    public static bool CaptureBusy() {
        var handle=OpenMutex(0x00100000,false,@"Local\RustShot.MVP.Capture");
        if(handle!=IntPtr.Zero){CloseHandle(handle);return true;}
        int error=Marshal.GetLastWin32Error();
        if(error==2)return false;
        throw new InvalidOperationException("无法确认截图互斥体状态，Win32="+error);
    }
    public static uint InputTick() {
        var info=new LastInput{Size=(uint)Marshal.SizeOf(typeof(LastInput))};
        if(!GetLastInputInfo(ref info))throw new InvalidOperationException("无法读取用户输入时间");
        return info.Tick;
    }
    public static int Windows(uint pid) {
        int count=0;
        if(!EnumWindows((window,data)=>{uint owner;GetWindowThreadProcessId(window,out owner);
            if(owner==pid)++count;return true;},IntPtr.Zero))
            throw new InvalidOperationException("无法确认自有窗口已经回收");
        return count;
    }
    public static int[] Desktop() {
        IntPtr previous=SetThreadDpiAwarenessContext(new IntPtr(-4));
        if(previous==IntPtr.Zero)throw new InvalidOperationException("无法读取物理屏幕尺寸");
        try{return new[]{GetSystemMetrics(76),GetSystemMetrics(77),GetSystemMetrics(78),GetSystemMetrics(79),GetSystemMetrics(80)};}
        finally{SetThreadDpiAwarenessContext(previous);}
    }
}
'@
}

function Assert-Idle {
    if (@(Get-Process -Name $taskProcessNames -ErrorAction SilentlyContinue).Count -ne 0 -or
        [RustShotStartupDllQaNative]::CaptureBusy()) {
        throw '已有截图、贴图或设置会话；未启动后续对照，也没有结束现有窗口。'
    }
}
function Assert-Hashes {
    foreach ($taskMode in @('eager', 'delayed')) {
        if ((Get-FileHash -LiteralPath $taskWorkers[$taskMode] -Algorithm SHA256).Hash -ne $taskHashes[$taskMode]) {
            throw "对照期间 $taskMode 构建文件发生变化。"
        }
    }
}
function One-Phase([object[]]$Trace, [string]$Phase) {
    $matches = @($Trace | Where-Object { $_.phase -eq $Phase })
    if ($matches.Count -ne 1) { throw "阶段 $Phase 必须恰好出现一次，实际 $($matches.Count)。" }
    return $matches[0]
}
function One-Stage([object[]]$Trace, [string]$Stage) {
    $matches = @($Trace | Where-Object { $_.phase -eq 'startup_stage' -and $_.stage -eq $Stage })
    if ($matches.Count -ne 1) { throw "计时 $Stage 必须恰好出现一次，实际 $($matches.Count)。" }
    return $matches[0]
}
function Number($Value, [string]$Name, [switch]$Positive) {
    if ($null -eq $Value) { throw "缺少数字：$Name。" }
    $result = [double]$Value
    if ([double]::IsNaN($result) -or [double]::IsInfinity($result) -or $result -lt 0 -or ($Positive -and $result -le 0)) {
        throw "数字无效：$Name=$Value。"
    }
    return $result
}
function Median($Values) {
    $sorted = @($Values | Sort-Object)
    if ($sorted.Count -eq 0) { return $null }
    if ($sorted.Count % 2 -eq 1) { return [double]$sorted[[int][math]::Floor($sorted.Count / 2)] }
    return ([double]$sorted[$sorted.Count / 2 - 1] + [double]$sorted[$sorted.Count / 2]) / 2
}

# 不触碰剪贴板/图像文件。只使用工作进程自己的 --startup-probe 自动 ESC 和自然退出。
Assert-Idle
$taskInputTick = [RustShotStartupDllQaNative]::InputTick()
$taskDesktop = [RustShotStartupDllQaNative]::Desktop()
if ($taskDesktop[2] -le 0 -or $taskDesktop[3] -le 0 -or $taskDesktop[4] -le 0) { throw '没有可测量的活动桌面。' }
$taskStamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$taskFolder = Join-Path $taskRoot "target/qa/startup-dlls/$taskStamp"
$taskSettings = Join-Path $taskRoot "target/qa/settings/startup-dlls/$taskStamp"
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null
if ([string]::IsNullOrWhiteSpace($Report)) {
    $taskReport = Join-Path $taskFolder 'report.json'
} else {
    $taskReport = [IO.Path]::GetFullPath($Report, $taskRoot)
}
$taskNames = @('RUSTSHOT_STARTUP_TRACE', 'RUSTSHOT_STARTUP_REQUEST_QPC',
    'RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE', 'RUSTSHOT_GPUI_LEGACY_BOUNDS', 'RUSTSHOT_GPUI_SERIAL_FONTS',
    'RUSTSHOT_GPUI_EAGER_PATHS', 'RUSTSHOT_GPUI_RESOURCE_RECOVERY_PROBE', 'RUSTSHOT_DATA_DIR',
    'RUSTSHOT_QA_NO_RESIDENT_NOTIFY', 'RUSTSHOT_CAPTURE_BACKEND', 'RUSTSHOT_STARTUP_SERIAL',
    'RUSTSHOT_STARTUP_SURFACE', 'RUSTSHOT_GDI_SINGLE_PASS', 'RUSTSHOT_SURFACE_QA_CASE', 'RUSTSHOT_SURFACE_QA_DIR')
$taskPrior = @{}
foreach ($taskName in $taskNames) { $taskPrior[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process') }
$taskRows = @()
$taskFailure = $null
$taskUserInput = $false
$taskCurrentLog = $null
try {
    for ($taskIteration = 1; $taskIteration -le $Count; $taskIteration++) {
        # AB / BA / AB，避免将缓存升温或固定启动位置归因于加载方式。
        $taskOrder = if ($taskIteration % 2 -eq 1) { @('eager', 'delayed') } else { @('delayed', 'eager') }
        foreach ($taskMode in $taskOrder) {
            Assert-Idle
            Assert-Hashes
            if ([RustShotStartupDllQaNative]::InputTick() -ne $taskInputTick) {
                $taskUserInput = $true
                throw '出现用户输入，停止后续启动对照。'
            }
            if (([RustShotStartupDllQaNative]::Desktop() -join ',') -ne ($taskDesktop -join ',')) { throw '屏幕布局发生变化，停止比较。' }
            $taskStem = Join-Path $taskFolder "$taskMode-$taskIteration"
            $taskCurrentLog = "$taskStem.log"
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_TRACE', '1', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE', '0', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_LEGACY_BOUNDS', '0', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_SERIAL_FONTS', '0', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_EAGER_PATHS', '0', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_RESOURCE_RECOVERY_PROBE', '0', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_DATA_DIR', (Join-Path $taskSettings "$taskMode-$taskIteration"), 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY', '1', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_CAPTURE_BACKEND', 'gdi', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_SERIAL', '1', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_SURFACE', '1', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GDI_SINGLE_PASS', '0', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_SURFACE_QA_CASE', $null, 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_SURFACE_QA_DIR', $null, 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_REQUEST_QPC', [Diagnostics.Stopwatch]::GetTimestamp().ToString(), 'Process')
            $taskProcess = Start-Process -FilePath $taskWorkers[$taskMode] -ArgumentList '--startup-probe' `
                -WorkingDirectory $taskRoot -WindowStyle Normal -PassThru `
                -RedirectStandardOutput "$taskStem.log" -RedirectStandardError "$taskStem.err"
            try {
                $taskDeadline = [Diagnostics.Stopwatch]::StartNew()
                while (-not $taskProcess.WaitForExit(100)) {
                    if ([RustShotStartupDllQaNative]::InputTick() -ne $taskInputTick) { $taskUserInput = $true }
                    if ($taskDeadline.Elapsed.TotalSeconds -ge 15) {
                        throw "自有启动专项超时，PID=$($taskProcess.Id)；保留窗口供检查，没有强制结束进程。"
                    }
                }
                $taskProcess.Refresh()
                if ([RustShotStartupDllQaNative]::InputTick() -ne $taskInputTick) { $taskUserInput = $true }
                if ($taskProcess.ExitCode -ne 0) { throw "$taskMode 启动专项退出码 $($taskProcess.ExitCode)，详见 $taskStem.err。" }
                if ([RustShotStartupDllQaNative]::Windows([uint32]$taskProcess.Id) -ne 0) { throw '自然退出后仍有自有原生窗口。' }
                if ([RustShotStartupDllQaNative]::CaptureBusy()) { throw '专项退出后 CaptureMutex 未释放。' }
                Assert-Idle
                Assert-Hashes
                if ($taskUserInput) { throw '本轮出现用户输入；自有专项已自然退出，停止后续比较且不统计受干扰样本。' }
                if (([RustShotStartupDllQaNative]::Desktop() -join ',') -ne ($taskDesktop -join ',')) { throw '本轮屏幕布局发生变化，不统计样本。' }
                $taskTrace = @(Get-Content -LiteralPath "$taskStem.log" -Encoding utf8 |
                    Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
                $taskEntry = One-Phase $taskTrace 'startup_entry'
                $taskNative = One-Phase $taskTrace 'startup_surface_ready'
                $taskReady = One-Phase $taskTrace 'startup_ready'
                $taskHandoff = One-Phase $taskTrace 'startup_surface_handoff'
                $taskRecycle = One-Phase $taskTrace 'startup_recycle'
                $taskCancel = One-Phase $taskTrace 'startup_input_cancel'
                $taskDevice = One-Phase $taskTrace 'gpui_device_mode'
                $taskFont = One-Phase $taskTrace 'gpui_font_mode'
                $taskPath = One-Phase $taskTrace 'gpui_path_mode'
                $taskFirst = One-Phase $taskTrace 'gpui_first_draw'
                $taskGeometry = One-Phase $taskTrace 'startup_window_geometry'
                $taskBackend = One-Phase $taskTrace 'startup_backend'
                # 正式版本只有逐屏 GDI 路径，没有已撤回的 single-pass 原型模式日志。
                # 兼容带该日志的旧构建，但有日志时必须明确为 per_monitor。
                $taskFreezeModes = @($taskTrace | Where-Object { $_.phase -eq 'gdi_freeze_mode' })
                if ($taskFreezeModes.Count -gt 1 -or ($taskFreezeModes.Count -eq 1 -and $taskFreezeModes[0].mode -ne 'per_monitor')) {
                    throw '冻结模式不是统一的逐屏 GDI，不能统计为 DLL 加载样本。'
                }
                $taskFreeze = One-Stage $taskTrace 'gdi_freeze'
                $taskApplication = One-Stage $taskTrace 'gpui_initialize'
                $taskWindow = One-Stage $taskTrace 'window_create'
                $taskCreations = @($taskTrace | Where-Object { $_.phase -eq 'startup_stage' -and
                    $_.stage -in @('gpui_d3d11_probe_create', 'gpui_d3d11_device_create') })
                if ($taskDevice.mode -ne 'reuse' -or $taskCreations.Count -ne 1 -or
                    $taskFont.mode -ne 'parallel' -or $taskPath.mode -ne 'lazy' -or
                    $taskFirst.paths -ne 0 -or $taskFirst.path_resources -ne $false -or
                    $taskGeometry.legacy -ne $false -or $taskBackend.backend -ne 'gdi') {
                    throw '实际设备/字体/路径/逐屏冻结模式不符合统一对照前提，保留日志但不统计样本。'
                }
                if ($taskEntry.pid -ne $taskProcess.Id -or $taskNative.pid -ne $taskProcess.Id -or
                    $taskReady.pid -ne $taskProcess.Id -or $taskNative.ok -ne $true -or
                    $taskNative.foreground -ne $true -or $taskReady.ok -ne $true -or
                    $taskHandoff.ok -ne $true -or $taskRecycle.ok -ne $true -or
                    $taskRecycle.capture_mutex_released -ne $true -or $taskRecycle.ready_verified -ne $true -or
                    $taskRecycle.input_cancel_requested -ne $true -or $taskCancel.ok -ne $true -or
                    $taskCancel.method -ne 'owned_escape' -or $taskReady.present_method -ne 'second_frame_dwm_flush') {
                    throw '缺少当前子进程首屏、成功交接、原生输入取消或真实回收证据。'
                }
                if ($taskReady.width -ne $taskDesktop[2] -or $taskReady.height -ne $taskDesktop[3] -or
                    $taskReady.screens -ne $taskDesktop[4] -or $taskNative.width -ne $taskReady.width -or
                    $taskNative.height -ne $taskReady.height -or $taskFirst.width -ne $taskReady.width -or
                    $taskFirst.height -ne $taskReady.height -or $taskGeometry.final_width -ne $taskReady.width -or
                    $taskGeometry.final_height -ne $taskReady.height) {
                    throw '原生首屏、完整 GPUI 首帧或显示器物理尺寸不一致。'
                }
                $taskEntryMs = Number $taskEntry.request_to_entry_ms 'request_to_entry_ms'
                $taskNativeMs = Number $taskNative.request_to_ready_ms 'native request_to_ready_ms' -Positive
                $taskNativeWorkerMs = Number $taskNative.ready_ms 'native ready_ms' -Positive
                $taskNativeInitMs = Number $taskNative.start_surface_to_ready_ms 'native init_ms'
                $taskGpuiMs = Number $taskReady.request_to_ready_ms 'gpui request_to_ready_ms' -Positive
                $taskGpuiWorkerMs = Number $taskReady.ready_ms 'gpui ready_ms' -Positive
                $taskHandoffMs = Number $taskHandoff.elapsed_ms 'handoff elapsed_ms'
                $taskFreezeMs = Number $taskFreeze.duration_ms 'gdi_freeze_ms'
                $taskApplicationMs = Number $taskApplication.duration_ms 'gpui_initialize_ms'
                $taskWindowMs = Number $taskWindow.duration_ms 'window_create_ms'
                $taskWs = Number $taskReady.working_set_mib 'working_set_mib' -Positive
                $taskPrivate = Number $taskReady.private_mib 'private_mib' -Positive
                if ($taskEntryMs -gt $taskNativeMs -or $taskNativeMs -gt $taskGpuiMs -or
                    $taskHandoffMs -lt $taskNativeWorkerMs -or $taskHandoffMs -gt $taskGpuiWorkerMs -or
                    $taskNativeWorkerMs -lt $taskFreezeMs -or $taskNativeInitMs -gt $taskNativeWorkerMs -or
                    $taskGpuiWorkerMs + 2 -lt $taskNativeWorkerMs + $taskApplicationMs -or
                    [math]::Abs(($taskNativeMs - $taskNativeWorkerMs) - ($taskGpuiMs - $taskGpuiWorkerMs)) -gt 5) {
                    throw 'QPC/worker 时间原点或原生优先初始化顺序不符合对照前提。'
                }
                $taskNativeIndex = [Array]::IndexOf($taskTrace, $taskNative)
                $taskDeviceIndex = [Array]::IndexOf($taskTrace, $taskDevice)
                if ($taskNativeIndex -lt 0 -or $taskNativeIndex -ge $taskDeviceIndex) { throw '实际没有先完成原生首屏再初始化 GPUI。' }
                $taskRow = [pscustomobject]@{
                    mode=$taskMode; iteration=$taskIteration; worker=$taskWorkers[$taskMode]; sha256=$taskHashes[$taskMode]
                    pid=$taskProcess.Id; request_to_entry_ms=$taskEntryMs; native_ready_ms=$taskNativeMs
                    gpui_ready_ms=$taskGpuiMs; freeze_ms=$taskFreezeMs; native_init_ms=$taskNativeInitMs
                    application_ms=$taskApplicationMs; window_create_ms=$taskWindowMs
                    working_set_mib=$taskWs; private_mib=$taskPrivate; device_creations=$taskCreations.Count
                    geometry=$taskGeometry; entry=$taskEntry; surface=$taskNative; handoff=$taskHandoff
                    ready=$taskReady; recycle=$taskRecycle
                    stages=@($taskTrace | Where-Object { $_.phase -eq 'startup_stage' })
                    log="$taskStem.log"; stderr="$taskStem.err"; ok=$true
                }
                $taskRows += $taskRow
                [pscustomobject]@{mode=$taskMode;iteration=$taskIteration;native_ready_ms=$taskNativeMs;gpui_ready_ms=$taskGpuiMs;ok=$true} | ConvertTo-Json -Compress
            } finally {
                $taskProcess.Dispose()
            }
        }
    }
} catch {
    $taskFailure = $_.Exception.Message
} finally {
    foreach ($taskName in $taskNames) { [Environment]::SetEnvironmentVariable($taskName, $taskPrior[$taskName], 'Process') }
}
$taskSummary = @('eager', 'delayed') | ForEach-Object {
    $taskMode = $_
    $taskMatches = @($taskRows | Where-Object { $_.mode -eq $taskMode })
    [pscustomobject]@{
        mode=$taskMode; worker=$taskWorkers[$taskMode]; sha256=$taskHashes[$taskMode]; count=$taskMatches.Count
        request_to_entry_median_ms=(Median @($taskMatches | ForEach-Object { $_.request_to_entry_ms }))
        native_ready_median_ms=(Median @($taskMatches | ForEach-Object { $_.native_ready_ms }))
        gpui_ready_median_ms=(Median @($taskMatches | ForEach-Object { $_.gpui_ready_ms }))
        freeze_median_ms=(Median @($taskMatches | ForEach-Object { $_.freeze_ms }))
        native_init_median_ms=(Median @($taskMatches | ForEach-Object { $_.native_init_ms }))
        application_median_ms=(Median @($taskMatches | ForEach-Object { $_.application_ms }))
        window_create_median_ms=(Median @($taskMatches | ForEach-Object { $_.window_create_ms }))
        working_set_median_mib=(Median @($taskMatches | ForEach-Object { $_.working_set_mib }))
        private_median_mib=(Median @($taskMatches | ForEach-Object { $_.private_mib }))
    }
}
$taskDelta = @{}
if ($taskSummary[0].count -gt 0 -and $taskSummary[1].count -gt 0) {
    foreach ($taskMetric in @('request_to_entry_median_ms','native_ready_median_ms','gpui_ready_median_ms',
        'freeze_median_ms','native_init_median_ms','application_median_ms','window_create_median_ms',
        'working_set_median_mib','private_median_mib')) {
        $taskDelta[$taskMetric] = [double]$taskSummary[1].$taskMetric - [double]$taskSummary[0].$taskMetric
    }
}
$taskResult = [pscustomobject]@{
    ok=($null -eq $taskFailure -and $taskRows.Count -eq 2 * $Count)
    error=$taskFailure; user_input_observed=$taskUserInput; failed_log=$taskCurrentLog
    eager_worker=$taskWorkers.eager; eager_sha256=$taskHashes.eager
    delayed_worker=$taskWorkers.delayed; delayed_sha256=$taskHashes.delayed
    launch_window_style='Normal'; freeze='per_monitor'; native_surface=$true; serial=$true
    font_mode='parallel'; path_mode='lazy'; desktop=$taskDesktop; settings_root=$taskSettings
    method='不同 exe 交替 AB/BA；QPC 含进程创建/DLL加载至原生可交互首屏及完整 GPUI；只等待自有 ESC 自然退出；内存为完整 GPUI 就绪时读数。'
    delta_sign='delayed - eager；负值表示减少'; delayed_minus_eager=$taskDelta
    summary=@($taskSummary); runs=$taskRows
}
[IO.Directory]::CreateDirectory((Split-Path -Parent $taskReport)) | Out-Null
$taskResult | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $taskReport -Encoding utf8
[pscustomobject]@{ok=$taskResult.ok;report=$taskReport;summary=@($taskSummary);delayed_minus_eager=$taskDelta} | ConvertTo-Json -Depth 4
if ($null -ne $taskFailure) { throw "$taskFailure 报告：$taskReport" }
