[CmdletBinding()]
param(
 [string]$TargetDirectory='target/next',
 [switch]$PreflightOnly,
 [string]$BuildVerification='target/qa/long-browser-stress-build-verification.json',
 [string]$ChromePath=$env:RUSTSHOT_BROWSER_QA_CHROME,
 [string]$NodePath=$env:RUSTSHOT_BROWSER_QA_NODE,
 [string]$ModuleDirectory=$env:RUSTSHOT_BROWSER_QA_MODULES
)
$ErrorActionPreference='Stop'
$taskRoot=Split-Path -Parent $PSScriptRoot
if(-not $ChromePath){
 $taskChromeCommand=Get-Command chrome.exe -ErrorAction SilentlyContinue
 if($taskChromeCommand){$ChromePath=$taskChromeCommand.Source}
 else{
  $ChromePath=@(foreach($taskBase in @($env:ProgramFiles,${env:ProgramFiles(x86)},$env:LOCALAPPDATA)){
   if($taskBase){$taskCandidate=Join-Path $taskBase 'Google/Chrome/Application/chrome.exe';if(Test-Path -LiteralPath $taskCandidate){$taskCandidate}}
  })|Select-Object -First 1
 }
}
if(-not $ChromePath -or -not (Test-Path -LiteralPath $ChromePath -PathType Leaf)){throw '未找到 Chrome；请用 -ChromePath 或 RUSTSHOT_BROWSER_QA_CHROME 指定。'}
$taskChrome=(Get-Item -LiteralPath $ChromePath).FullName
if(-not $NodePath){$NodePath='node'}
$taskNode=(Get-Command $NodePath -ErrorAction Stop).Source
if(-not $ModuleDirectory){$ModuleDirectory=Join-Path $PSScriptRoot 'node_modules'}
$taskModules=if([IO.Path]::IsPathRooted($ModuleDirectory)){[IO.Path]::GetFullPath($ModuleDirectory)}else{[IO.Path]::GetFullPath((Join-Path $taskRoot $ModuleDirectory))}
foreach($taskModule in @('playwright','pngjs')){
 if(-not (Test-Path -LiteralPath (Join-Path $taskModules $taskModule) -PathType Container)){
  throw '缺少浏览器 QA 依赖；运行 npm install --prefix Tools --no-save playwright pngjs，或用 -ModuleDirectory / RUSTSHOT_BROWSER_QA_MODULES 指定 node_modules。'
 }
}
if(-not ('RustShotBrowserStressSupervisor' -as [type])) {
Add-Type @'
using System;using System.Text;using System.Runtime.InteropServices;using System.ComponentModel;using System.Collections.Generic;
public sealed class RustShotBrowserStressSupervisor:IDisposable {
 [StructLayout(LayoutKind.Sequential)] struct MEMORY {public uint cb,faults;public UIntPtr peakWS,workingSet,peakPaged,paged,peakNonPaged,nonPaged,pagefile,peakPagefile,privateUsage;}
 [DllImport("psapi.dll",SetLastError=true)] static extern bool GetProcessMemoryInfo(IntPtr p,ref MEMORY counters,uint bytes);
 public sealed class FamilyMemory {public ulong? private_bytes;public ulong partial_private_bytes;public int members,sampled;public bool complete;}
 [StructLayout(LayoutKind.Sequential)] struct SI {public uint cb;public string reserved,desktop,title;public uint x,y,w,h,cx,cy,fill,flags;public ushort show,reserved2;public IntPtr bytes,input,output,error;}
 [StructLayout(LayoutKind.Sequential)] struct PI {public IntPtr process,thread;public uint pid,tid;}
 [StructLayout(LayoutKind.Sequential)] struct BasicLimit {public long perProcess,perJob;public uint flags;public UIntPtr minWS,maxWS;public uint active;public UIntPtr affinity;public uint priority,scheduling;}
 [StructLayout(LayoutKind.Sequential)] struct IO {public ulong readOps,writeOps,otherOps,readBytes,writeBytes,otherBytes;}
 [StructLayout(LayoutKind.Sequential)] struct ExtendedLimit {public BasicLimit basic;public IO io;public UIntPtr processMemory,jobMemory,peakProcess,peakJob;}
 [StructLayout(LayoutKind.Sequential)] public struct RECT {public int left,top,right,bottom;}
 [StructLayout(LayoutKind.Sequential)] struct POINT {public int x,y;}
 [StructLayout(LayoutKind.Sequential)] struct FT {public uint low,high;public ulong Value {get{return ((ulong)high<<32)|low;}}}
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateJobObject(IntPtr sa,string name);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job,int cls,ref ExtendedLimit limits,uint size);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job,IntPtr process);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr psa,IntPtr tsa,bool inherit,uint flags,IntPtr environment,string cwd,ref SI si,out PI pi);
 [DllImport("kernel32.dll",SetLastError=true)] static extern uint ResumeThread(IntPtr thread);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
 [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint timeout);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetProcessTimes(IntPtr process,out FT create,out FT exit,out FT kernel,out FT user);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool IsProcessInJob(IntPtr process,IntPtr job,out bool member);
 [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job,int cls,IntPtr buffer,uint length,IntPtr returned);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr process,uint code);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateJobObject(IntPtr job,uint code);
 delegate bool EnumProc(IntPtr hwnd,IntPtr lparam);
 [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc proc,IntPtr data);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd,out uint pid);
 [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
 [DllImport("user32.dll")] static extern bool IsIconic(IntPtr hwnd);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr hwnd,StringBuilder title,int length);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd,StringBuilder value,int length);
 [DllImport("user32.dll",SetLastError=true)] static extern bool GetClientRect(IntPtr hwnd,out RECT rect);
 [DllImport("user32.dll",SetLastError=true)] static extern bool ClientToScreen(IntPtr hwnd,ref POINT point);
 [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr hwnd,uint command);
 [DllImport("user32.dll",SetLastError=true)] static extern bool GetWindowRect(IntPtr hwnd,out RECT rect);
 [DllImport("user32.dll")] static extern IntPtr WindowFromPoint(POINT point);
 [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr hwnd,uint flags);
 [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr hwnd,uint attribute,out uint value,uint size);
 [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
 [DllImport("user32.dll",SetLastError=true)] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
 [DllImport("user32.dll",SetLastError=true)] static extern bool SetWindowPos(IntPtr hwnd,IntPtr after,int x,int y,int w,int h,uint flags);
 public IntPtr Job {get;private set;} public IntPtr Process {get;private set;} public uint Pid {get;private set;} public string Name {get;private set;} public string Creation {get;private set;}
 public bool Exited {get{return Process!=IntPtr.Zero&&WaitForSingleObject(Process,0)==0;}}
 public static string Quote(string value) {if(value.IndexOf('\0')>=0)throw new ArgumentException("NUL argument");var b=new StringBuilder("\"");int slashes=0;foreach(char c in value){if(c=='\\'){slashes++;continue;}if(c=='\"'){b.Append('\\',slashes*2+1).Append(c);slashes=0;continue;}b.Append('\\',slashes).Append(c);slashes=0;}b.Append('\\',slashes*2).Append('"');return b.ToString();}
 static void Check(bool valid,string op){if(!valid)throw new Win32Exception(Marshal.GetLastWin32Error(),op);}
 public RustShotBrowserStressSupervisor(string exe,string[] args,string cwd,string name) {
  Name=name;Job=CreateJobObject(IntPtr.Zero,name);Check(Job!=IntPtr.Zero,"CreateJob");
  try {
  var limits=new ExtendedLimit();limits.basic.flags=0x2000;Check(SetInformationJobObject(Job,9,ref limits,(uint)Marshal.SizeOf(typeof(ExtendedLimit))),"Job KILL_ON_CLOSE");
  var text=new StringBuilder(Quote(exe));foreach(string arg in args)text.Append(' ').Append(Quote(arg));
  var si=new SI();si.cb=(uint)Marshal.SizeOf(typeof(SI));si.flags=1;si.show=5;PI pi;
  Check(CreateProcess(exe,text,IntPtr.Zero,IntPtr.Zero,false,4|0x400,IntPtr.Zero,cwd,ref si,out pi),"Create suspended own browser");Process=pi.process;Pid=pi.pid;
  try {if(!AssignProcessToJobObject(Job,Process)){int assignError=Marshal.GetLastWin32Error();TerminateProcess(Process,2);throw new Win32Exception(assignError,"Assign suspended browser to own Job");}FT c,e,k,u;Check(GetProcessTimes(Process,out c,out e,out k,out u),"Get owned creation time");Creation=c.Value.ToString();if(ResumeThread(pi.thread)==0xffffffff)throw new Win32Exception(Marshal.GetLastWin32Error(),"Resume own browser");}finally{CloseHandle(pi.thread);}
  } catch {Dispose();throw;}
 }
 public bool Owns(uint pid){var p=OpenProcess(0x1000|0x100000,false,pid);if(p==IntPtr.Zero)return false;try{bool member;return IsProcessInJob(p,Job,out member)&&member;}finally{CloseHandle(p);}}
 public static ulong? PrivateOnHandle(IntPtr handle){if(WaitForSingleObject(handle,0)==0)return null;var counters=new MEMORY();counters.cb=(uint)Marshal.SizeOf(typeof(MEMORY));if(!GetProcessMemoryInfo(handle,ref counters,counters.cb)){if(WaitForSingleObject(handle,0)==0)return null;throw new Win32Exception(Marshal.GetLastWin32Error(),"Owned native process memory");}return WaitForSingleObject(handle,0)==0?(ulong?)null:counters.privateUsage.ToUInt64();}
 public FamilyMemory CurrentFamilyMemory(){var ids=Members();var sample=new FamilyMemory{members=ids.Length,complete=true};foreach(uint id in ids){var p=OpenProcess(0x1000|0x100000,false,id);if(p==IntPtr.Zero){sample.complete=false;continue;}try{bool owned;if(!IsProcessInJob(p,Job,out owned)||!owned){sample.complete=false;continue;}var bytes=PrivateOnHandle(p);if(bytes.HasValue){sample.partial_private_bytes+=bytes.Value;sample.sampled++;}else sample.complete=false;}finally{CloseHandle(p);}}sample.private_bytes=sample.complete?(ulong?)sample.partial_private_bytes:null;return sample;}
 public uint[] Members(){for(int capacity=64;capacity<=4096;capacity*=2){int size=8+IntPtr.Size*capacity;var p=Marshal.AllocHGlobal(size);try{if(!QueryInformationJobObject(Job,3,p,(uint)size,IntPtr.Zero)){if(Marshal.GetLastWin32Error()==234)continue;throw new Win32Exception(Marshal.GetLastWin32Error(),"Query own Job members");}int count=Marshal.ReadInt32(p,4);if(count<0||count>capacity)throw new InvalidOperationException("invalid Job list");var ids=new uint[count];for(int i=0;i<count;i++)ids[i]=checked((uint)Marshal.ReadIntPtr(p,8+IntPtr.Size*i).ToInt64());return ids;}finally{Marshal.FreeHGlobal(p);}}throw new InvalidOperationException("Job list exceeded bound");}
 public IntPtr FindWindow(string nonce){var found=new List<IntPtr>();EnumWindows((h,l)=>{uint pid;GetWindowThreadProcessId(h,out pid);if(pid==Pid&&Owns(pid)&&IsWindowVisible(h)&&!IsIconic(h)){var text=new StringBuilder(1024);GetWindowText(h,text,text.Capacity);if(text.ToString().Contains(nonce))found.Add(h);}return true;},IntPtr.Zero);if(found.Count>1)throw new InvalidOperationException("multiple owned nonce windows");return found.Count==1?found[0]:IntPtr.Zero;}
 public string Class(IntPtr hwnd){uint pid;GetWindowThreadProcessId(hwnd,out pid);if(pid!=Pid||!Owns(pid))throw new InvalidOperationException("foreign window");var text=new StringBuilder(256);GetClassName(hwnd,text,text.Capacity);return text.ToString();}
 public RECT Client(IntPtr hwnd){uint pid;GetWindowThreadProcessId(hwnd,out pid);if(pid!=Pid||!Owns(pid))throw new InvalidOperationException("foreign client");var previous=SetThreadDpiAwarenessContext(new IntPtr(-4));Check(previous!=IntPtr.Zero,"Physical client DPI context");try{RECT r;POINT p=new POINT();Check(GetClientRect(hwnd,out r),"Get own client");Check(ClientToScreen(hwnd,ref p),"Locate own client");return new RECT{left=p.x,top=p.y,right=p.x+r.right-r.left,bottom=p.y+r.bottom-r.top};}finally{Check(SetThreadDpiAwarenessContext(previous)!=IntPtr.Zero,"Restore supervisor DPI context");}}
 public void PlaceTopmost(IntPtr hwnd){uint pid;GetWindowThreadProcessId(hwnd,out pid);if(pid!=Pid||!Owns(pid))throw new InvalidOperationException("foreign placement");Check(SetWindowPos(hwnd,new IntPtr(-1),0,0,0,0,1|2|0x10|0x40),"Place own browser");}
[DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr hwnd, EnumProc callback, IntPtr data);
[DllImport("user32.dll")] static extern IntPtr GetParent(IntPtr hwnd);
[DllImport("user32.dll", EntryPoint="GetWindowLongPtrW")] static extern IntPtr GetWindowLongPtrForFixture(IntPtr hwnd, int index);
bool startup_geometry_frozen;
public sealed class ContentNativeSnapshot {
 public long hwnd, root_hwnd;
 public uint pid, tid, root_pid, root_tid, dpi;
 public string process_creation_filetime, root_process_creation_filetime, cls;
 public RECT raw_client, visible_client, root_client, root_window;
}
public sealed class ContentChildObservation {
 public long hwnd, parent_hwnd;
 public uint pid, tid;
 public string cls;
 public bool visible;
}
public ContentChildObservation[] LastOwnedContentChildren {get;private set;}
public int LastContentCandidateCount {get;private set;}
static RECT IntersectClient(RECT a, RECT b) {
 var r=new RECT{left=Math.Max(a.left,b.left),top=Math.Max(a.top,b.top),right=Math.Min(a.right,b.right),bottom=Math.Min(a.bottom,b.bottom)};
 if(r.left>=r.right||r.top>=r.bottom)throw new InvalidOperationException("Empty native content intersection");
 return r;
}
RECT OwnedClientPhysical(IntPtr h) {
 uint pid;GetWindowThreadProcessId(h,out pid);
 if(!Owns(pid))throw new InvalidOperationException("Foreign content client");
 RECT r;POINT p=new POINT();Check(GetClientRect(h,out r),"Own content client");Check(ClientToScreen(h,ref p),"Own content physical origin");
 return new RECT{left=p.x,top=p.y,right=p.x+r.right-r.left,bottom=p.y+r.bottom-r.top};
}
void CheckOriginalRoot(IntPtr h) {
 uint pid;GetWindowThreadProcessId(h,out pid);
 if(pid!=Pid||Exited||!Owns(pid))throw new InvalidOperationException("Original live root required");
 FT c,e,k,u;Check(GetProcessTimes(Process,out c,out e,out k,out u),"Original leader creation");
 if(c.Value.ToString()!=Creation)throw new InvalidOperationException("Original leader creation changed");
}
public void SetOwnNormalClientBeforeLease(IntPtr h) {
 if(startup_geometry_frozen)throw new InvalidOperationException("Cannot resize frozen lease geometry");
 CheckOriginalRoot(h);
 var old=SetThreadDpiAwarenessContext(new IntPtr(-4));Check(old!=IntPtr.Zero,"Fixture physical DPI context");
 try {
  // 不重写浏览器 style；只接受普通非最大化/非 fullscreen 自有 app 窗口。
  long style=GetWindowLongPtrForFixture(h,-16).ToInt64();
  if((style&0x40000000L)!=0||(style&0x01000000L)!=0||(style&0x00c00000L)!=0x00c00000L)throw new InvalidOperationException("Normal app root with caption style required");
  var client=Client(h);RECT outer;Check(GetWindowRect(h,out outer),"Own root window");
  int bw=outer.right-outer.left-(client.right-client.left),bh=outer.bottom-outer.top-(client.bottom-client.top);
  if(bw<0||bw>256||bh<0||bh>256)throw new InvalidOperationException("Own frame budget");
  // 正常 app 可处理 WINDOWPOSCHANGING；无需 NOSENDCHANGING。
  Check(SetWindowPos(h,new IntPtr(-1),60,60,1600+bw,1002+bh,0x10|0x20|0x40),"Set only own normal client before lease");
 }finally{Check(SetThreadDpiAwarenessContext(old)!=IntPtr.Zero,"Restore fixture DPI context");}
}
public ContentNativeSnapshot SnapshotOwnContentBeforeLease(IntPtr root) {
 CheckOriginalRoot(root);
 var old=SetThreadDpiAwarenessContext(new IntPtr(-4));Check(old!=IntPtr.Zero,"Content physical DPI context");
 try {
  var rootClient=Client(root);RECT rootWindow;Check(GetWindowRect(root,out rootWindow),"Own root bounds");
  uint rootPid;uint rootTid=GetWindowThreadProcessId(root,out rootPid);
  var candidates=new List<ContentNativeSnapshot>();var observed=new List<ContentChildObservation>();string error=null;int count=0;
  EnumChildWindows(root,(child,data)=>{
   try {
    if(++count>256)throw new InvalidOperationException("Bounded owned child enumeration");
    uint pid;uint tid=GetWindowThreadProcessId(child,out pid);
    // 仅诊断原Job/root子窗口；外部 PID 不读取 class、title、client 或 pixels。
    if(!Owns(pid)||GetAncestor(child,2)!=root)return true;
    var name=new StringBuilder(256);GetClassName(child,name,name.Capacity);
    bool isVisible=IsWindowVisible(child);
    observed.Add(new ContentChildObservation{hwnd=child.ToInt64(),parent_hwnd=GetParent(child).ToInt64(),pid=pid,tid=tid,cls=name.ToString(),visible=isVisible});
    if(!isVisible||name.ToString()!="Chrome_RenderWidgetHostHWND")return true;
    var process=OpenProcess(0x1000|0x100000,false,pid);Check(process!=IntPtr.Zero,"Hold original content process");
    try {
     bool member;Check(IsProcessInJob(process,Job,out member)&&member,"Held content process in own Job");
     FT c,e,k,u;Check(GetProcessTimes(process,out c,out e,out k,out u),"Content process creation");
     if(WaitForSingleObject(process,0)!=258)throw new InvalidOperationException("Content process exited");
     var raw=OwnedClientPhysical(child);var visible=IntersectClient(raw,rootClient);
     // 本夹具只接受原root的直接child，交集就是完整父客户区裁剪。
     if(GetParent(child)!=root)throw new InvalidOperationException("Direct owned renderer parent required");
     uint afterPid;uint afterTid=GetWindowThreadProcessId(child,out afterPid);
     if(afterPid!=pid||afterTid!=tid||GetAncestor(child,2)!=root)throw new InvalidOperationException("Content window identity changed");
     candidates.Add(new ContentNativeSnapshot{hwnd=child.ToInt64(),root_hwnd=root.ToInt64(),pid=pid,tid=tid,root_pid=rootPid,root_tid=rootTid,process_creation_filetime=c.Value.ToString(),root_process_creation_filetime=Creation,cls=name.ToString(),dpi=GetDpiForWindow(root),raw_client=raw,visible_client=visible,root_client=rootClient,root_window=rootWindow});
    }finally{CloseHandle(process);}
    return true;
   }catch(Exception e){error=e.Message;return false;}
  },IntPtr.Zero);
  LastOwnedContentChildren=observed.ToArray();LastContentCandidateCount=candidates.Count;
  if(error!=null)throw new InvalidOperationException(error);
  if(candidates.Count!=1)throw new InvalidOperationException("Exactly one visible owned native renderer required");
  CheckOriginalRoot(root);return candidates[0];
 }finally{Check(SetThreadDpiAwarenessContext(old)!=IntPtr.Zero,"Restore content DPI context");}
}
// 调用者确认 exact DOM inner/visual*dpr == visible_client 尺寸、unique DXGI ROI
// 反推 content origin 完全一致、两次稳定 snapshot 后冻结；raw_client/root_client/
// root_window/PID/TID/creation/child HWND/class/DPI 也必须后续完全一致。
public void FreezeStartupGeometryBeforeLease(){startup_geometry_frozen=true;}

 public void StopOwnedJob(){Check(TerminateJobObject(Job,2),"Terminate only own browser Job");}
 public string ClientReadiness(IntPtr hwnd){
  uint pid;GetWindowThreadProcessId(hwnd,out pid);if(pid!=Pid||!Owns(pid)||Exited)return "original browser identity unavailable";
  var previous=SetThreadDpiAwarenessContext(new IntPtr(-4));Check(previous!=IntPtr.Zero,"Readiness physical DPI context");
  try {var r=Client(hwnd);var visited=new HashSet<IntPtr>();
   for(var above=GetWindow(hwnd,3);above!=IntPtr.Zero;above=GetWindow(above,3)){
    if(visited.Count>=1024||!visited.Add(above))return "upper Z-order invalid";
    if(IsWindowVisible(above)&&!IsIconic(above)){RECT cover;if(!GetWindowRect(above,out cover))return "upper rectangle unavailable";
     if(cover.left<r.right&&r.left<cover.right&&cover.top<r.bottom&&r.top<cover.bottom){uint cloaked;if(DwmGetWindowAttribute(above,14,out cloaked,4)<0||cloaked==0){
      uint blockerPid;GetWindowThreadProcessId(above,out blockerPid);var blockerClass=new StringBuilder(128);GetClassName(above,blockerClass,blockerClass.Capacity);
      bool owned=Owns(blockerPid);var title=new StringBuilder(512);if(owned)GetWindowText(above,title,title.Capacity);
      return "upper visible window hwnd="+above.ToInt64().ToString("X")+" pid="+blockerPid+" owned="+owned+" class="+blockerClass+" owned_title="+title+" rect="+cover.left+","+cover.top+","+cover.right+","+cover.bottom;
     }}
    }
   }
   int[] xs={r.left,r.left+(r.right-r.left)/2,r.right-1},ys={r.top,r.top+(r.bottom-r.top)/2,r.bottom-1};
   foreach(int x in xs)foreach(int y in ys){var hit=WindowFromPoint(new POINT{x=x,y=y});uint hitPid;GetWindowThreadProcessId(hit,out hitPid);if(GetAncestor(hit,2)!=hwnd||!Owns(hitPid))return "client hit point not original owned root";}
   return null;
  }finally{Check(SetThreadDpiAwarenessContext(previous)!=IntPtr.Zero,"Restore readiness DPI context");}
 }
 public void Dispose(){if(Process!=IntPtr.Zero){CloseHandle(Process);Process=IntPtr.Zero;}if(Job!=IntPtr.Zero){CloseHandle(Job);Job=IntPtr.Zero;}}
}
// 原始 Node 句柄与创建/退出时间限定其唯一子进程，清理不按进程名或可复用的 PID 操作。
public sealed class RustShotBrowserStressChild:IDisposable {
 [StructLayout(LayoutKind.Sequential)] struct MEMORY {public uint cb,faults;public UIntPtr peakWS,workingSet,peakPaged,paged,peakNonPaged,nonPaged,pagefile,peakPagefile,privateUsage;}
 [DllImport("psapi.dll",SetLastError=true)] static extern bool GetProcessMemoryInfo(IntPtr p,ref MEMORY counters,uint bytes);
 [StructLayout(LayoutKind.Sequential)] struct FT {public uint low,high;public ulong Value {get{return ((ulong)high<<32)|low;}}}
 [StructLayout(LayoutKind.Sequential)] struct PBI {public IntPtr reserved1,peb,reserved2a,reserved2b,pid,parent;}
 [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
 [DllImport("kernel32.dll")] static extern bool GetProcessTimes(IntPtr p,out FT c,out FT e,out FT k,out FT u);
 [DllImport("kernel32.dll")] static extern uint GetProcessId(IntPtr p);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool QueryFullProcessImageName(IntPtr p,uint flags,StringBuilder text,ref uint size);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr p,uint code);
 [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr p,uint timeout);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr p);
 [DllImport("ntdll.dll")] static extern int NtQueryInformationProcess(IntPtr p,int cls,out PBI info,uint bytes,out uint returned);
 public uint Pid {get;private set;} public ulong Creation {get;private set;} public IntPtr Handle {get;private set;}
 public bool Exited {get{return Handle!=IntPtr.Zero&&WaitForSingleObject(Handle,0)==0;}}
 // 原生原始句柄采样；结束后返回unavailable，不按可复用PID打开别的进程。
 public ulong? CurrentPrivateBytes(){if(Exited)return null;var counters=new MEMORY();counters.cb=(uint)Marshal.SizeOf(typeof(MEMORY));if(!GetProcessMemoryInfo(Handle,ref counters,counters.cb)){if(Exited)return null;throw new Win32Exception(Marshal.GetLastWin32Error(),"Owned Rust memory sample");}return Exited?(ulong?)null:counters.privateUsage.ToUInt64();}
 public static RustShotBrowserStressChild Open(IntPtr originalNode,uint pid,string expectedExe){
  var p=OpenProcess(0x1000|0x100000|1,false,pid);if(p==IntPtr.Zero)return null;
  try {FT c,e,k,u,nc,ne,nk,nu;PBI info;uint returned;
   if(!GetProcessTimes(p,out c,out e,out k,out u)||!GetProcessTimes(originalNode,out nc,out ne,out nk,out nu)||NtQueryInformationProcess(p,0,out info,(uint)Marshal.SizeOf(typeof(PBI)),out returned)<0)throw new InvalidOperationException("cannot validate owned Rust identity");
   if((ulong)info.parent.ToInt64()!=GetProcessId(originalNode)||c.Value<nc.Value||(ne.Value!=0&&c.Value>ne.Value))throw new InvalidOperationException("Rust is not a child of the original Node lifetime");
   var text=new StringBuilder(32768);uint size=(uint)text.Capacity;
   if(!QueryFullProcessImageName(p,0,text,ref size)||!String.Equals(text.ToString(),expectedExe,StringComparison.OrdinalIgnoreCase))throw new InvalidOperationException("owned Rust executable changed");
   var result=new RustShotBrowserStressChild{Pid=pid,Creation=c.Value,Handle=p};p=IntPtr.Zero;return result;
  } finally {if(p!=IntPtr.Zero)CloseHandle(p);}
 }
 public void Stop(){if(!Exited){if(!TerminateProcess(Handle,2))throw new Win32Exception(Marshal.GetLastWin32Error(),"Terminate owned Rust handle");if(WaitForSingleObject(Handle,5000)!=0)throw new InvalidOperationException("owned Rust did not exit");}}
 public void Dispose(){if(Handle!=IntPtr.Zero){CloseHandle(Handle);Handle=IntPtr.Zero;}}
}
'@
}
$taskNonce=[guid]::NewGuid().ToString('N')
function Get-BrowserSources {
 @(@(foreach($taskDir in @('src','assets','vendor')){Get-ChildItem -LiteralPath (Join-Path $taskRoot $taskDir) -Recurse -File};foreach($taskFile in @('Cargo.toml','Cargo.lock','build.rs','build.ps1','.cargo/config.toml','Tools/verify-long-auto.ps1','Tools/verify-long-region.ps1','Tools/verify-long-pipeline.ps1','Tools/verify-long-dxgi-memory.ps1','Tools/verify-long-browser.ps1','Tools/qa-long-browser-control.cjs','Tools/qa-long-browser-page.html','Tools/verify-long-browser-stress.ps1','Tools/qa-long-browser-stress-control.cjs','Tools/qa-long-browser-stress-page.html')){Get-Item -LiteralPath (Join-Path $taskRoot $taskFile)})|Sort-Object FullName -Unique|ForEach-Object{[pscustomobject]@{path=$_.FullName.Substring($taskRoot.Length+1).Replace('\','/');sha256=(Get-FileHash -LiteralPath $_.FullName).Hash}})
}
if(-not $PreflightOnly){
 $taskBuildProof=Get-Content -Raw -LiteralPath (Join-Path $taskRoot $BuildVerification)|ConvertFrom-Json
 if($taskBuildProof.ok-ne $true){throw '浏览器原生验收需要本轮最终Release构建证明'}
 $taskBuildSources=Get-Content -Raw -LiteralPath (Join-Path $taskRoot $taskBuildProof.source_manifest)|ConvertFrom-Json
 if(($taskBuildSources|ConvertTo-Json -Depth 4)-cne (Get-BrowserSources|ConvertTo-Json -Depth 4)){throw '构建来源集合或内容已改变'}
 foreach($taskSource in $taskBuildSources){if((Get-FileHash -LiteralPath (Join-Path $taskRoot $taskSource.path)).Hash-cne $taskSource.sha256){throw "构建后来源有变化：$($taskSource.path)"}}
 foreach($taskFile in $taskBuildProof.files){$taskFilePath=Join-Path $taskRoot "$TargetDirectory/release/$($taskFile.name)";if((Get-FileHash -LiteralPath $taskFilePath).Hash-cne $taskFile.sha256){throw "浏览器原生验收二进制不是本轮最终版：$($taskFile.name)"}}
}
$taskFolder=Join-Path $taskRoot ('target/qa/long-browser-stress-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff')+'-'+$taskNonce.Substring(0,6))
[void](New-Item -ItemType Directory -Path $taskFolder)
$taskProfile=Join-Path $taskFolder 'profile'
[void](New-Item -ItemType Directory -Path $taskProfile)
if((Get-Item -LiteralPath $taskProfile).Attributes-band [IO.FileAttributes]::ReparsePoint){throw '测试profile不能为符号链接或junction'}
$taskDefaultProfile=Join-Path $taskProfile 'Default'
[void](New-Item -ItemType Directory -Path $taskDefaultProfile)
# 仅此新建隔离 profile：翻译弹窗会遮挡首个 whole-client 校准，禁止在测试页弹出。
@{translate=@{enabled=$false}}|ConvertTo-Json -Depth 3|Set-Content -LiteralPath (Join-Path $taskDefaultProfile 'Preferences') -Encoding utf8
$taskHtml=Join-Path $taskFolder 'page.html'
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'qa-long-browser-stress-page.html') -Destination $taskHtml
$taskUrl=([uri]$taskHtml).AbsoluteUri+'?nonce='+$taskNonce
$taskBrowser=$null;$taskHelper=$null;$taskSuccess=$false;$taskForced=$false;$taskRustPid=$null;$taskRustHandle=$null
$taskCleanupErrors=[System.Collections.Generic.List[string]]::new()
$taskPlacementRequested=$false;$taskPlacementAck=$false;$taskPlacementClearCount=0;$taskPlacementDeadline=$null
$taskPlacementObservations=[System.Collections.Generic.List[object]]::new()
$taskPeakBrowser=0L;$taskPeakNode=0L;$taskPeakRust=0L;$taskSamples=[System.Collections.Generic.List[object]]::new()
$taskSamplingClock=[Diagnostics.Stopwatch]::StartNew();$taskPreviousSampleMs=$null;$taskMaximumSampleGapMs=0.0
try {
 $taskArguments=@("--user-data-dir=$taskProfile",'--remote-debugging-port=0','--window-position=48,48','--window-size=1280,800',"--app=$taskUrl",'--no-first-run','--no-default-browser-check','--disable-background-networking','--disable-sync','--disable-component-update','--disable-extensions','--disable-breakpad','--disable-smooth-scrolling','--disable-features=Translate')
 $taskBrowser=[RustShotBrowserStressSupervisor]::new($taskChrome,$taskArguments,$taskRoot,'RustShot.LongBrowserStress.QA.'+$taskNonce)
 $taskDeadline=(Get-Date).AddSeconds(40)
 $taskEndpointFile=Join-Path $taskProfile 'DevToolsActivePort';$taskWindow=[IntPtr]::Zero
 do {
  if($taskBrowser.Exited){throw '自有Chrome在启动校准前已退出'}
  if(Test-Path -LiteralPath $taskEndpointFile){$taskPortLines=Get-Content -LiteralPath $taskEndpointFile;if($taskPortLines.Count-ge 2){$taskWindow=$taskBrowser.FindWindow($taskNonce)}}
  if($taskWindow-ne [IntPtr]::Zero){break}
  Start-Sleep -Milliseconds 100
 }while((Get-Date)-lt $taskDeadline)
 if($taskWindow-eq [IntPtr]::Zero){throw '自有Chrome窗口或调试端口未就绪；没有启动第二实例'}
 $taskPort=[int]$taskPortLines[0]
 if($taskPort-le 0 -or $taskPort-gt 65535 -or $taskPortLines[1]-notmatch '^/devtools/browser/[A-Za-z0-9-]+$'){throw '自有调试端口格式无效'}
 $taskSockets=@(Get-NetTCPConnection -State Listen -LocalPort $taskPort -ErrorAction Stop)
 if($taskSockets.Count-eq 0 -or @($taskSockets|Where-Object {$_.LocalAddress-notin @('127.0.0.1','::1') -or -not $taskBrowser.Owns($_.OwningProcess)}).Count-ne 0){throw 'CDP侦听端口不属于自有Job或不是loopback'}
 $taskBrowser.PlaceTopmost($taskWindow)
 $taskBrowser.SetOwnNormalClientBeforeLease($taskWindow)
 $taskFixtureDeadline=(Get-Date).AddSeconds(10);$taskFixturePrevious=$null;$taskFixtureStable=0
 $taskFixtureObservations=[Collections.Generic.List[object]]::new()
 do {
  $taskRect=$taskBrowser.Client($taskWindow)
  if(($taskRect.right-$taskRect.left)-ne 1600 -or ($taskRect.bottom-$taskRect.top)-ne 1002){Start-Sleep -Milliseconds 100;continue}
  try {$taskContent=$taskBrowser.SnapshotOwnContentBeforeLease($taskWindow)}catch{
   $taskFixtureError=$_.Exception
   while($null-ne $taskFixtureError.InnerException){$taskFixtureError=$taskFixtureError.InnerException}
   if($taskFixtureError.Message-cne 'Exactly one visible owned native renderer required'){throw}
   # 尚未创建lease或启动读取；只在原Job/root中等待唯一renderer，不重绑运行中会话。
   $taskFixturePrevious=$null;$taskFixtureStable=0
   $taskFixtureObservations.Add([pscustomobject]@{utc=(Get-Date).ToUniversalTime().ToString('o');ready=$false;error=$taskFixtureError.Message;candidate_count=$taskBrowser.LastContentCandidateCount;owned_children=$taskBrowser.LastOwnedContentChildren})
   $taskFixtureObservations|ConvertTo-Json -Depth 6|Set-Content -LiteralPath (Join-Path $taskFolder 'startup-content-observations.json') -Encoding utf8
   Start-Sleep -Milliseconds 100;continue
  }
  $taskContentStamp=$taskContent|ConvertTo-Json -Depth 6 -Compress
  $taskFixtureObservations.Add([pscustomobject]@{utc=(Get-Date).ToUniversalTime().ToString('o');ready=$true;content=$taskContent;candidate_count=$taskBrowser.LastContentCandidateCount;owned_children=$taskBrowser.LastOwnedContentChildren})
  $taskFixtureObservations|ConvertTo-Json -Depth 6|Set-Content -LiteralPath (Join-Path $taskFolder 'startup-content-observations.json') -Encoding utf8
  if($taskContentStamp-ceq $taskFixturePrevious){$taskFixtureStable++}else{$taskFixturePrevious=$taskContentStamp;$taskFixtureStable=1}
  if($taskFixtureStable-ge 2){break}
  Start-Sleep -Milliseconds 100
 }while((Get-Date)-lt $taskFixtureDeadline)
 if($taskFixtureStable-lt 2){throw 'normal app/root/renderer未稳定，尚未创建lease'}
 $taskBrowser.FreezeStartupGeometryBeforeLease()
 $taskNativeOwner=[ordered]@{job_name=$taskBrowser.Name;browser_pid=$taskBrowser.Pid;browser_creation_filetime=$taskBrowser.Creation;hwnd=$taskWindow.ToInt64();class=$taskBrowser.Class($taskWindow);title_nonce=$taskNonce;client_rect=[ordered]@{left=$taskRect.left;top=$taskRect.top;width=$taskRect.right-$taskRect.left;height=$taskRect.bottom-$taskRect.top};dpi=[RustShotBrowserStressSupervisor]::GetDpiForWindow($taskWindow);expected_document_nonce=$taskNonce;expected_target_id=''}
 $taskNativeOwner.content_window=[ordered]@{hwnd=$taskContent.hwnd;window_pid=$taskContent.pid;window_thread=$taskContent.tid;class=$taskContent.cls;creation_filetime=$taskContent.process_creation_filetime;client_rect=[ordered]@{left=$taskContent.raw_client.left;top=$taskContent.raw_client.top;width=$taskContent.raw_client.right-$taskContent.raw_client.left;height=$taskContent.raw_client.bottom-$taskContent.raw_client.top}}
 $taskConfig=[ordered]@{protocol=1;endpoint_url=('ws://127.0.0.1:'+$taskPort+$taskPortLines[1]);own_url=$taskUrl;nonce=$taskNonce;artifact_root=$taskFolder;rust_exe=[IO.Path]::GetFullPath((Join-Path $taskRoot "$TargetDirectory/release/raishot-gpui.exe"));native_owner=$taskNativeOwner;playwright_module=(Join-Path $taskModules 'playwright');pngjs_module=(Join-Path $taskModules 'pngjs');preflight_only=$PreflightOnly.IsPresent}
 $taskConfigFile=Join-Path $taskFolder 'config.json'
 $taskConfig|ConvertTo-Json -Depth 8|Set-Content -LiteralPath $taskConfigFile -Encoding utf8
 $taskMetadata=[pscustomobject]@{suite='long-browser-stress';nonce=$taskNonce;profile=$taskProfile;browser_path=$taskChrome;browser_sha256=(Get-FileHash -LiteralPath $taskChrome).Hash;browser_version=(Get-Item -LiteralPath $taskChrome).VersionInfo.FileVersion;browser_pid=$taskBrowser.Pid;browser_creation_filetime=$taskBrowser.Creation;job_name=$taskBrowser.Name;job_assigned_before_resume=$true;breakaway_allowed=$false;native_owner=$taskNativeOwner;loopback_endpoint_owned=$true;initial_job_members=@($taskBrowser.Members());preflight_only=$PreflightOnly.IsPresent;source_manifest=if($PreflightOnly){$null}else{$taskBuildProof.source_manifest};binary_files=if($PreflightOnly){@()}else{$taskBuildProof.files};qa_sources=@(foreach($taskQaFile in @('verify-long-browser-stress.ps1','qa-long-browser-stress-control.cjs','qa-long-browser-stress-page.html')){[pscustomobject]@{path="Tools/$taskQaFile";sha256=(Get-FileHash -LiteralPath (Join-Path $PSScriptRoot $taskQaFile)).Hash}});full_desktop_freeze_used=$false;global_input_sent=$false;clipboard_untouched=$true}
 $taskMetadata|Add-Member -NotePropertyName fixture_window_mode -NotePropertyValue 'normal-app-renderer-visible-client'
 $taskMetadata|Add-Member -NotePropertyName fixture_client_physical_size -NotePropertyValue @(1600,1002)
 $taskMetadata|Add-Member -NotePropertyName fixture_sizing_completed_before_lease -NotePropertyValue $true
 $taskMetadata|Add-Member -NotePropertyName fullscreen_kiosk_used -NotePropertyValue $false
 $taskMetadata|Add-Member -NotePropertyName content_direct_parent_required -NotePropertyValue $true
 $taskMetadata|ConvertTo-Json -Depth 8|Set-Content -LiteralPath (Join-Path $taskFolder 'supervisor-start.json') -Encoding utf8
 $taskHelper=Start-Process -FilePath $taskNode -ArgumentList @([RustShotBrowserStressSupervisor]::Quote((Join-Path $PSScriptRoot 'qa-long-browser-stress-control.cjs')),[RustShotBrowserStressSupervisor]::Quote($taskConfigFile)) -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskFolder 'node-stdout.jsonl') -RedirectStandardError (Join-Path $taskFolder 'node-stderr.log')
 $taskDeadline=(Get-Date).AddSeconds(780)
 while(-not $taskHelper.WaitForExit(100)) {
  $taskReadyFile=Join-Path $taskFolder 'node-ready.json'
  if(-not $PreflightOnly -and -not $taskPlacementAck -and (Test-Path -LiteralPath $taskReadyFile)){
   if(-not $taskPlacementRequested){
    $taskReady=Get-Content -Raw -LiteralPath $taskReadyFile|ConvertFrom-Json
    if($taskReady.pid-ne $taskHelper.Id -or $taskReady.browser_pid-ne $taskBrowser.Pid -or $taskReady.document_nonce-cne $taskNonce -or $taskReady.native_client_exact-ne $false -or $taskReady.native_content_exact-ne $true){throw '页面ready不属于本调用原进程/文档/正常app内容区域'}
    $taskVisible=$taskReady.native_visible_content_rect
    if($taskVisible.left-ne $taskContent.visible_client.left -or $taskVisible.top-ne $taskContent.visible_client.top -or $taskVisible.width-ne ($taskContent.visible_client.right-$taskContent.visible_client.left) -or $taskVisible.height-ne ($taskContent.visible_client.bottom-$taskContent.visible_client.top)){throw '页面可见content矩形不等原生直接child与父客户区交集'}
    $taskBrowser.PlaceTopmost($taskWindow);$taskPlacementRequested=$true;$taskPlacementDeadline=(Get-Date).AddSeconds(8)
   }
   $taskClientNow=$taskBrowser.Client($taskWindow)
   if($taskClientNow.left-ne $taskRect.left -or $taskClientNow.top-ne $taskRect.top -or $taskClientNow.right-ne $taskRect.right -or $taskClientNow.bottom-ne $taskRect.bottom -or [RustShotBrowserStressSupervisor]::GetDpiForWindow($taskWindow)-ne $taskNativeOwner.dpi){throw '启动呈现后客户区/DPI改变，需要重新校准'}
   $taskContentNow=$taskBrowser.SnapshotOwnContentBeforeLease($taskWindow)
   if(($taskContentNow|ConvertTo-Json -Depth 6 -Compress)-cne $taskContentStamp){throw '启动呈现后原生content/root身份或原始矩形改变'}
   $taskBlocked=$taskBrowser.ClientReadiness($taskWindow)
   $taskPlacementObservations.Add([pscustomobject]@{utc=(Get-Date).ToUniversalTime().ToString('o');blocked=$taskBlocked})
   if($null-eq $taskBlocked){$taskPlacementClearCount++}else{
    $taskPlacementClearCount=0
    # 仅重新排列本调用 Chrome；下一次仍完整核验客户区与九点，绝不豁免遮挡。
    $taskBrowser.PlaceTopmost($taskWindow)
   }
   if($taskPlacementClearCount-ge 2){
    $taskAckPending=Join-Path $taskFolder 'supervisor-placement-ack.pending.json'
    [pscustomobject]@{ok=$true;nonce=$taskNonce;browser_pid=$taskBrowser.Pid;hwnd=$taskWindow.ToInt64();native_client_rect=$taskNativeOwner.client_rect;whole_client_zorder_and_nine_points_checked=$true;stable_checks=$taskPlacementClearCount;observations=$taskPlacementObservations}|ConvertTo-Json -Depth 5|Set-Content -LiteralPath $taskAckPending -Encoding utf8
    Move-Item -LiteralPath $taskAckPending -Destination (Join-Path $taskFolder 'supervisor-placement-ack.json')
    $taskPlacementAck=$true
   }elseif((Get-Date)-ge $taskPlacementDeadline){throw '原自有浏览器客户区未稳定无遮挡；没有读取被遮挡像素'}
  }
  $taskFamilySample=$taskBrowser.CurrentFamilyMemory();$taskFamilyPrivate=$taskFamilySample.private_bytes
  $taskNodeCurrent=[RustShotBrowserStressSupervisor]::PrivateOnHandle($taskHelper.Handle)
  if($null-ne $taskFamilyPrivate){$taskPeakBrowser=[math]::Max($taskPeakBrowser,[long]$taskFamilyPrivate)}
  if($null-ne $taskNodeCurrent){$taskPeakNode=[math]::Max($taskPeakNode,[long]$taskNodeCurrent)}
  $taskRustInfoFile=Join-Path $taskFolder 'rust-process.json'
  if(-not $taskRustHandle -and (Test-Path -LiteralPath $taskRustInfoFile)){$taskRustInfo=Get-Content -Raw -LiteralPath $taskRustInfoFile|ConvertFrom-Json;if($taskRustInfo.parent_pid-ne $taskHelper.Id){throw 'Rust过程记录不属于原Node'};$taskRustHandle=[RustShotBrowserStressChild]::Open($taskHelper.Handle,[uint32]$taskRustInfo.pid,$taskConfig.rust_exe);if($taskRustHandle){$taskRustPid=$taskRustHandle.Pid}}
  $taskRustCurrent=$null
  if($taskRustHandle){$taskRustCurrent=$taskRustHandle.CurrentPrivateBytes();if($null-ne $taskRustCurrent){$taskPeakRust=[math]::Max($taskPeakRust,[long]$taskRustCurrent)}}
  $taskSampleMs=$taskSamplingClock.Elapsed.TotalMilliseconds;$taskSampleGapMs=$null
  if($null-ne $taskPreviousSampleMs){$taskSampleGapMs=$taskSampleMs-$taskPreviousSampleMs;$taskMaximumSampleGapMs=[math]::Max($taskMaximumSampleGapMs,$taskSampleGapMs)}
  $taskPreviousSampleMs=$taskSampleMs
  $taskSamples.Add([pscustomobject]@{utc=(Get-Date).ToUniversalTime().ToString('o');monotonic_elapsed_ms=$taskSampleMs;actual_sample_gap_ms=$taskSampleGapMs;browser_family_private_bytes=$taskFamilyPrivate;browser_sample_complete=$taskFamilySample.complete;browser_members=$taskFamilySample.members;browser_members_sampled=$taskFamilySample.sampled;browser_partial_private_bytes=$taskFamilySample.partial_private_bytes;node_private_bytes=$taskNodeCurrent;rust_private_bytes=$taskRustCurrent;rust_sample_available=$null-ne $taskRustCurrent;rust_pid=$taskRustPid;rust_creation_filetime=if($taskRustHandle){$taskRustHandle.Creation.ToString()}else{$null};rust_sample_uses_original_native_handle=$true;rust_private_peak_so_far_bytes=$taskPeakRust})
  if((Get-Date)-ge $taskDeadline){throw '自有浏览器验收超时，保留同一调用证据；未启动替代实例'}
 }
 $taskHelper.Refresh()
 $taskResultFile=Join-Path $taskFolder 'node-result.json'
 if(-not (Test-Path -LiteralPath $taskResultFile)){throw "Node没有完成报告：exit $($taskHelper.ExitCode)"}
 $taskResult=Get-Content -Raw -LiteralPath $taskResultFile|ConvertFrom-Json
 if($taskHelper.ExitCode-ne 0 -or $taskResult.ok-ne $true){throw "隔离浏览器验收失败：$($taskResult.error)"}
 $taskDeadline=(Get-Date).AddSeconds(12)
 while(@($taskBrowser.Members()).Count-ne 0 -and (Get-Date)-lt $taskDeadline){Start-Sleep -Milliseconds 100}
 if(-not $taskBrowser.Exited -or @($taskBrowser.Members()).Count-ne 0){throw 'Browser.close 后自有Chrome Job仍有进程，不能算正常清理'}
 if($taskRustHandle -and -not $taskRustHandle.Exited){throw '自有Rust验收进程仍未结束'}
 if(-not $PreflightOnly){
  if(($taskBuildSources|ConvertTo-Json -Depth 4)-cne (Get-BrowserSources|ConvertTo-Json -Depth 4)){throw '原生验收期间来源集合或内容改变'}
  foreach($taskFile in $taskBuildProof.files){if((Get-FileHash -LiteralPath (Join-Path $taskRoot "$TargetDirectory/release/$($taskFile.name)")).Hash-cne $taskFile.sha256){throw "原生验收期间产物改变：$($taskFile.name)"}}
  [pscustomobject]@{ok=$true;source_count=$taskBuildSources.Count;source_manifest=$taskBuildProof.source_manifest;binary_files=$taskBuildProof.files}|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskFolder 'source-binary-after.json') -Encoding utf8
 }
 $taskSuccess=$true
 [pscustomobject]@{ok=$true;preflight_only=$PreflightOnly.IsPresent;node_pid=$taskHelper.Id;node_exit_code=$taskHelper.ExitCode;browser_normal_exit=$true;job_members_after_close=0;sample_target_interval_ms=100;sample_interval_is_target_not_exact=$true;maximum_actual_sample_gap_ms=$taskMaximumSampleGapMs;actual_sample_count=$taskSamples.Count;rust_sample_uses_original_native_handle=$true;sampled_peak_browser_family_private_mib=[math]::Round($taskPeakBrowser/1MB,2);sampled_peak_node_private_mib=[math]::Round($taskPeakNode/1MB,2);sampled_peak_rust_private_mib=[math]::Round($taskPeakRust/1MB,2);result=$taskResultFile;supervisor=(Join-Path $taskFolder 'supervisor-start.json');full_desktop_freeze_used=$false;global_input_sent=$false;clipboard_untouched=$true}|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $taskFolder 'runner-report.json') -Encoding utf8
 Write-Output $taskFolder
}finally {
 # 即使Node在第一次轮询前退出，也重新读取本次记录；原Node句柄仍锁定其真实存活区间。
 try{if(-not $taskRustHandle -and $taskHelper -and (Test-Path -LiteralPath (Join-Path $taskFolder 'rust-process.json'))){$taskRustInfo=Get-Content -Raw -LiteralPath (Join-Path $taskFolder 'rust-process.json')|ConvertFrom-Json;if($taskRustInfo.parent_pid-ne $taskHelper.Id){throw '清理过程记录不属于原Node'};$taskRustHandle=[RustShotBrowserStressChild]::Open($taskHelper.Handle,[uint32]$taskRustInfo.pid,$taskConfig.rust_exe);if($taskRustHandle){$taskRustPid=$taskRustHandle.Pid}}}catch{$taskCleanupErrors.Add($_.Exception.Message);$taskForced=$true}
 try{if($taskHelper){$taskHelper.Refresh();if(-not $taskHelper.HasExited){Stop-Process -InputObject $taskHelper -Force -ErrorAction Stop;[void]$taskHelper.WaitForExit(5000);$taskForced=$true}}}catch{$taskCleanupErrors.Add($_.Exception.Message);$taskForced=$true}
 if($taskRustHandle){try{if(-not $taskRustHandle.Exited){$taskRustHandle.Stop();$taskForced=$true}}catch{$taskCleanupErrors.Add($_.Exception.Message);$taskForced=$true}finally{$taskRustHandle.Dispose()}}
 $taskRemaining=@()
 if($taskBrowser){try{$taskRemaining=@($taskBrowser.Members());if($taskRemaining.Count-ne 0){$taskForced=$true;$taskBrowser.StopOwnedJob()}}catch{$taskCleanupErrors.Add($_.Exception.Message);$taskForced=$true;try{$taskBrowser.StopOwnedJob()}catch{$taskCleanupErrors.Add($_.Exception.Message)}}finally{$taskBrowser.Dispose()}}
 [pscustomobject]@{ok=$taskSuccess -and -not $taskForced -and $taskCleanupErrors.Count-eq 0;forced_cleanup=$taskForced;cleanup_errors=$taskCleanupErrors;browser_job_remaining_before_dispose=$taskRemaining;node_pid=if($taskHelper){$taskHelper.Id}else{$null};rust_pid=$taskRustPid;scope='仅本调用持有的Chrome Job/Node/Rust句柄；forced cleanup不计成功'}|ConvertTo-Json -Depth 4|Set-Content -LiteralPath (Join-Path $taskFolder 'cleanup.json') -Encoding utf8
 $taskSamples|ConvertTo-Json -Depth 3|Set-Content -LiteralPath (Join-Path $taskFolder 'process-memory-samples.json') -Encoding utf8
 $taskPlacementObservations|ConvertTo-Json -Depth 3|Set-Content -LiteralPath (Join-Path $taskFolder 'supervisor-placement-observations.json') -Encoding utf8
}

