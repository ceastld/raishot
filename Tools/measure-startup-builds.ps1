[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BaselineWorker,
    [Parameter(Mandatory = $true)][string]$OptimizedWorker,
    [ValidateRange(1, 10)][int]$Count = 5,
    [Parameter(Mandatory = $true)][string]$Report,
    [ScriptBlock]$BeforeLaunch
)
$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
function Resolve-Worker([string]$Path) {
    if (-not [IO.Path]::IsPathFullyQualified($Path)) { throw '两版Worker必须使用绝对路径。' }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf) -or [IO.Path]::GetExtension($resolved) -ne '.exe') {
        throw 'Worker必须是已存在的Windows可执行文件。'
    }
    return $resolved
}
$taskWorkers = @{
    baseline = Resolve-Worker $BaselineWorker
    optimized = Resolve-Worker $OptimizedWorker
}
if ([StringComparer]::OrdinalIgnoreCase.Equals($taskWorkers.baseline, $taskWorkers.optimized)) {
    throw '两版Worker路径相同，无法做构建对照。'
}
if (-not [IO.Path]::IsPathFullyQualified($Report)) { throw 'Report必须使用绝对路径。' }
$taskReport = [IO.Path]::GetFullPath($Report)
$taskHashes = @{}
foreach ($taskMode in @('baseline', 'optimized')) {
    $taskHashes[$taskMode] = (Get-FileHash -LiteralPath $taskWorkers[$taskMode] -Algorithm SHA256).Hash
}
$taskProcessNames = @('raishot-gpui', [IO.Path]::GetFileNameWithoutExtension($taskWorkers.baseline),
    [IO.Path]::GetFileNameWithoutExtension($taskWorkers.optimized) | Select-Object -Unique)
$taskFolder = Join-Path $taskRoot ('target/qa/startup-builds/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null

if (-not ('RustShotStartupBuildQa' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class RustShotStartupBuildQa {
    [StructLayout(LayoutKind.Sequential)] struct LastInput { public uint Size, Tick; }
    [StructLayout(LayoutKind.Sequential)] struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern IntPtr OpenMutex(uint access, bool inherit, string name);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder path, ref uint count);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("user32.dll", SetLastError=true)] static extern bool EnumWindows(EnumProc callback, IntPtr context);
    delegate bool EnumProc(IntPtr window, IntPtr context);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    [DllImport("user32.dll", SetLastError=true)] static extern bool GetLastInputInfo(ref LastInput info);
    [DllImport("user32.dll", SetLastError=true)] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
    [DllImport("user32.dll", SetLastError=true)] static extern bool EnumDisplayMonitors(IntPtr dc, IntPtr clip, MonitorProc callback, IntPtr context);
    delegate bool MonitorProc(IntPtr monitor, IntPtr dc, ref Rect rect, IntPtr context);
    public static uint InputTick() {
        var info=new LastInput { Size=(uint)Marshal.SizeOf(typeof(LastInput)) };
        if(!GetLastInputInfo(ref info))throw new Win32Exception(Marshal.GetLastWin32Error(),"无法读取用户输入时间");
        return info.Tick;
    }
    public static int[] Desktop() {
        var previous=SetThreadDpiAwarenessContext(new IntPtr(-4));
        if(previous==IntPtr.Zero)throw new Win32Exception(Marshal.GetLastWin32Error(),"无法建立物理屏幕坐标上下文");
        try {
            var monitors=new List<int[]>();
            MonitorProc callback=delegate(IntPtr monitor,IntPtr dc,ref Rect rect,IntPtr context) {
                monitors.Add(new int[]{rect.Left,rect.Top,rect.Right,rect.Bottom});return true;
            };
            if(!EnumDisplayMonitors(IntPtr.Zero,IntPtr.Zero,callback,IntPtr.Zero))
                throw new Win32Exception(Marshal.GetLastWin32Error(),"无法核对物理显示器布局");
            monitors.Sort((left,right)=>{for(int i=0;i<4;++i){int order=left[i].CompareTo(right[i]);if(order!=0)return order;}return 0;});
            var layout=new List<int>{GetSystemMetrics(76),GetSystemMetrics(77),GetSystemMetrics(78),GetSystemMetrics(79),GetSystemMetrics(80)};
            foreach(var monitor in monitors)layout.AddRange(monitor);
            return layout.ToArray();
        } finally {
            if(SetThreadDpiAwarenessContext(previous)==IntPtr.Zero)
                throw new Win32Exception(Marshal.GetLastWin32Error(),"无法恢复测量线程DPI上下文");
        }
    }
    public static bool CaptureExists() {
        var mutex=OpenMutex(0x00100000,false,@"Local\RustShot.MVP.Capture");
        if(mutex!=IntPtr.Zero) {CloseHandle(mutex);return true;}
        int error=Marshal.GetLastWin32Error();
        if(error==2)return false;
        throw new Win32Exception(error,"不能可靠检查CaptureMutex");
    }
    public static string ImagePath(uint pid) {
        var process=OpenProcess(0x1000,false,pid);
        if(process==IntPtr.Zero)throw new Win32Exception(Marshal.GetLastWin32Error(),"无法核对自有worker路径");
        try {
            var text=new StringBuilder(32768); uint count=(uint)text.Capacity;
            if(!QueryFullProcessImageName(process,0,text,ref count))
                throw new Win32Exception(Marshal.GetLastWin32Error(),"无法读取自有worker路径");
            return text.ToString();
        } finally { CloseHandle(process); }
    }
    public static int OwnWindows(uint pid) {
        int count=0;
        if(!EnumWindows((window,context)=>{uint owner;GetWindowThreadProcessId(window,out owner);if(owner==pid)++count;return true;},IntPtr.Zero))
            throw new Win32Exception(Marshal.GetLastWin32Error(),"无法可靠检查自有窗口回收");
        return count;
    }
}
'@
}
function Assert-Idle {
    if (@(Get-Process -Name $taskProcessNames -ErrorAction SilentlyContinue).Count -ne 0 -or
        [RustShotStartupBuildQa]::CaptureExists()) {
        throw '已有截图、贴图或设置会话；保留现有进程，未继续测量。'
    }
}
function Assert-Binaries {
    foreach ($taskMode in @('baseline', 'optimized')) {
        if ((Get-FileHash -LiteralPath $taskWorkers[$taskMode] -Algorithm SHA256).Hash -ne $taskHashes[$taskMode]) {
            throw "测量期间 $taskMode 构建文件发生变化，保留已有日志。"
        }
    }
}
function Assert-Number($Value, [string]$Name) {
    if ($null -eq $Value) { throw "缺少计时：$Name" }
    $number = [double]$Value
    if ([double]::IsNaN($number) -or [double]::IsInfinity($number) -or $number -lt 0) { throw "无效计时：$Name" }
    return $number
}
function Get-Phase($Rows, [string]$Phase, [bool]$RequiresOk) {
    $found = @($Rows | Where-Object { $_.phase -eq $Phase })
    if ($found.Count -ne 1 -or ($RequiresOk -and $found[0].ok -ne $true)) { throw "缺少唯一成功证据：$Phase" }
    return $found[0]
}
function Median($Values) {
    $sorted = @($Values | Sort-Object)
    if ($sorted.Count -eq 0) { return $null }
    if ($sorted.Count % 2 -eq 1) { return $sorted[[int][Math]::Floor($sorted.Count / 2)] }
    return ($sorted[$sorted.Count / 2 - 1] + $sorted[$sorted.Count / 2]) / 2
}
function Save-Report($Value, [string]$Destination, [bool]$Replace = $false) {
    [IO.Directory]::CreateDirectory((Split-Path -Parent $Destination)) | Out-Null
    $temporary = $Destination + '.tmp-' + [Guid]::NewGuid().ToString('N')
    try {
        [IO.File]::WriteAllText($temporary, ($Value | ConvertTo-Json -Depth 16), [Text.UTF8Encoding]::new($false))
        [IO.File]::Move($temporary, $Destination, $Replace)
    } finally {
        if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
    }
}
function Read-Interference([string]$Stage) {
    $inputTick = [RustShotStartupBuildQa]::InputTick()
    $desktop = [RustShotStartupBuildQa]::Desktop()
    $inputChanged = $inputTick -ne $taskAudit.initial_input_tick
    $layoutChanged = ($desktop -join ',') -ne ($taskAudit.initial_desktop -join ',')
    $taskAudit.checks++
    $taskAudit.last_stage = $Stage
    $taskAudit.last_input_tick = $inputTick
    $taskAudit.last_desktop = $desktop
    if ($inputChanged) { $taskAudit.user_input_during_measurement = $true }
    if ($layoutChanged) { $taskAudit.desktop_layout_changed = $true }
    if (($inputChanged -or $layoutChanged) -and $null -eq $taskAudit.first_interference) {
        $taskAudit.first_interference = [pscustomobject]@{ stage = $Stage; input_tick = $inputTick;
            desktop = $desktop; qpc = [Diagnostics.Stopwatch]::GetTimestamp() }
    }
    if ($null -ne $taskCurrent) {
        $taskCurrent.audit.checks++
        $taskCurrent.audit.last_stage = $Stage
        $taskCurrent.audit.input_tick_after = $inputTick
        $taskCurrent.audit.desktop_after = $desktop
        if ($inputChanged) { $taskCurrent.audit.user_input_during_measurement = $true }
        if ($layoutChanged) { $taskCurrent.audit.desktop_layout_changed = $true }
    }
}
function Assert-Uninterrupted {
    if ($taskAudit.user_input_during_measurement -or $taskAudit.desktop_layout_changed) {
        $reason = if ($taskAudit.user_input_during_measurement) { 'user_input_during_measurement' } else { 'desktop_layout_changed' }
        if ($null -ne $taskCurrent) { $taskCurrent.excluded_reason = $reason }
        throw "$reason：测量受到干扰，本轮不统计，停止后续启动；自有进程仅自然等待，现有用户会话保持不动。"
    }
}

# 两版使用相同入口和运行时选项；额外专项覆盖项不会从调用者环境泄漏进来。
$taskEnvironment = @{
    RUSTSHOT_STARTUP_TRACE = '1'
    RUSTSHOT_STARTUP_SURFACE = '1'
    RUSTSHOT_STARTUP_SERIAL = '1'
    RUSTSHOT_CAPTURE_BACKEND = 'gdi'
    RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE = '0'
    RUSTSHOT_GPUI_LEGACY_BOUNDS = '0'
    RUSTSHOT_GPUI_SERIAL_FONTS = '0'
    RUSTSHOT_GPUI_EAGER_PATHS = '0'
    RUSTSHOT_SURFACE_QA_CASE = $null
    RUSTSHOT_DATA_DIR = $null
    RUSTSHOT_STARTUP_REQUEST_QPC = $null
}
$taskPrior = @{}
foreach ($taskName in $taskEnvironment.Keys) {
    $taskPrior[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process')
}
$taskRuns = @()
$taskFailure = $null
$taskCurrent = $null
Assert-Idle
$taskDesktop = [RustShotStartupBuildQa]::Desktop()
if ($taskDesktop[2] -le 0 -or $taskDesktop[3] -le 0 -or $taskDesktop[4] -le 0 -or
    $taskDesktop.Length -ne 5 + 4 * $taskDesktop[4]) { throw '物理桌面布局无效，未启动worker。' }
$taskInputTick = [RustShotStartupBuildQa]::InputTick()
$taskAudit = [pscustomobject]@{ initial_input_tick = $taskInputTick; last_input_tick = $taskInputTick;
    initial_desktop = $taskDesktop; last_desktop = $taskDesktop; checks = 0; last_stage = 'initial';
    user_input_during_measurement = $false; desktop_layout_changed = $false; first_interference = $null }
$taskSamplesReport = Join-Path $taskFolder 'samples.json'
try {
    for ($taskIteration = 1; $taskIteration -le $Count; $taskIteration++) {
        $taskOrder = if ($taskIteration % 2 -eq 1) { @('baseline', 'optimized') } else { @('optimized', 'baseline') }
        foreach ($taskMode in $taskOrder) {
            Assert-Idle
            Assert-Binaries
            Read-Interference 'before_launch'
            Assert-Uninterrupted
            $taskStem = Join-Path $taskFolder "$taskMode-$taskIteration"
            $taskSettings = "$taskStem-settings"
            [IO.Directory]::CreateDirectory($taskSettings) | Out-Null
            foreach ($taskName in $taskEnvironment.Keys) {
                [Environment]::SetEnvironmentVariable($taskName, $taskEnvironment[$taskName], 'Process')
            }
            [Environment]::SetEnvironmentVariable('RUSTSHOT_DATA_DIR', $taskSettings, 'Process')
            $taskCurrent = [ordered]@{ mode = $taskMode; iteration = $taskIteration; worker = $taskWorkers[$taskMode];
                sha256 = $taskHashes[$taskMode]; stdout = "$taskStem.jsonl"; stderr = "$taskStem.err"; settings = $taskSettings;
                audit = [pscustomobject]@{ input_tick_before = $taskInputTick; input_tick_after = $taskInputTick;
                    desktop_before = $taskDesktop; desktop_after = $taskDesktop; checks = 0; last_stage = 'before_request';
                    user_input_during_measurement = $false; desktop_layout_changed = $false } }
            Read-Interference 'before_request'
            Assert-Uninterrupted
            if ($null -ne $BeforeLaunch) {
                # 可由调用者恢复自有前台窗口；回调耗时不计入启动QPC。
                & $BeforeLaunch $taskMode $taskWorkers[$taskMode] | Out-Null
                $taskCurrent.before_launch_callback_completed = $true
                Assert-Idle
                Assert-Binaries
                Read-Interference 'after_before_launch_callback'
                Assert-Uninterrupted
            }
            # 与Rust启动专项共用系统QPC，包含CreateProcess与入口前DLL加载。
            $taskRequestQpc = [Diagnostics.Stopwatch]::GetTimestamp()
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_REQUEST_QPC', $taskRequestQpc.ToString(), 'Process')
            $taskProcess = Start-Process -FilePath $taskWorkers[$taskMode] -ArgumentList '--startup-probe' `
                -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru `
                -RedirectStandardOutput "$taskStem.jsonl" -RedirectStandardError "$taskStem.err"
            $taskCurrent.pid = $taskProcess.Id
            $taskCurrent.request_qpc = $taskRequestQpc
            $taskActualPath = [IO.Path]::GetFullPath([RustShotStartupBuildQa]::ImagePath([uint32]$taskProcess.Id))
            if (-not [StringComparer]::OrdinalIgnoreCase.Equals($taskActualPath, $taskWorkers[$taskMode])) {
                throw '新worker的精确PID路径不符；没有向该进程发送取消或结束请求。'
            }
            $taskCurrent.actual_worker = $taskActualPath
            $taskWait = [Diagnostics.Stopwatch]::StartNew()
            while (-not $taskProcess.WaitForExit(100)) {
                # 干扰只记录，先自然等待已启动的自有专项退出，不发送取消/强杀消息。
                Read-Interference 'waiting_for_exit'
                if ($taskWait.ElapsedMilliseconds -ge 15000) {
                    $taskCurrent.timed_out = $true
                    if ($taskAudit.user_input_during_measurement -or $taskAudit.desktop_layout_changed) {
                        $taskCurrent.excluded_reason = if ($taskAudit.user_input_during_measurement) { 'user_input_during_measurement' } else { 'desktop_layout_changed' }
                    }
                    throw "自有启动专项超时，PID=$($taskProcess.Id)；保留进程供检查，没有强制结束。"
                }
            }
            $taskProcess.Refresh()
            $taskCurrent.exit_code = $taskProcess.ExitCode
            Read-Interference 'after_exit'
            Assert-Uninterrupted
            if ($taskProcess.ExitCode -ne 0) { throw '启动专项返回失败，详见本轮原始stdout/stderr。' }
            $taskOutput = @(Get-Content -LiteralPath "$taskStem.jsonl" | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
            if ($taskOutput.Count -eq 0 -or @($taskOutput | Where-Object {
                $null -ne $_.pid -and $_.pid -ne $taskProcess.Id
            }).Count -ne 0) { throw '启动报告为空或含非自有PID。' }
            $taskEntry = Get-Phase $taskOutput 'startup_entry' $false
            $taskSurface = Get-Phase $taskOutput 'startup_surface_ready' $true
            $taskReady = Get-Phase $taskOutput 'startup_ready' $true
            $taskHandoff = Get-Phase $taskOutput 'startup_surface_handoff' $true
            $taskCancel = Get-Phase $taskOutput 'startup_input_cancel' $true
            $taskRecycle = Get-Phase $taskOutput 'startup_recycle' $true
            $taskBackend = Get-Phase $taskOutput 'startup_backend' $false
            foreach ($taskItem in @($taskEntry, $taskSurface, $taskReady)) {
                if ($taskItem.pid -ne $taskProcess.Id) { throw '入口或首屏报告PID与精确worker不符。' }
            }
            if ($taskSurface.foreground -ne $true -or $taskBackend.backend -ne 'gdi' -or
                $taskSurface.width -le 0 -or $taskSurface.height -le 0 -or
                $taskSurface.width -ne $taskReady.width -or $taskSurface.height -ne $taskReady.height -or
                $taskReady.width -ne $taskDesktop[2] -or $taskReady.height -ne $taskDesktop[3] -or $taskReady.screens -ne $taskDesktop[4] -or
                $taskReady.screens -le 0 -or $taskReady.scale -le 0 -or $taskReady.present_method -ne 'second_frame_dwm_flush') {
                throw '首屏前景/物理全屏尺寸/GPUI真实呈现或GDI后端不符合对照前提。'
            }
            if ($taskCancel.method -ne 'owned_escape' -or $taskRecycle.ready_verified -ne $true -or
                $taskRecycle.input_cancel_requested -ne $true -or $taskRecycle.capture_mutex_released -ne $true) {
                throw '没有经过自有ESC正常取消或真实回收。'
            }
            $taskMetrics = [ordered]@{
                request_to_entry_ms = Assert-Number $taskEntry.request_to_entry_ms 'request_to_entry_ms'
                request_to_surface_ready_ms = Assert-Number $taskSurface.request_to_ready_ms 'request_to_surface_ready_ms'
                surface_ready_ms = Assert-Number $taskSurface.ready_ms 'surface_ready_ms'
                start_surface_to_ready_ms = Assert-Number $taskSurface.start_surface_to_ready_ms 'start_surface_to_ready_ms'
                request_to_gpui_ready_ms = Assert-Number $taskReady.request_to_ready_ms 'request_to_gpui_ready_ms'
                gpui_ready_ms = Assert-Number $taskReady.ready_ms 'gpui_ready_ms'
                handoff_ms = Assert-Number $taskHandoff.elapsed_ms 'handoff_ms'
                recycle_ms = Assert-Number $taskRecycle.elapsed_ms 'recycle_ms'
                working_set_mib = Assert-Number $taskReady.working_set_mib 'working_set_mib'
                private_mib = Assert-Number $taskReady.private_mib 'private_mib'
            }
            if ($taskMetrics.request_to_entry_ms -gt $taskMetrics.request_to_surface_ready_ms -or
                $taskMetrics.request_to_surface_ready_ms -gt $taskMetrics.request_to_gpui_ready_ms -or
                $taskMetrics.handoff_ms -gt $taskMetrics.gpui_ready_ms -or
                $taskMetrics.gpui_ready_ms -gt $taskMetrics.recycle_ms) { throw '启动阶段时间顺序不一致。' }
            $taskOwnWindows = [RustShotStartupBuildQa]::OwnWindows([uint32]$taskProcess.Id)
            if ($taskOwnWindows -ne 0 -or [RustShotStartupBuildQa]::CaptureExists()) { throw '退出后自有窗口或CaptureMutex未释放。' }
            Assert-Binaries
            Assert-Idle
            Read-Interference 'after_validation'
            Assert-Uninterrupted
            $taskStages = @($taskOutput | Where-Object { $_.phase -eq 'startup_stage' })
            foreach ($taskStage in $taskStages) { $null = Assert-Number $taskStage.duration_ms "stage:$($taskStage.stage)" }
            $taskRow = [pscustomobject]@{ mode = $taskMode; iteration = $taskIteration; sequence = $taskRuns.Count + 1;
                pid = $taskProcess.Id; worker = $taskActualPath; sha256 = $taskHashes[$taskMode]; request_qpc = $taskRequestQpc;
                qpc_frequency = [Diagnostics.Stopwatch]::Frequency; settings = $taskSettings;
                entry = $taskEntry; surface = $taskSurface; ready = $taskReady; handoff = $taskHandoff; cancel = $taskCancel;
                recycle = $taskRecycle; metrics = [pscustomobject]$taskMetrics; stages = $taskStages; raw_events = $taskOutput;
                stdout = "$taskStem.jsonl"; stderr = "$taskStem.err"; exit_code = 0; own_windows_remaining = 0;
                capture_mutex_released = $true; audit = $taskCurrent.audit; ok = $true }
            $taskRuns += $taskRow
            $taskCurrent = $null
            # 每个已验证样本独立落盘，下一版失败不会丢掉有效baseline。
            Save-Report ([pscustomobject]@{ ok = $true; complete = $false; runs = $taskRuns }) $taskSamplesReport $true
            [pscustomobject]@{ mode = $taskMode; iteration = $taskIteration; pid = $taskProcess.Id;
                request_to_surface_ready_ms = $taskMetrics.request_to_surface_ready_ms;
                request_to_gpui_ready_ms = $taskMetrics.request_to_gpui_ready_ms; ok = $true } | ConvertTo-Json -Compress
        }
    }
} catch {
    # 阶段校验失败也先做退出后的输入审计，避免把真实用户操作误判为产品回归。
    $taskCaught = $_.Exception.Message
    try {
        Read-Interference 'failure_audit'
        if ($taskAudit.user_input_during_measurement -or $taskAudit.desktop_layout_changed) {
            Assert-Uninterrupted
        }
    } catch { $taskCaught = $_.Exception.Message }
    $taskFailure = $taskCaught
} finally {
    foreach ($taskName in $taskEnvironment.Keys) { [Environment]::SetEnvironmentVariable($taskName, $taskPrior[$taskName], 'Process') }
}

$taskStageNames = @($taskRuns | ForEach-Object { $_.stages | ForEach-Object { $_.stage } } | Sort-Object -Unique)
$taskSummary = @(
    foreach ($taskMode in @('baseline', 'optimized')) {
        $taskMatches = @($taskRuns | Where-Object { $_.mode -eq $taskMode })
        $taskMedians = [ordered]@{}
        if ($taskMatches.Count -gt 0) {
            foreach ($taskMetric in $taskMatches[0].metrics.PSObject.Properties.Name) {
                $taskMedians[$taskMetric] = Median @($taskMatches | ForEach-Object { $_.metrics.$taskMetric })
            }
        }
        $taskStageMedians = @(
            foreach ($taskStageName in $taskStageNames) {
                $taskDurations = @(
                    foreach ($taskMatch in $taskMatches) {
                        $taskEvents = @($taskMatch.stages | Where-Object { $_.stage -eq $taskStageName })
                        if ($taskEvents.Count -gt 0) { ($taskEvents | Measure-Object duration_ms -Sum).Sum }
                    }
                )
                if ($taskDurations.Count -gt 0) {
                    [pscustomobject]@{ stage = $taskStageName; samples = $taskDurations.Count;
                        median_ms = Median $taskDurations; aggregation = '同名阶段每轮求和，再取中位数' }
                }
            }
        )
        [pscustomobject]@{ mode = $taskMode; worker = $taskWorkers[$taskMode]; sha256 = $taskHashes[$taskMode];
            count = $taskMatches.Count; medians = [pscustomobject]$taskMedians; stage_medians = $taskStageMedians }
    }
)
$taskResult = [pscustomobject]@{ ok = ($null -eq $taskFailure); failure = $taskFailure; incomplete_run = $taskCurrent;
    count_per_build = $Count; launch_window_style = 'Hidden'; before_launch_callback = ($null -ne $BeforeLaunch);
    environment = $taskEnvironment; log_directory = $taskFolder;
    identical_binary_hash = ($taskHashes.baseline -eq $taskHashes.optimized); summary = $taskSummary; runs = $taskRuns;
    user_input_during_measurement = $taskAudit.user_input_during_measurement; desktop_layout_changed = $taskAudit.desktop_layout_changed;
    audit = $taskAudit; valid_samples_report = $taskSamplesReport; requested_report = $taskReport;
    method = '两版Release交替顺序；同一系统QPC包括进程创建及DLL加载；启动前/等待中/退出后审计真实输入tick和物理桌面逐屏布局，干扰样本排除并停止后续；原生首屏DwmFlush和前景、GPUI第二帧全屏视口/DPI/焦点、交接及自有ESC回收全部验证；不保存屏幕图像、不操作剪贴板' }
if ($null -ne $taskFailure) {
    $taskFailureReport = Join-Path $taskFolder 'failure.json'
    Save-Report $taskResult $taskFailureReport
    throw "$taskFailure 失败报告：$taskFailureReport；有效样本：$taskSamplesReport；既有Report未覆盖。"
}
Save-Report $taskResult $taskReport $true
$taskSummary | ConvertTo-Json -Depth 8
