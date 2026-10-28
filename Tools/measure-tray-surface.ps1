[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateRange(1, 2147483647)][int]$ExpectedResidentPid,
    [Parameter(Mandatory = $true)][string]$Worker,
    [Parameter(Mandatory = $true)][string]$Log,
    [ValidateSet(1, 8)][int]$Burst = 8
)

$ErrorActionPreference = 'Stop'
if (-not [IO.Path]::IsPathFullyQualified($Worker) -or -not [IO.Path]::IsPathFullyQualified($Log)) {
    throw 'Worker 和 Log 必须是绝对路径。'
}
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
$taskLog = [IO.Path]::GetFullPath($Log)

# 不读取屏幕像素；只检测原生窗口、进程身份和用户输入时间。
if (-not ('RustShotTraySurfaceNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class RustShotTraySurfaceNative {
    public sealed class Standby {
        public uint Pid;public IntPtr Control,Surface;public long Token,SurfaceToken,ParentToken;
        public long Transport;
    }
    public sealed class SurfaceStamp {
        public IntPtr Window;public long Token,RequestQpc,ReadyQpc;
    }
    [StructLayout(LayoutKind.Sequential)] struct Rect {public int Left,Top,Right,Bottom;}
    [StructLayout(LayoutKind.Sequential)] struct LastInput {public uint Size,Tick;}
    [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)] struct ProcessEntry {
        public uint Size,Usage,Pid;public UIntPtr Heap;public uint Module,Threads,Parent;
        public int Priority;public uint Flags;[MarshalAs(UnmanagedType.ByValTStr,SizeConst=260)]public string Name;
    }
    delegate bool EnumWindowProc(IntPtr window,IntPtr data);
    [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern IntPtr FindWindow(string name,string title);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindowProc callback,IntPtr data);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern IntPtr GetProp(IntPtr h,string name);
    [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr h,StringBuilder text,int count);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h,out Rect rect);
    [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr h,StringBuilder text,int count);
    [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
    [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll",SetLastError=true)] static extern bool GetLastInputInfo(ref LastInput info);
    [DllImport("user32.dll",SetLastError=true)] static extern bool PostMessage(IntPtr h,uint message,IntPtr wp,IntPtr lp);
    [DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern IntPtr OpenMutex(uint access,bool inherit,string name);
    [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
    [DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool QueryFullProcessImageName(IntPtr h,uint flags,StringBuilder name,ref uint size);
    [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr CreateToolhelp32Snapshot(uint flags,uint pid);
    [DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool Process32First(IntPtr h,ref ProcessEntry entry);
    [DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)] static extern bool Process32Next(IntPtr h,ref ProcessEntry entry);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode)] static extern IntPtr OpenFileMapping(uint access,bool inherit,string name);
    [DllImport("kernel32.dll")] static extern IntPtr MapViewOfFile(IntPtr mapping,uint access,uint high,uint low,UIntPtr bytes);
    [DllImport("kernel32.dll")] static extern bool UnmapViewOfFile(IntPtr view);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode)] static extern IntPtr OpenEvent(uint access,bool inherit,string name);
    static bool OwnedClass(IntPtr h,uint pid,string expected) {
        uint owner;GetWindowThreadProcessId(h,out owner);
        var text=new StringBuilder(160);
        return owner==pid&&GetClassName(h,text,text.Capacity)>0&&text.ToString()==expected;
    }
    static bool IdleChannel(Standby standby,IntPtr resident,uint residentPid) {
        // 与生产 IPC v3/80-byte 合同一致；只读映射和事件句柄，不发布请求或消费事件。
        string prefix=String.Format("Local\\RustShot.Capture.IPC.v3.{0}.{1}.{2:x16}.{3:x16}.",
            residentPid,standby.Pid,(ulong)standby.ParentToken,(ulong)standby.Token);
        IntPtr mapping=OpenFileMapping(4,false,prefix+"metadata"),view=IntPtr.Zero;
        IntPtr request=IntPtr.Zero,foreground=IntPtr.Zero;
        if(mapping==IntPtr.Zero)return false;
        try {
            view=MapViewOfFile(mapping,4,0,0,new UIntPtr(80));if(view==IntPtr.Zero)return false;
            request=OpenEvent(0x00100000,false,prefix+"request");
            foreground=OpenEvent(0x00100000,false,prefix+"foreground-complete");
            if(request==IntPtr.Zero||foreground==IntPtr.Zero||Marshal.ReadInt32(view,56)!=2)return false;
            System.Threading.Thread.MemoryBarrier();
            bool matches=Marshal.ReadInt32(view,0)==0x52534950&&Marshal.ReadInt32(view,4)==3&&
                Marshal.ReadInt32(view,8)==80&&(uint)Marshal.ReadInt32(view,12)==residentPid&&
                (uint)Marshal.ReadInt32(view,16)==standby.Pid&&Marshal.ReadInt32(view,20)==0&&
                Marshal.ReadInt64(view,24)==standby.ParentToken&&Marshal.ReadInt64(view,32)==standby.Token&&
                Marshal.ReadInt64(view,40)==resident.ToInt64()&&Marshal.ReadInt64(view,48)==standby.Control.ToInt64()&&
                Marshal.ReadInt32(view,60)==0&&Marshal.ReadInt32(view,64)==0&&
                Marshal.ReadInt32(view,68)==0&&Marshal.ReadInt64(view,72)==0;
            System.Threading.Thread.MemoryBarrier();
            return matches&&Marshal.ReadInt32(view,56)==2;
        } finally {
            if(foreground!=IntPtr.Zero)CloseHandle(foreground);if(request!=IntPtr.Zero)CloseHandle(request);
            if(view!=IntPtr.Zero)UnmapViewOfFile(view);CloseHandle(mapping);
        }
    }
    public static Standby IdleStandby(uint pid,IntPtr resident,uint residentPid) {
        if(IntPtr.Size!=8||Resident(residentPid)!=resident||GetProp(resident,"RustShot.Resident.Closing")!=IntPtr.Zero)return null;
        long parentToken=GetProp(resident,"RustShot.Resident.Token").ToInt64();if(parentToken<=0)return null;
        Standby found=null;int count=0;
        EnumWindows((h,p)=>{
            if(!OwnedClass(h,pid,"RustShot.Capture.Standby"))return true;
            count++;
            var item=new Standby{Pid=pid,Control=h,ParentToken=parentToken,
                Token=GetProp(h,"RustShot.Standby.Token").ToInt64(),
                Transport=GetProp(h,"RustShot.Standby.Transport").ToInt64(),
                Surface=GetProp(h,"RustShot.Standby.PrewarmedSurface"),
                SurfaceToken=GetProp(h,"RustShot.Standby.PrewarmedToken").ToInt64()};
            if(GetProp(h,"RustShot.Standby.Owner").ToInt64()!=residentPid||
                GetProp(h,"RustShot.Standby.OwnerWindow")!=resident||
                GetProp(h,"RustShot.Standby.ParentToken").ToInt64()!=parentToken||item.Token<=0||
                GetProp(h,"RustShot.Standby.TransportVersion").ToInt64()!=3||
                GetProp(h,"RustShot.Standby.MetadataBytes").ToInt64()!=80||
                GetProp(h,"RustShot.Standby.Ready")==IntPtr.Zero)return true;
            if(item.Transport==1) {if(!IdleChannel(item,resident,residentPid))return true;}
            else if(item.Transport!=2)return true;
            if(item.Surface!=IntPtr.Zero) {
                if(item.SurfaceToken<=0||!OwnedClass(item.Surface,pid,"RustShot.StartupSurface")||
                    GetProp(item.Surface,"RustShot.StartupSurface.Token").ToInt64()!=item.SurfaceToken||
                    GetProp(item.Surface,"RustShot.StartupSurface.Prewarmed")==IntPtr.Zero||
                    GetProp(item.Surface,"RustShot.StartupSurface.Ready")!=IntPtr.Zero)return true;
            } else if(item.SurfaceToken!=0||GetProp(h,"RustShot.Standby.PrewarmFailed")==IntPtr.Zero)return true;
            found=item;return true;
        },IntPtr.Zero);
        return count==1?found:null;
    }
    public static SurfaceStamp ReadySurface(uint pid,long postQpc) {
        SurfaceStamp found=null;int count=0;
        EnumWindows((h,p)=>{
            if(!OwnedClass(h,pid,"RustShot.StartupSurface")||GetProp(h,"RustShot.StartupSurface.Ready")==IntPtr.Zero)return true;
            long token=GetProp(h,"RustShot.StartupSurface.Token").ToInt64();
            long request=GetProp(h,"RustShot.StartupSurface.RequestQpc").ToInt64();
            long ready=GetProp(h,"RustShot.StartupSurface.ReadyQpc").ToInt64();
            if(token<=0||request<postQpc||ready<request||ready>System.Diagnostics.Stopwatch.GetTimestamp())return true;
            count++;found=new SurfaceStamp{Window=h,Token=token,RequestQpc=request,ReadyQpc=ready};return true;
        },IntPtr.Zero);
        return count==1?found:null;
    }
    public static IntPtr Resident(uint pid) {
        IntPtr h=FindWindow("RustShot.MVP.Window",null);uint owner;
        if(h==IntPtr.Zero)return IntPtr.Zero;GetWindowThreadProcessId(h,out owner);return owner==pid?h:IntPtr.Zero;
    }
    public static bool CaptureBusy() {
        IntPtr h=OpenMutex(0x00100000,false,"Local\\RustShot.MVP.Capture");
        if(h!=IntPtr.Zero){CloseHandle(h);return true;}
        int error=Marshal.GetLastWin32Error();if(error==2)return false;
        throw new InvalidOperationException("无法确定 CaptureMutex 状态，Win32="+error);
    }
    public static uint InputTick() {
        var info=new LastInput{Size=(uint)Marshal.SizeOf(typeof(LastInput))};
        if(!GetLastInputInfo(ref info))throw new InvalidOperationException("无法读取用户输入时间");return info.Tick;
    }
    public static bool PostTray(IntPtr h,uint pid) {
        uint owner;GetWindowThreadProcessId(h,out owner);
        // Shell v4：低位 NIN_SELECT=0x400，高位托盘 ID=1；窗口回调会继续转发到状态循环。
        return owner==pid&&PostMessage(h,0x8001,IntPtr.Zero,new IntPtr(0x10400));
    }
    public static string ImagePath(uint pid) {
        IntPtr h=OpenProcess(0x1000,false,pid);if(h==IntPtr.Zero)return null;
        try{uint size=32768;var name=new StringBuilder((int)size);return QueryFullProcessImageName(h,0,name,ref size)?name.ToString():null;}
        finally{CloseHandle(h);}
    }
    public static uint ParentPid(uint pid) {
        IntPtr h=CreateToolhelp32Snapshot(2,0);if(h==new IntPtr(-1))return 0;
        try{var e=new ProcessEntry{Size=(uint)Marshal.SizeOf(typeof(ProcessEntry))};
            if(!Process32First(h,ref e))return 0;do{if(e.Pid==pid)return e.Parent;}while(Process32Next(h,ref e));return 0;
        }finally{CloseHandle(h);}
    }
    public static IntPtr CaptureWindow(uint pid) {
        IntPtr previous=SetThreadDpiAwarenessContext(new IntPtr(-4));
        try {
            int left=GetSystemMetrics(76),top=GetSystemMetrics(77),width=GetSystemMetrics(78),height=GetSystemMetrics(79);
            IntPtr found=IntPtr.Zero;EnumWindows((h,p)=>{
                uint owner;GetWindowThreadProcessId(h,out owner);if(owner!=pid||!IsWindowVisible(h))return true;
                var text=new StringBuilder(160);GetWindowText(h,text,text.Capacity);if(text.ToString()!="RaiShot · GPUI")return true;
                Rect r;if(!GetWindowRect(h,out r)||Math.Abs((r.Right-r.Left)-width)>2||Math.Abs((r.Bottom-r.Top)-height)>2||Math.Abs(r.Left-left)>2||Math.Abs(r.Top-top)>2)return true;
                found=h;return false;
            },IntPtr.Zero);return found;
        }finally{if(previous!=IntPtr.Zero)SetThreadDpiAwarenessContext(previous);}
    }
    public static IntPtr SurfaceWindow(uint pid) {
        IntPtr previous=SetThreadDpiAwarenessContext(new IntPtr(-4));
        try {
            int left=GetSystemMetrics(76),top=GetSystemMetrics(77),width=GetSystemMetrics(78),height=GetSystemMetrics(79);
            IntPtr found=IntPtr.Zero;EnumWindows((h,p)=>{
                uint owner;GetWindowThreadProcessId(h,out owner);if(owner!=pid||!IsWindowVisible(h))return true;
                var text=new StringBuilder(160);GetWindowText(h,text,text.Capacity);if(text.ToString()!="RaiShot · 快速框选")return true;
                if(GetProp(h,"RustShot.StartupSurface.Ready")==IntPtr.Zero)return true;
                Rect r;if(!GetWindowRect(h,out r)||r.Right-r.Left!=width||r.Bottom-r.Top!=height||r.Left!=left||r.Top!=top)return true;
                found=h;return false;
            },IntPtr.Zero);return found;
        }finally{if(previous!=IntPtr.Zero)SetThreadDpiAwarenessContext(previous);}
    }
    public static IntPtr ReadyCaptureWindow(uint pid) {
        IntPtr h=CaptureWindow(pid);
        if(h==IntPtr.Zero||GetForegroundWindow()!=h||SurfaceWindow(pid)!=IntPtr.Zero||GetProp(h,"RustShot.GpuiPresentSerial")==IntPtr.Zero)return IntPtr.Zero;
        var size=Size(h);
        long packed=GetProp(h,"RustShot.GpuiPresentSize").ToInt64();
        return ((packed>>16)&0xffff)==size[0]&&(packed&0xffff)==size[1]?h:IntPtr.Zero;
    }
    public static int[] Size(IntPtr h) {
        IntPtr previous=SetThreadDpiAwarenessContext(new IntPtr(-4));
        try{Rect r;return GetWindowRect(h,out r)?new[]{r.Right-r.Left,r.Bottom-r.Top}:new[]{0,0};}
        finally{if(previous!=IntPtr.Zero)SetThreadDpiAwarenessContext(previous);}
    }
    public static bool CloseOwned(IntPtr h,uint pid) {
        uint owner;GetWindowThreadProcessId(h,out owner);
        return owner==pid&&(h==CaptureWindow(pid)||h==SurfaceWindow(pid))&&PostMessage(h,0x0010,IntPtr.Zero,IntPtr.Zero);
    }
}
'@
}

function Get-ResidentCaptureWorkers {
    # 独立设置/帮助等会话不属于这次托盘请求；仅跟踪指定实例直接启动的同镜像进程。
    foreach ($taskCandidate in @(Get-Process -Name 'raishot-gpui' -ErrorAction SilentlyContinue)) {
        if ([RustShotTraySurfaceNative]::ParentPid([uint32]$taskCandidate.Id) -ne $ExpectedResidentPid) { continue }
        if (-not [string]::Equals([RustShotTraySurfaceNative]::ImagePath([uint32]$taskCandidate.Id), $taskWorker, [StringComparison]::OrdinalIgnoreCase)) {
            throw '指定托盘的 GPUI 子进程镜像不符；保留会话，不发送或取消截图。'
        }
        $taskCandidate
    }
}

$taskResident = Get-Process -Id $ExpectedResidentPid
$taskResidentCreated = $taskResident.StartTime
$taskResidentWindow = [RustShotTraySurfaceNative]::Resident([uint32]$ExpectedResidentPid)
if ($taskResidentWindow -eq [IntPtr]::Zero) { throw '常驻窗口的 PID 与 ExpectedResidentPid 不匹配；没有发送通知。' }
if ([RustShotTraySurfaceNative]::CaptureBusy()) { throw '已有 CaptureMutex；没有发送通知。' }
$taskInitialWorkers = @(Get-ResidentCaptureWorkers)
$taskStandby = $null
if ($taskInitialWorkers.Count -gt 1) { throw '已有多个 GPUI 工作进程；没有发送通知。' }
if ($taskInitialWorkers.Count -eq 1) {
    $taskInitialWorker = $taskInitialWorkers[0]
    if (-not [string]::Equals([RustShotTraySurfaceNative]::ImagePath([uint32]$taskInitialWorker.Id), $taskWorker, [StringComparison]::OrdinalIgnoreCase) -or
        [RustShotTraySurfaceNative]::ParentPid([uint32]$taskInitialWorker.Id) -ne $ExpectedResidentPid) {
        throw '已有 GPUI 工作进程的文件或父进程不符；没有发送通知。'
    }
    $taskStandby = [RustShotTraySurfaceNative]::IdleStandby([uint32]$taskInitialWorker.Id, $taskResidentWindow, [uint32]$ExpectedResidentPid)
    if ($null -eq $taskStandby) { throw '已有 GPUI 工作进程不是归属及传输状态均已核验的空闲 standby；没有发送通知。' }
}

$taskSeen = [Collections.Generic.HashSet[int]]::new()
$taskWorkerProcess = $null
if ($null -ne $taskStandby) {
    $taskWorkerProcess = $taskInitialWorker
    $taskWorkerProcess.Handle | Out-Null
    $taskSeen.Add($taskWorkerProcess.Id) | Out-Null
}
$taskWindow = [IntPtr]::Zero
$taskInputTick = [RustShotTraySurfaceNative]::InputTick()
$taskProcessMs = $null
$taskVisibleMs = $null
$taskSurfaceMs = $null
$taskSurfaceObservedMs = $null
$taskSurfaceStamp = $null
$taskPostQpc = 0L
$taskSurfaceTiming = 'external_poll'
$taskGpuiMs = $null
$taskGpuiReadyMs = $null
$taskVisibleAt = $null
$taskExitCode = $null
$taskCancelled = $false
$taskAbort = $null
$taskSize = @(0, 0)
$taskSent = 0
$taskWatch = [Diagnostics.Stopwatch]::StartNew()

try {
    # 紧挨通知发送再次检查，避免初始验证之后新出现的用户会话被测量接管。
    $taskBeforePost = @(Get-ResidentCaptureWorkers)
    $taskExpectedCount = $(if ($null -eq $taskStandby) { 0 } else { 1 })
    if ($taskBeforePost.Count -ne $taskExpectedCount -or [RustShotTraySurfaceNative]::CaptureBusy()) {
        throw '发送前出现其他会话；没有发送通知。'
    }
    if ($null -ne $taskStandby) {
        $taskRechecked = [RustShotTraySurfaceNative]::IdleStandby([uint32]$taskWorkerProcess.Id, $taskResidentWindow, [uint32]$ExpectedResidentPid)
        if ($taskBeforePost[0].Id -ne $taskWorkerProcess.Id -or $null -eq $taskRechecked -or
            $taskRechecked.Control -ne $taskStandby.Control -or $taskRechecked.Token -ne $taskStandby.Token -or
            $taskRechecked.ParentToken -ne $taskStandby.ParentToken -or $taskWorkerProcess.HasExited) {
            throw '发送前 standby 的身份或空闲状态变化；没有发送通知。'
        }
    }
    if ([RustShotTraySurfaceNative]::InputTick() -ne $taskInputTick) { throw '发送前出现用户输入；没有发送通知。' }
    $taskWatch.Restart()
    $taskPostQpc = [Diagnostics.Stopwatch]::GetTimestamp()
    for ($taskIndex = 0; $taskIndex -lt $Burst; $taskIndex++) {
        if (-not [RustShotTraySurfaceNative]::PostTray($taskResidentWindow, [uint32]$ExpectedResidentPid)) {
            throw '常驻窗口身份变化或托盘通知投递失败。'
        }
        $taskSent++
    }
    while ($taskWatch.ElapsedMilliseconds -lt 5000) {
        if ([RustShotTraySurfaceNative]::InputTick() -ne $taskInputTick) {
            $taskAbort = 'user_input_during_measurement'; break
        }
        if ([RustShotTraySurfaceNative]::Resident([uint32]$ExpectedResidentPid) -ne $taskResidentWindow) {
            $taskAbort = 'resident_window_changed'; break
        }
        $taskProcesses = @(Get-ResidentCaptureWorkers)
        $taskObservedMs = $taskWatch.Elapsed.TotalMilliseconds
        foreach ($taskProcess in $taskProcesses) {
            if ($taskSeen.Add($taskProcess.Id)) {
                if ($null -eq $taskProcessMs) { $taskProcessMs = $taskObservedMs }
                $taskPath = [RustShotTraySurfaceNative]::ImagePath([uint32]$taskProcess.Id)
                $taskParent = [RustShotTraySurfaceNative]::ParentPid([uint32]$taskProcess.Id)
                if (-not [string]::Equals($taskPath, $taskWorker, [StringComparison]::OrdinalIgnoreCase) -or $taskParent -ne $ExpectedResidentPid) {
                    $taskAbort = 'foreign_or_unverified_new_worker'; break
                }
                # Get-Process 不是 Start-Process 返回的 Child，提前持有自有进程句柄以便退出后读取 ExitCode。
                $taskProcess.Handle | Out-Null
                $taskWorkerProcess = $taskProcess
            }
        }
        if ($null -ne $taskAbort) { break }
        if ($taskSeen.Count -gt 1) { $taskAbort = 'multiple_new_workers_preserved'; break }
        if ($null -ne $taskWorkerProcess) {
            if ($taskWorkerProcess.HasExited) { $taskAbort = 'worker_exited_before_cancel'; break }
            $taskFastWindow = [RustShotTraySurfaceNative]::SurfaceWindow([uint32]$taskWorkerProcess.Id)
            if ($null -eq $taskSurfaceObservedMs -and $taskFastWindow -ne [IntPtr]::Zero) { $taskSurfaceObservedMs = $taskWatch.Elapsed.TotalMilliseconds }
            if ($null -eq $taskStandby) {
                if ($null -eq $taskSurfaceMs) { $taskSurfaceMs = $taskSurfaceObservedMs }
            } elseif ($null -eq $taskSurfaceStamp) {
                $taskStamp = [RustShotTraySurfaceNative]::ReadySurface([uint32]$taskWorkerProcess.Id, $taskPostQpc)
                if ($null -ne $taskStamp) {
                    if ($taskStamp.Window -eq $taskStandby.Surface -and $taskStamp.Token -ne $taskStandby.SurfaceToken) {
                        $taskAbort = 'prewarmed_surface_identity_changed'; break
                    }
                    $taskSurfaceStamp = $taskStamp
                    $taskSurfaceMs = ($taskStamp.ReadyQpc - $taskPostQpc) * 1000.0 / [Diagnostics.Stopwatch]::Frequency
                    $taskSurfaceTiming = 'published_ready_qpc'
                }
            }
            $taskGpuWindow = [RustShotTraySurfaceNative]::CaptureWindow([uint32]$taskWorkerProcess.Id)
            if ($null -eq $taskGpuiMs -and $taskGpuWindow -ne [IntPtr]::Zero) { $taskGpuiMs = $taskWatch.Elapsed.TotalMilliseconds }
            if ($null -eq $taskGpuiReadyMs -and [RustShotTraySurfaceNative]::ReadyCaptureWindow([uint32]$taskWorkerProcess.Id) -ne [IntPtr]::Zero) { $taskGpuiReadyMs = $taskWatch.Elapsed.TotalMilliseconds }
            if ($taskWindow -eq [IntPtr]::Zero) {
                $taskWindow = if ($taskFastWindow -ne [IntPtr]::Zero) { $taskFastWindow } else { $taskGpuWindow }
                if ($taskWindow -ne [IntPtr]::Zero) {
                    $taskVisibleMs = $taskWatch.Elapsed.TotalMilliseconds
                    $taskVisibleAt = $taskWatch.ElapsedMilliseconds
                    $taskSize = [RustShotTraySurfaceNative]::Size($taskWindow)
                }
            }
            # 保留短观察期确认 8 次通知没有产生第二个 worker；不把短暂首窗当作结论。
            if ($null -ne $taskVisibleAt -and $taskWatch.ElapsedMilliseconds - $taskVisibleAt -ge 250 -and
                ($null -ne $taskGpuiReadyMs -or $taskWatch.ElapsedMilliseconds - $taskVisibleAt -ge 2000)) { break }
        }
        Start-Sleep -Milliseconds 10
    }
    if ($null -eq $taskAbort -and ($taskWindow -eq [IntPtr]::Zero -or $taskSeen.Count -ne 1)) {
        $taskAbort = 'capture_window_not_observed'
    }
    if ($null -eq $taskAbort) {
        # 首屏可能已完成交接；取消当前仍属于该子进程的界面，绝不对旧 HWND 操作。
        $taskNowSurface = [RustShotTraySurfaceNative]::SurfaceWindow([uint32]$taskWorkerProcess.Id)
        $taskWindow = if ($taskNowSurface -ne [IntPtr]::Zero) { $taskNowSurface } else { [RustShotTraySurfaceNative]::CaptureWindow([uint32]$taskWorkerProcess.Id) }
        $taskNow = @(Get-ResidentCaptureWorkers)
        if ([RustShotTraySurfaceNative]::InputTick() -ne $taskInputTick -or $taskNow.Count -ne 1 -or $taskNow[0].Id -ne $taskWorkerProcess.Id) {
            $taskAbort = 'concurrent_input_or_worker_before_cancel'
        } elseif ([RustShotTraySurfaceNative]::Resident([uint32]$ExpectedResidentPid) -ne $taskResidentWindow -or $taskResident.StartTime -ne $taskResidentCreated) {
            $taskAbort = 'resident_changed_before_cancel'
        } elseif (-not [string]::Equals([RustShotTraySurfaceNative]::ImagePath([uint32]$taskWorkerProcess.Id), $taskWorker, [StringComparison]::OrdinalIgnoreCase) -or [RustShotTraySurfaceNative]::ParentPid([uint32]$taskWorkerProcess.Id) -ne $ExpectedResidentPid) {
            $taskAbort = 'worker_identity_changed_before_cancel'
        } else {
            for ($taskCloseAttempt = 0; $taskCloseAttempt -lt 3; $taskCloseAttempt++) {
                if ([RustShotTraySurfaceNative]::InputTick() -ne $taskInputTick -or
                    [RustShotTraySurfaceNative]::Resident([uint32]$ExpectedResidentPid) -ne $taskResidentWindow -or
                    -not [string]::Equals([RustShotTraySurfaceNative]::ImagePath([uint32]$taskWorkerProcess.Id), $taskWorker, [StringComparison]::OrdinalIgnoreCase) -or
                    [RustShotTraySurfaceNative]::ParentPid([uint32]$taskWorkerProcess.Id) -ne $ExpectedResidentPid) { break }
                $taskNowSurface = [RustShotTraySurfaceNative]::SurfaceWindow([uint32]$taskWorkerProcess.Id)
                $taskWindow = if ($taskNowSurface -ne [IntPtr]::Zero) { $taskNowSurface } else { [RustShotTraySurfaceNative]::CaptureWindow([uint32]$taskWorkerProcess.Id) }
                if ([RustShotTraySurfaceNative]::CloseOwned($taskWindow, [uint32]$taskWorkerProcess.Id)) { $taskCancelled = $true; break }
                Start-Sleep -Milliseconds 10
            }
            if (-not $taskCancelled) { $taskAbort = 'owned_wm_close_not_posted' }
            elseif ($taskWorkerProcess.WaitForExit(5000)) { $taskExitCode = $taskWorkerProcess.ExitCode }
            else { $taskAbort = 'owned_worker_did_not_exit_no_force' }
        }
    }
} catch {
    $taskAbort = $_.Exception.Message
}
# 异常/并发/多 worker 时不进行 finally 清窗，避免误关用户会话。
$taskWatch.Stop()
$taskMutexReleased = -not [RustShotTraySurfaceNative]::CaptureBusy()
$taskRemaining = @(Get-ResidentCaptureWorkers)
$taskReplacementStandby = $null
$taskReplacementReadyWaitMs = $null
$taskRemainingAcceptable = $taskRemaining.Count -eq 0
if ($null -eq $taskAbort -and $null -ne $taskStandby -and $taskRemaining.Count -eq 1) {
    $taskReplacement = $taskRemaining[0]
    if ($taskReplacement.Id -ne $taskWorkerProcess.Id -and
        [string]::Equals([RustShotTraySurfaceNative]::ImagePath([uint32]$taskReplacement.Id), $taskWorker, [StringComparison]::OrdinalIgnoreCase) -and
        [RustShotTraySurfaceNative]::ParentPid([uint32]$taskReplacement.Id) -eq $ExpectedResidentPid) {
        # 后继可能刚创建、尚在 PREPARING；只读等它发布完整 Ready，不操作其窗口或事件。
        $taskReplacement.Handle | Out-Null
        $taskReplacementWait = [Diagnostics.Stopwatch]::StartNew()
        do {
            $taskCurrentWorkers = @(Get-ResidentCaptureWorkers)
            if ($taskReplacement.HasExited -or $taskCurrentWorkers.Count -ne 1 -or
                $taskCurrentWorkers[0].Id -ne $taskReplacement.Id -or
                [RustShotTraySurfaceNative]::Resident([uint32]$ExpectedResidentPid) -ne $taskResidentWindow -or
                [RustShotTraySurfaceNative]::InputTick() -ne $taskInputTick -or
                [RustShotTraySurfaceNative]::CaptureBusy()) { break }
            $taskReplacementStandby = [RustShotTraySurfaceNative]::IdleStandby([uint32]$taskReplacement.Id, $taskResidentWindow, [uint32]$ExpectedResidentPid)
            if ($null -ne $taskReplacementStandby) { break }
            Start-Sleep -Milliseconds 20
        } while ($taskReplacementWait.ElapsedMilliseconds -lt 2000)
        $taskReplacementReadyWaitMs = $taskReplacementWait.Elapsed.TotalMilliseconds
        $taskRemainingAcceptable = $null -ne $taskReplacementStandby
    }
}
$taskRemaining = @(Get-ResidentCaptureWorkers)
if ($null -ne $taskReplacementStandby) {
    $taskRemainingAcceptable = $taskRemaining.Count -eq 1 -and $taskRemaining[0].Id -eq $taskReplacementStandby.Pid
} elseif ($taskRemaining.Count -ne 0) {
    $taskRemainingAcceptable = $false
}
$taskMutexReleased = -not [RustShotTraySurfaceNative]::CaptureBusy()
if ($null -eq $taskAbort -and -not $taskRemainingAcceptable) { $taskAbort = 'remaining_worker_is_not_a_verified_replacement_standby' }
if ($null -eq $taskAbort -and ($null -eq $taskSurfaceMs -or $null -eq $taskGpuiReadyMs)) {
    $taskAbort = 'missing_surface_or_foreground_gpui_readiness'
}
$taskResident.Refresh()
$taskResidentAlive = -not $taskResident.HasExited
$taskOk = $null -eq $taskAbort -and $taskSent -eq $Burst -and $taskSeen.Count -eq 1 -and $taskCancelled -and $taskExitCode -eq 0 -and $taskMutexReleased -and $taskRemainingAcceptable -and $taskResidentAlive
$taskNewWorkerPids = @($taskSeen | Where-Object { $null -eq $taskStandby -or $_ -ne $taskStandby.Pid } | Sort-Object)
$taskRow = [ordered]@{
    phase = 'tray_surface_external'; measured_utc = [DateTime]::UtcNow.ToString('o')
    expected_resident_pid = $ExpectedResidentPid; worker = $taskWorker
    worker_process_scope = 'expected_resident_direct_children_with_verified_image'
    burst = $Burst; notifications_posted = $taskSent; new_worker_count = $taskNewWorkerPids.Count
    new_worker_pids = $taskNewWorkerPids; capture_worker_count = $taskSeen.Count
    consumed_standby_pid = $(if ($null -ne $taskStandby) { $taskStandby.Pid } else { $null })
    standby_transport = $(if ($null -ne $taskStandby) { $taskStandby.Transport } else { $null })
    post_to_new_process_ms = $(if ($null -eq $taskProcessMs) { $null } else { [math]::Round($taskProcessMs, 3) })
    post_to_window_visible_ms = $(if ($null -eq $taskVisibleMs) { $null } else { [math]::Round($taskVisibleMs, 3) })
    post_to_surface_ready_ms = $(if ($null -eq $taskSurfaceMs) { $null } else { [math]::Round($taskSurfaceMs, 3) })
    surface_ready_timing = $taskSurfaceTiming
    post_to_surface_observed_ms = $(if ($null -eq $taskSurfaceObservedMs) { $null } else { [math]::Round($taskSurfaceObservedMs, 3) })
    request_to_surface_ready_ms = $(if ($null -ne $taskSurfaceStamp) { [math]::Round(($taskSurfaceStamp.ReadyQpc - $taskSurfaceStamp.RequestQpc) * 1000.0 / [Diagnostics.Stopwatch]::Frequency, 3) } else { $null })
    post_qpc = $taskPostQpc
    surface_request_qpc = $(if ($null -ne $taskSurfaceStamp) { $taskSurfaceStamp.RequestQpc } else { $null })
    surface_ready_qpc = $(if ($null -ne $taskSurfaceStamp) { $taskSurfaceStamp.ReadyQpc } else { $null })
    post_to_gpui_visible_ms = $(if ($null -eq $taskGpuiMs) { $null } else { [math]::Round($taskGpuiMs, 3) })
    post_to_gpui_ready_ms = $(if ($null -eq $taskGpuiReadyMs) { $null } else { [math]::Round($taskGpuiReadyMs, 3) })
    window_width = $taskSize[0]; window_height = $taskSize[1]
    cancel = $(if ($taskCancelled) { 'owned_wm_close' } else { 'none_preserved' })
    exit_code = $taskExitCode; capture_mutex_released = $taskMutexReleased
    remaining_worker_count = $taskRemaining.Count; resident_alive = $taskResidentAlive
    replacement_standby_pid = $(if ($null -ne $taskReplacementStandby) { $taskReplacementStandby.Pid } else { $null })
    replacement_ready_wait_ms = $(if ($null -ne $taskReplacementReadyWaitMs) { [math]::Round($taskReplacementReadyWaitMs, 3) } else { $null })
    resident_working_set_mib = $(if ($taskResidentAlive) { [math]::Round($taskResident.WorkingSet64 / 1MB, 3) } else { $null })
    resident_private_mib = $(if ($taskResidentAlive) { [math]::Round($taskResident.PrivateMemorySize64 / 1MB, 3) } else { $null })
    total_ms = [math]::Round($taskWatch.Elapsed.TotalMilliseconds, 3)
    aborted_reason = $taskAbort; ok = $taskOk
}
[IO.Directory]::CreateDirectory((Split-Path -Parent $taskLog)) | Out-Null
$taskJson = $taskRow | ConvertTo-Json -Compress
[IO.File]::AppendAllText($taskLog, $taskJson + [Environment]::NewLine)
Write-Output $taskJson
if (-not $taskOk) { throw '托盘启动测量未完成或检测到并发；已保留未确认的会话，没有强制终止。' }
