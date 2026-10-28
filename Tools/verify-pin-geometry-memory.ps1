param(
    [ValidateSet('reuse', 'borrowed', 'shared')][string]$Mode = 'reuse',
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug', 'release')][string]$Configuration = 'release',
    [string]$BuildVerification = 'target/qa/pin-geometry-build-verification.json'
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'qa-source-manifest.ps1')
$taskBuildPath = (Resolve-Path -LiteralPath (Join-Path $taskRoot $BuildVerification)).Path
$taskBuildHash = (Get-FileHash -LiteralPath $taskBuildPath -Algorithm SHA256).Hash
$taskBuild = Get-Content -Raw -LiteralPath $taskBuildPath | ConvertFrom-Json
if (-not $taskBuild.ok -or $taskBuild.configuration -cne $Configuration) { throw '本轮来源与构建证明不符' }
$taskSources = Get-RustShotQaSourceManifest -Root $taskRoot
$taskManifest = ConvertTo-Json -InputObject @($taskSources) -Depth 4
$taskBuiltSourcePath = (Resolve-Path -LiteralPath $taskBuild.source_manifest).Path
$taskBuiltSourceHash = (Get-FileHash -LiteralPath $taskBuiltSourcePath -Algorithm SHA256).Hash
if ($taskBuiltSourceHash -cne $taskBuild.source_manifest_sha256) { throw '构建来源清单本身的 SHA 不符' }
$taskBuiltSources = Get-Content -Raw -LiteralPath $taskBuiltSourcePath | ConvertFrom-Json
if ($taskSources.Count -ne $taskBuiltSources.Count -or ($taskManifest -cne (ConvertTo-Json -InputObject @($taskBuiltSources) -Depth 4))) {
    throw '本轮完整来源已变化，不能使用旧构建运行几何验收'
}
$taskExe = (Resolve-Path -LiteralPath (Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe")).Path
$taskExeDirectory = Split-Path -Parent $taskExe
$taskNames = @('raishot.exe', 'raishot-gpui.exe', 'raishot-cli.exe')
if (@($taskBuild.files).Count -ne 3 -or @($taskBuild.files.path | Sort-Object -Unique).Count -ne 3) { throw '构建证明必须恰含三个唯一程序' }
foreach ($taskName in $taskNames) {
    if (@($taskBuild.files | Where-Object { (Split-Path -Leaf $_.path) -ieq $taskName }).Count -ne 1) { throw "缺少唯一 $taskName" }
}
foreach ($taskFile in $taskBuild.files) {
    $taskActualFile = Get-Item -LiteralPath $taskFile.path
    if ($taskActualFile.DirectoryName -ine $taskExeDirectory -or $taskActualFile.Length -ne $taskFile.bytes -or (Get-FileHash -LiteralPath $taskFile.path -Algorithm SHA256).Hash -cne $taskFile.sha256) { throw '同目录三件套的路径、长度或 SHA 不符' }
}
if (@($taskBuild.files | Where-Object { $_.path -ieq $taskExe }).Count -ne 1) { throw '验收 exe 不属于本轮构建三件套' }
if (@(Get-CimInstance Win32_Process -Filter "Name='raishot-gpui.exe'" | Where-Object {
    $_.ExecutablePath -like "$taskRoot\*" -and $_.CommandLine -match '--(long-runtime-memory|long-browser-stress|long-browser|long-dxgi-memory)-smoke'
}).Count -ne 0) { throw '已有本仓库原生内存验收进程，未重复启动' }

if ($null -eq ([System.Management.Automation.PSTypeName]'RustShotPinGeometryCounters').Type) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class RustShotPinGeometryCounters {
    [StructLayout(LayoutKind.Sequential)]
    public struct Counters {
        public uint cb, PageFaultCount;
        public UIntPtr PeakWorkingSetSize, WorkingSetSize, QuotaPeakPagedPoolUsage,
            QuotaPagedPoolUsage, QuotaPeakNonPagedPoolUsage, QuotaNonPagedPoolUsage,
            PagefileUsage, PeakPagefileUsage, PrivateUsage;
    }
    [DllImport("psapi.dll", SetLastError=true)]
    private static extern bool GetProcessMemoryInfo(SafeProcessHandle h, ref Counters c, uint size);
    [DllImport("kernel32.dll", SetLastError=true)]
    private static extern bool GetProcessTimes(SafeProcessHandle h, out long creation, out long exit, out long kernel, out long user);
    public static Counters Read(SafeProcessHandle h) {
        Counters c = new Counters();
        c.cb = (uint)Marshal.SizeOf(typeof(Counters));
        if (!GetProcessMemoryInfo(h, ref c, c.cb))
            throw new Win32Exception(Marshal.GetLastWin32Error());
        return c;
    }
    public static long Creation(SafeProcessHandle h) {
        long c, e, k, u;
        if (!GetProcessTimes(h, out c, out e, out k, out u))
            throw new Win32Exception(Marshal.GetLastWin32Error());
        return c;
    }
}
'@
}
$taskFolder = Join-Path $taskRoot ('target/qa/pin-geometry-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff') + '-' + $Mode)
[void](New-Item -ItemType Directory -Path $taskFolder)
$taskManifest | Set-Content -LiteralPath (Join-Path $taskFolder 'source-hashes.json') -Encoding utf8
$taskVariables = @('RUSTSHOT_LONG_RUNTIME_QA_DIR', 'RUSTSHOT_LONG_RUNTIME_GROUPS', 'RUSTSHOT_DATA_DIR', 'RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskOld = @{}
foreach ($taskName in $taskVariables) { $taskOld[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process') }
$taskSamples = [Collections.Generic.List[object]]::new()
$taskClock = [Diagnostics.Stopwatch]::StartNew()
$taskProcess = $null
$taskForced = $false
$taskPeakPrivate = 0L
$taskPeakWorking = 0L
$taskMaxGap = 0.0
$taskLastMs = $null
$taskSamplingErrors = [Collections.Generic.List[string]]::new()
$taskTerminalSampleRaces = [Collections.Generic.List[object]]::new()
try {
    $env:RUSTSHOT_LONG_RUNTIME_QA_DIR = $taskFolder
    $env:RUSTSHOT_LONG_RUNTIME_GROUPS = "geometry-$Mode"
    $env:RUSTSHOT_DATA_DIR = Join-Path $taskFolder 'settings'
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--long-runtime-memory-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskFolder 'stdout.log') -RedirectStandardError (Join-Path $taskFolder 'stderr.log')
    # 保留此 Start-Process 产生的 SafeHandle；之后按同一句柄采样，不按 PID 重新附加。
    $taskHandle = $taskProcess.SafeHandle
    $taskCreation = [RustShotPinGeometryCounters]::Creation($taskHandle)
    if ($taskCreation -ne $taskProcess.StartTime.ToUniversalTime().ToFileTimeUtc()) { throw '原进程句柄与启动时间不符' }
    while (-not $taskProcess.WaitForExit(100)) {
        $taskMs = $taskClock.Elapsed.TotalMilliseconds
        if ($null -ne $taskLastMs) { $taskMaxGap = [Math]::Max($taskMaxGap, $taskMs - $taskLastMs) }
        $taskLastMs = $taskMs
        try {
            $taskMemory = [RustShotPinGeometryCounters]::Read($taskHandle)
            $taskPrivate = [long]$taskMemory.PrivateUsage.ToUInt64()
            $taskWorking = [long]$taskMemory.WorkingSetSize.ToUInt64()
            $taskPeakPrivate = [Math]::Max($taskPeakPrivate, $taskPrivate)
            $taskPeakWorking = [Math]::Max($taskPeakWorking, $taskWorking)
            $taskSamples.Add([pscustomobject]@{utc = [DateTime]::UtcNow.ToString('o'); elapsed_ms = $taskMs; private_bytes = $taskPrivate; working_set_bytes = $taskWorking})
        } catch {
            # Read 与自然退出可交错；仅原受控句柄的进程已终止时记终端诊断。
            if ($taskProcess.HasExited) {
                $taskTerminalSampleRaces.Add([pscustomobject]@{elapsed_ms = $taskMs; error = $_.Exception.Message; original_process_exited = $true})
            } else { $taskSamplingErrors.Add($_.Exception.Message) }
        }
        if ($taskMs -ge 600000) { throw "自有几何工作进程超时：$($taskProcess.Id)，记录 $taskFolder" }
    }
    $taskProcess.WaitForExit()
    $taskReport = Get-Content -Raw -LiteralPath (Join-Path $taskFolder 'report.json') | ConvertFrom-Json
    if ($taskProcess.ExitCode -ne 0 -or -not $taskReport.ok) { throw "几何检查失败：$($taskReport.error)，exit=$($taskProcess.ExitCode)，记录 $taskFolder" }
    if ($taskReport.pid -ne $taskProcess.Id -or $taskReport.suite -cne 'long-runtime-memory' -or @($taskReport.requested_groups).Count -ne 1 -or $taskReport.requested_groups[0] -cne "geometry-$Mode") { throw '报告不属于原自有进程或请求组' }
    $taskExpectedGroup = if ($Mode -ceq 'shared') { 'shared-png-pin-geometry-buffer' } else { "three-rounds-32000px-pin-geometry-$Mode" }
    if (@($taskReport.groups).Count -ne 1 -or -not $taskReport.groups[0].ok -or $taskReport.groups[0].name -cne $taskExpectedGroup) { throw '几何检查组不完整' }
    if ($taskReport.clipboard_untouched -ne $true -or $taskReport.real_dxgi_capture -ne $false) { throw '验收范围不符' }
    if ($taskSamples.Count -lt 3 -or $taskSamplingErrors.Count -ne 0) { throw '原句柄运行内存采样不完整' }
    if ($taskManifest -cne (ConvertTo-Json -InputObject @(Get-RustShotQaSourceManifest -Root $taskRoot) -Depth 4)) { throw '原生检查期间来源变化' }
    if ((Get-FileHash -LiteralPath $taskBuildPath -Algorithm SHA256).Hash -cne $taskBuildHash -or (Get-FileHash -LiteralPath $taskBuiltSourcePath -Algorithm SHA256).Hash -cne $taskBuiltSourceHash) { throw '原生检查期间构建证明或来源清单已变化' }
    foreach ($taskFile in $taskBuild.files) {
        if ((Get-Item -LiteralPath $taskFile.path).Length -ne $taskFile.bytes -or (Get-FileHash -LiteralPath $taskFile.path -Algorithm SHA256).Hash -cne $taskFile.sha256) { throw '原生检查期间同目录三件套变化' }
    }
    [pscustomobject]@{
        ok = $true; suite = 'pin-geometry-memory-runner'; mode = $Mode
        configuration = $Configuration; final_release = ($Configuration -eq 'release')
        build_proof = $taskBuildPath; build_proof_sha256 = $taskBuildHash
        built_source_manifest = $taskBuiltSourcePath; built_source_manifest_sha256 = $taskBuiltSourceHash
        source_manifest = (Join-Path $taskFolder 'source-hashes.json'); source_count = $taskSources.Count
        files = $taskBuild.files; worker_pid = $taskProcess.Id; worker_creation_filetime = $taskCreation
        worker_exit_code = $taskProcess.ExitCode; forced_cleanup = $false; native_handle_retained = $true
        sample_target_interval_ms = 100; interval_is_target_not_exact = $true
        peak_is_sampled_not_continuous = $true
        actual_sample_count = $taskSamples.Count; maximum_actual_gap_ms = $taskMaxGap
        terminal_exit_sampling_races = @($taskTerminalSampleRaces.ToArray())
        peak_private_bytes = $taskPeakPrivate; peak_working_set_bytes = $taskPeakWorking
        report = (Join-Path $taskFolder 'report.json'); report_sha256 = (Get-FileHash -LiteralPath (Join-Path $taskFolder 'report.json')).Hash
        real_dxgi_capture = $false; clipboard_untouched = $true
        limitations = @('自有非均色合成源；不代表浏览器滚动/第三方应用/混合DPI/长期无泄漏', 'borrowed是本版的受控借用分支，不是旧程序的实测')
    } | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath (Join-Path $taskFolder 'runner-report.json') -Encoding utf8
    Write-Output $taskFolder
} catch {
    [pscustomobject]@{ok = $false; error = $_.Exception.Message; folder = $taskFolder} | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $taskFolder 'runner-failure.json') -Encoding utf8
    throw
} finally {
    if ($null -ne $taskProcess) {
        if (-not $taskProcess.HasExited) {
            Stop-Process -InputObject $taskProcess -Force -ErrorAction SilentlyContinue
            [void]$taskProcess.WaitForExit(5000)
            $taskForced = $true
        }
        [pscustomobject]@{pid = $taskProcess.Id; exited = $taskProcess.HasExited; forced_cleanup = $taskForced; path = $taskExe} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskFolder 'worker-cleanup.json') -Encoding utf8
        $taskProcess.Dispose()
    }
    ConvertTo-Json -InputObject @($taskSamples.ToArray()) -Depth 4 | Set-Content -LiteralPath (Join-Path $taskFolder 'process-memory-samples.json') -Encoding utf8
    ConvertTo-Json -InputObject @($taskSamplingErrors.ToArray()) | Set-Content -LiteralPath (Join-Path $taskFolder 'sampling-errors.json') -Encoding utf8
    ConvertTo-Json -InputObject @($taskTerminalSampleRaces.ToArray()) -Depth 4 | Set-Content -LiteralPath (Join-Path $taskFolder 'terminal-exit-sampling-races.json') -Encoding utf8
    foreach ($taskName in $taskVariables) { [Environment]::SetEnvironmentVariable($taskName, $taskOld[$taskName], 'Process') }
}
