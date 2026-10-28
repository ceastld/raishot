[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Worker,
    [ValidateRange(1, 5)][int]$Count = 3,
    [ValidateSet('Hidden','Normal')][string]$ProcessWindowStyle = 'Normal',
    [string]$Report = 'target/qa/latency-ab.json',
    [switch]$CompareLegacy,
    [switch]$CompareFonts,
    [switch]$ComparePaths,
    [switch]$CompareSurface,
    [switch]$PrioritizeSurface
)
$ErrorActionPreference = 'Stop'
if (@($CompareLegacy, $CompareFonts, $ComparePaths, $CompareSurface).Where({ $_ }).Count -gt 1) { throw '一次专项只比较一种初始化优化。' }
if ($PrioritizeSurface -and -not $CompareSurface) { throw '首屏初始化顺序专项必须搭配 CompareSurface。' }
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
$taskWorkerHash = (Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash
$taskReport = [IO.Path]::GetFullPath($Report, $taskRoot)
$taskFolder = Join-Path $taskRoot ('target/qa/latency/' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null

if (-not ('RustShotLatencyMutex' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class RustShotLatencyMutex {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern IntPtr OpenMutex(uint access, bool inherit, string name);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    public static bool Busy() {
        var handle=OpenMutex(0x00100000,false,"Local\\RustShot.MVP.Capture");
        if(handle!=IntPtr.Zero) {CloseHandle(handle);return true;}
        int error=Marshal.GetLastWin32Error();
        if(error==2)return false;
        throw new InvalidOperationException("无法确认截图互斥体状态："+error);
    }
}
'@
}
function Assert-Idle {
    if (@(Get-Process -Name 'raishot-gpui' -ErrorAction SilentlyContinue).Count -ne 0 -or [RustShotLatencyMutex]::Busy()) {
        throw '已有截图/贴图/设置会话；没有启动测量，也没有结束现有窗口。'
    }
}
function Median($values) {
    $sorted = @($values | Sort-Object)
    if ($sorted.Count % 2 -eq 1) { return $sorted[[int][math]::Floor($sorted.Count / 2)] }
    return ($sorted[$sorted.Count / 2 - 1] + $sorted[$sorted.Count / 2]) / 2
}

$taskNames = @('RUSTSHOT_STARTUP_TRACE', 'RUSTSHOT_STARTUP_REQUEST_QPC', 'RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE', 'RUSTSHOT_GPUI_LEGACY_BOUNDS', 'RUSTSHOT_GPUI_SERIAL_FONTS', 'RUSTSHOT_GPUI_EAGER_PATHS', 'RUSTSHOT_DATA_DIR', 'RUSTSHOT_QA_NO_RESIDENT_NOTIFY', 'RUSTSHOT_CAPTURE_BACKEND', 'RUSTSHOT_STARTUP_SERIAL', 'RUSTSHOT_STARTUP_SURFACE')
$taskPrevious = @{}
foreach ($name in $taskNames) { $taskPrevious[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$taskModes = @(if ($CompareSurface) { 'gpui_only', 'optimized' } elseif ($CompareLegacy) { 'legacy', 'reuse_device', 'optimized' } elseif ($CompareFonts) { 'serial_fonts', 'optimized' } elseif ($ComparePaths) { 'eager_paths', 'optimized' } else { 'optimized' })
$taskRows = @()
try {
    for ($taskIteration = 1; $taskIteration -le $Count; $taskIteration++) {
        # 轮换首位，减少缓存与固定顺序的影响。
        $taskOrder = @(for ($taskPosition=0; $taskPosition -lt $taskModes.Count; $taskPosition++) { $taskModes[($taskPosition + $taskIteration - 1) % $taskModes.Count] })
        foreach ($taskMode in $taskOrder) {
            Assert-Idle
            if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskWorkerHash) { throw '测量前构建文件已变化。' }
            $taskStem = Join-Path $taskFolder "$taskMode-$taskIteration"
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_TRACE', '1', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE', $(if ($taskMode -eq 'legacy') { '1' } else { '0' }), 'Process')
            $taskLegacyBounds = $taskMode -in @('legacy', 'reuse_device')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_LEGACY_BOUNDS', $(if ($taskLegacyBounds) { '1' } else { '0' }), 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_SERIAL_FONTS', $(if ($taskMode -eq 'serial_fonts') { '1' } else { '0' }), 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_GPUI_EAGER_PATHS', $(if ($taskMode -eq 'eager_paths') { '1' } else { '0' }), 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_DATA_DIR', "$taskStem-settings", 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY', '1', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_CAPTURE_BACKEND', 'gdi', 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_SERIAL', $(if ($CompareSurface -and $taskMode -eq 'optimized') { if ($PrioritizeSurface) { '1' } else { $null } } else { '0' }), 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_SURFACE', $(if ($CompareSurface -and $taskMode -eq 'optimized') { '1' } else { '0' }), 'Process')
            [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_REQUEST_QPC', [Diagnostics.Stopwatch]::GetTimestamp().ToString(), 'Process')
            $taskProcess = Start-Process -FilePath $taskWorker -ArgumentList '--startup-probe' -WorkingDirectory $taskRoot -WindowStyle $ProcessWindowStyle -PassThru -RedirectStandardOutput "$taskStem.log" -RedirectStandardError "$taskStem.err"
            if (-not $taskProcess.WaitForExit(15000)) { throw "自有启动专项没有按时结束，PID=$($taskProcess.Id)；保留窗口供检查。" }
            $taskProcess.Refresh()
            if ($taskProcess.ExitCode -ne 0) { throw "启动专项失败：$taskMode，详见 $taskStem.log / .err" }
            $taskOutput = @(Get-Content -LiteralPath "$taskStem.log" | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
            $taskReady = @($taskOutput | Where-Object { $_.phase -eq 'startup_ready' })
            $taskRecycle = @($taskOutput | Where-Object { $_.phase -eq 'startup_recycle' })
            if ($taskReady.Count -ne 1 -or $taskRecycle.Count -ne 1 -or -not $taskReady[0].ok -or -not $taskRecycle[0].ok -or $null -eq $taskReady[0].request_to_ready_ms -or $taskReady[0].pid -ne $taskProcess.Id) { throw '缺少可信的首帧/原生取消/回收证据。' }
            $taskDevice = @($taskOutput | Where-Object { $_.phase -eq 'gpui_device_mode' })
            $taskBackend = @($taskOutput | Where-Object { $_.phase -eq 'startup_backend' })
            $taskFontMode = @($taskOutput | Where-Object { $_.phase -eq 'gpui_font_mode' })
            $taskPathMode = @($taskOutput | Where-Object { $_.phase -eq 'gpui_path_mode' })
            $taskFirstDraw = @($taskOutput | Where-Object { $_.phase -eq 'gpui_first_draw' })
            $taskEntry = @($taskOutput | Where-Object { $_.phase -eq 'startup_entry' })
            $taskSurface = @($taskOutput | Where-Object { $_.phase -eq 'startup_surface_ready' })
            $taskHandoff = @($taskOutput | Where-Object { $_.phase -eq 'startup_surface_handoff' })
            if ($CompareSurface -and $taskMode -eq 'optimized' -and ($taskSurface.Count -ne 1 -or -not $taskSurface[0].ok -or $null -eq $taskSurface[0].request_to_ready_ms -or $taskHandoff.Count -ne 1 -or -not $taskHandoff[0].ok)) { throw '缺少原生可交互首屏呈现或 GPUI 交接证据。' }
            if ($taskMode -eq 'gpui_only' -and $taskSurface.Count -ne 0) { throw 'GPUI对照意外打开了原生首屏。' }
            if ($ComparePaths) {
                $taskEagerPaths = $taskMode -eq 'eager_paths'
                $taskExpectedPathMode = if ($taskEagerPaths) { 'eager' } else { 'lazy' }
                if ($taskPathMode.Count -ne 1 -or $taskPathMode[0].mode -ne $taskExpectedPathMode -or $taskFirstDraw.Count -ne 1 -or $taskFirstDraw[0].paths -ne 0 -or $taskFirstDraw[0].path_resources -ne $taskEagerPaths) {
                    throw '首帧未走预期的路径资源分配方式，或首帧已有路径；保留日志，不统计为按需分配样本。'
                }
                if ($taskEntry.Count -ne 1 -or $null -eq $taskEntry[0].request_to_entry_ms) { throw '缺少请求到工作进程入口的计时。' }
            }
            if ($CompareFonts) {
                $taskExpectedFontMode = if ($taskMode -eq 'serial_fonts') { 'serial' } else { 'parallel' }
                if ($taskFontMode.Count -ne 1 -or $taskFontMode[0].mode -ne $taskExpectedFontMode) {
                    throw '字体初始化未走预期路径；保留回退日志，不把它统计为并行样本。'
                }
            }
            $taskGeometry = @($taskOutput | Where-Object { $_.phase -eq 'startup_window_geometry' })
            $taskCreations = @($taskOutput | Where-Object { $_.phase -eq 'startup_stage' -and $_.stage -in @('gpui_d3d11_probe_create','gpui_d3d11_device_create') })
            $taskExpectedDevice = if($taskMode -eq 'legacy') { 'legacy_probe' } else { 'reuse' }
            $taskExpectedCreations = if($taskMode -eq 'legacy') { 2 } else { 1 }
            # 本机首适配器成功的专项：若有失败适配器额外尝试，停止统计供人工检查，不当作应用回退失败。
            if($taskDevice.Count -ne 1 -or $taskDevice[0].mode -ne $taskExpectedDevice -or $taskGeometry.Count -ne 1 -or $taskGeometry[0].legacy -ne $taskLegacyBounds -or $taskCreations.Count -ne $taskExpectedCreations -or $taskBackend.Count -ne 1 -or $taskBackend[0].backend -ne 'gdi') { throw '本机对照前提不满足：路径/创建次数/GDI后端有变化；检查适配器尝试与日志，未判定产品回退失败。' }
            if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskWorkerHash) { throw '测量期间构建文件已变化。' }
            Assert-Idle
            $taskRow = [pscustomobject]@{ mode=$taskMode; iteration=$taskIteration; backend=$taskBackend[0].backend; device_mode=$taskDevice[0].mode; font_mode=$(if($taskFontMode.Count -eq 1){$taskFontMode[0].mode}else{$null}); path_mode=$(if($taskPathMode.Count -eq 1){$taskPathMode[0].mode}else{$null}); first_draw=$(if($taskFirstDraw.Count -eq 1){$taskFirstDraw[0]}else{$null}); entry=$(if($taskEntry.Count -eq 1){$taskEntry[0]}else{$null}); device_creations=$taskCreations.Count; geometry=$taskGeometry[0]; ready=$taskReady[0]; stages=@($taskOutput | Where-Object { $_.phase -eq 'startup_stage' }); recycle=$taskRecycle[0]; log="$taskStem.log" }
            $taskRow | Add-Member -NotePropertyName surface -NotePropertyValue $(if($taskSurface.Count -eq 1){$taskSurface[0]}else{$null})
            $taskRow | Add-Member -NotePropertyName handoff -NotePropertyValue $(if($taskHandoff.Count -eq 1){$taskHandoff[0]}else{$null})
            $taskRow | Add-Member -NotePropertyName actionable_ms -NotePropertyValue $(if($taskSurface.Count -eq 1){$taskSurface[0].request_to_ready_ms}else{$taskReady[0].request_to_ready_ms})
            $taskRows += $taskRow
            [pscustomobject]@{mode=$taskMode;iteration=$taskIteration;request_to_ready_ms=$taskReady[0].request_to_ready_ms;ready_ms=$taskReady[0].ready_ms;ok=$true} | ConvertTo-Json -Compress
        }
    }
} finally {
    foreach ($name in $taskNames) { [Environment]::SetEnvironmentVariable($name, $taskPrevious[$name], 'Process') }
}
$taskSummary = @($taskModes | ForEach-Object {
    $taskMode = $_
    $taskMatches = @($taskRows | Where-Object { $_.mode -eq $taskMode })
    $taskEntryTimes = @($taskMatches | Where-Object { $null -ne $_.entry.request_to_entry_ms } | ForEach-Object { $_.entry.request_to_entry_ms })
    [pscustomobject]@{mode=$taskMode;count=$taskMatches.Count;actionable_median_ms=(Median @($taskMatches | ForEach-Object { $_.actionable_ms }));request_to_ready_median_ms=(Median @($taskMatches | ForEach-Object { $_.ready.request_to_ready_ms }));ready_median_ms=(Median @($taskMatches | ForEach-Object { $_.ready.ready_ms }));request_to_entry_median_ms=$(if($taskEntryTimes.Count -gt 0){Median $taskEntryTimes}else{$null});window_create_median_ms=(Median @($taskMatches | ForEach-Object { $_.stages | Where-Object { $_.stage -eq 'window_create' } | ForEach-Object { $_.duration_ms } }));working_set_median_mib=(Median @($taskMatches | ForEach-Object { $_.ready.working_set_mib }));private_median_mib=(Median @($taskMatches | ForEach-Object { $_.ready.private_mib }))}
})
$taskResult = [pscustomobject]@{ok=$true;worker=$taskWorker;sha256=$taskWorkerHash;launch_window_style=$ProcessWindowStyle;prioritize_surface=[bool]$PrioritizeSurface;method='同一Release轮换路径位置；系统QPC包含进程创建/DLL加载；第二帧+DwmFlush首帧上界；验证设备创建次数/实际模式/物理尺寸/GPUI视口/原生焦点；自有ESC取消与CaptureMutex释放';summary=$taskSummary;runs=$taskRows}
[IO.Directory]::CreateDirectory((Split-Path -Parent $taskReport)) | Out-Null
$taskResult | ConvertTo-Json -Depth 9 | Set-Content -LiteralPath $taskReport -Encoding utf8
$taskSummary | ConvertTo-Json -Depth 3
