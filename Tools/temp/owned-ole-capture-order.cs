using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Threading;

// 独立受控探针；不进入 RustShot，不读取桌面或剪贴板，源永不返回 DROP。
public static class OwnedOleCaptureOrder
{
    const uint WM_CAPTURECHANGED = 0x0215, WM_TIMER = 0x0113, WM_KEYDOWN = 0x0100;
    const int CANCEL = 0x00040101;
    static readonly object Gate = new object();
    static readonly List<ProbeEvent> Events = new List<ProbeEvent>();
    static readonly Stopwatch Clock = Stopwatch.StartNew();
    static WndProc WindowProcRoot = WindowProc;
    static TimerProc TimerProcRoot = Tick;
    static string LivePath;
    static string CaseName;
    static int Sequence, Phase, QueryCount, TransferCount, NullTransfers;
    static uint OwnerThread, StaThread;
    static IntPtr Owner, StaControl;
    static bool ContinueFirst;
    static int WakeCount;

    public sealed class ProbeEvent
    {
        public int Sequence { get; set; }
        public double Milliseconds { get; set; }
        public string Case { get; set; }
        public uint Thread { get; set; }
        public string Event { get; set; }
        public string Details { get; set; }
    }
    public sealed class CaseResult
    {
        public string Name { get; set; }
        public string Api { get; set; }
        public bool ForcedFirstContinue { get; set; }
        public uint OwnerThread { get; set; }
        public uint StaThread { get; set; }
        public int Queries { get; set; }
        public int DirectStaTransfers { get; set; }
        public int NullTransfers { get; set; }
        public int WakeMessages { get; set; }
        public string HResult { get; set; }
        public uint Effect { get; set; }
        public bool Finished { get; set; }
        public string Error { get; set; }
    }
    public sealed class Report
    {
        public int Pid { get; set; }
        public List<CaseResult> Cases { get; set; }
        public List<ProbeEvent> Events { get; set; }
        public bool NoGlobalInput { get; set; }
        public bool NoDesktopRead { get; set; }
        public bool NoClipboard { get; set; }
        public bool NeverDrop { get; set; }
    }
    static string Handle(IntPtr h) { return "0x" + h.ToInt64().ToString("X"); }
    static void Log(string name, string details)
    {
        lock (Gate)
        {
            var e = new ProbeEvent { Sequence = ++Sequence, Milliseconds = Clock.Elapsed.TotalMilliseconds,
                Case = CaseName, Thread = GetCurrentThreadId(), Event = name, Details = details };
            Events.Add(e);
            File.AppendAllText(LivePath, e.Sequence + "\t" + e.Milliseconds.ToString("F3") + "\t" + e.Case + "\t" + e.Thread + "\t" + name + "\t" + details + Environment.NewLine);
        }
    }
    public static Report Run(string png, string livePath)
    {
        if (!Path.IsPathRooted(png) || !File.Exists(png)) throw new ArgumentException("需要自有 PNG 绝对路径");
        LivePath = livePath;
        File.WriteAllText(LivePath, "sequence\tms\tcase\tthread\tevent\tdetails\n");
        int complete = 0;
        var watchdog = new Thread(delegate() {
            Thread.Sleep(12000);
            if (Volatile.Read(ref complete) == 0)
            {
                Log("PROCESS_HARD_TIMEOUT", "仅结束自有探针进程；无外部 DROP");
                Environment.Exit(124);
            }
        });
        watchdog.IsBackground = true;
        watchdog.Start();
        var cases = new List<CaseResult>();
        cases.Add(RunCase("shell-first-query", true, false, png));
        cases.Add(RunCase("shell-forced-first-continue", true, true, png));
        cases.Add(RunCase("ole-first-query", false, false, png));
        Volatile.Write(ref complete, 1);
        return new Report { Pid = Process.GetCurrentProcess().Id, Cases = cases, Events = new List<ProbeEvent>(Events),
            NoGlobalInput = true, NoDesktopRead = true, NoClipboard = true, NeverDrop = true };
    }
    static CaseResult RunCase(string name, bool shell, bool forceContinue, string png)
    {
        CaseName = name; Phase = 1; QueryCount = TransferCount = NullTransfers = WakeCount = 0;
        StaThread = 0; ContinueFirst = forceContinue;
        CaseResult result = null;
        Exception error = null;
        var ready = new ManualResetEvent(false);
        var done = new ManualResetEvent(false);
        var ownerThread = new Thread(delegate() {
            OwnerThread = GetCurrentThreadId();
            Owner = MakeWindow("owner");
            SetCapture(Owner);
            Log("OWNER_CAPTURE", "owner=" + Handle(Owner) + ";capture=" + Handle(GetCapture()));
            ready.Set();
            while (!done.WaitOne(0))
            {
                MSG msg;
                while (PeekMessageW(out msg, IntPtr.Zero, 0, 0, 1))
                {
                    TranslateMessage(ref msg);
                    DispatchMessageW(ref msg);
                }
                Thread.Sleep(1);
            }
            if (GetCapture() == Owner) ReleaseCapture();
            DestroyWindow(Owner);
            Owner = IntPtr.Zero;
        });
        ownerThread.IsBackground = true;
        ownerThread.SetApartmentState(ApartmentState.STA);
        ownerThread.Start();
        if (!ready.WaitOne(1000)) throw new Exception("自有 owner HWND 未就绪");
        var sta = new Thread(delegate() {
            bool initialized = false;
            IDataObject data = null;
            try
            {
                StaThread = GetCurrentThreadId();
                int init = OleInitialize(IntPtr.Zero);
                if (init < 0) Marshal.ThrowExceptionForHR(init);
                initialized = true;
                StaControl = MakeWindow("sta-control");
                UIntPtr timer = SetTimer(IntPtr.Zero, UIntPtr.Zero, 50, TimerProcRoot);
                if (timer == UIntPtr.Zero) throw new Exception("自有 timer 创建失败");
                data = FileData(png);
                var source = new ProbeSource();
                Log("BEFORE_OLE", "api=" + (shell ? "SHDoDragDrop" : "DoDragDrop") + ";owner=" + Handle(Owner) + ";sta=" + StaThread);
                uint effect = 0;
                int hr = shell ? SHDoDragDrop(IntPtr.Zero, data, source, 1, out effect)
                    : DoDragDrop(data, source, 1, out effect);
                Log("AFTER_OLE", "hr=0x" + hr.ToString("X8") + ";effect=" + effect + ";capture=" + Handle(GetCapture()));
                KillTimer(IntPtr.Zero, timer);
                result = new CaseResult { Name = name, Api = shell ? "SHDoDragDrop" : "DoDragDrop", ForcedFirstContinue = forceContinue,
                    OwnerThread = OwnerThread, StaThread = StaThread, Queries = QueryCount, DirectStaTransfers = TransferCount,
                    NullTransfers = NullTransfers, WakeMessages = WakeCount, HResult = "0x" + hr.ToString("X8"), Effect = effect, Finished = true };
            }
            catch (Exception ex) { error = ex; Log("ERROR", ex.ToString()); }
            finally
            {
                if (data != null && Marshal.IsComObject(data)) Marshal.ReleaseComObject(data);
                if (StaControl != IntPtr.Zero) { DestroyWindow(StaControl); StaControl = IntPtr.Zero; }
                if (initialized) OleUninitialize();
                done.Set();
            }
        });
        sta.IsBackground = true;
        sta.SetApartmentState(ApartmentState.STA);
        sta.Start();
        if (!done.WaitOne(2500))
        {
            Interlocked.Exchange(ref Phase, 3);
            Log("CASE_TIMEOUT_CANCEL", "对自有 STA 窗口定向 Esc；外部无消息");
            for (int i = 0; i < 40 && !done.WaitOne(25); ++i)
                EnumThreadWindows(StaThread, delegate(IntPtr h, IntPtr p) { PostMessageW(h, WM_KEYDOWN, (UIntPtr)27, (IntPtr)1); return true; }, IntPtr.Zero);
        }
        if (!done.WaitOne(0)) throw new Exception("OLE 未有界结束，进程 watchdog 兜底");
        sta.Join(); ownerThread.Join();
        if (error != null) throw new Exception("探针失败", error);
        return result;
    }
    [ComVisible(true), ClassInterface(ClassInterfaceType.None)]
    public sealed class ProbeSource : IDropSource
    {
        public int QueryContinueDrag(bool escape, uint keys)
        {
            int n = Interlocked.Increment(ref QueryCount);
            IntPtr capture = GetCapture();
            uint pid;
            uint captureThread = capture == IntPtr.Zero ? 0 : GetWindowThreadProcessId(capture, out pid);
            bool transferred = Volatile.Read(ref Phase) == 2;
            int answer = n == 1 && (ContinueFirst || !transferred) ? 0 : CANCEL;
            Log("QUERY_CONTINUE_DRAG", "n=" + n + ";phase=" + Volatile.Read(ref Phase) + ";capture=" + Handle(capture)
                + ";capture_thread=" + captureThread + ";esc=" + escape + ";keys=" + keys + ";answer=0x" + answer.ToString("X8"));
            return answer;
        }
        public int GiveFeedback(uint effect) { Log("GIVE_FEEDBACK", "effect=" + effect); return 0; }
    }
    static void Tick(IntPtr h, uint message, UIntPtr id, uint time)
    {
        IntPtr capture = GetCapture();
        if (QueryCount <= 1 || Volatile.Read(ref Phase) == 3)
        {
            uint pid;
            uint thread = capture == IntPtr.Zero ? 0 : GetWindowThreadProcessId(capture, out pid);
            // 只向本 STA 的 HWND 发窗口消息，不使用 SendInput 或设置光标。
            IntPtr target = thread == StaThread ? capture : StaControl;
            if (target != IntPtr.Zero)
            {
                WakeCount++;
                Log("TIMER_OWN_ESC", "capture=" + Handle(capture) + ";target=" + Handle(target) + ";thread=" + thread);
                PostMessageW(target, WM_KEYDOWN, (UIntPtr)27, (IntPtr)1);
            }
        }
    }
    static IntPtr WindowProc(IntPtr h, uint message, UIntPtr wp, IntPtr lp)
    {
        if (message == WM_CAPTURECHANGED && h == Owner)
        {
            uint pid = 0;
            uint nextThread = lp == IntPtr.Zero ? 0 : GetWindowThreadProcessId(lp, out pid);
            int before = Volatile.Read(ref Phase);
            if (lp == IntPtr.Zero) NullTransfers++;
            else if (nextThread == StaThread && pid == Process.GetCurrentProcess().Id) TransferCount++;
            if (before == 1 && nextThread == StaThread && nextThread != 0)
                Interlocked.CompareExchange(ref Phase, 2, 1);
            else if (before != 2) Interlocked.Exchange(ref Phase, 3);
            Log("OWNER_CAPTURE_CHANGED", "next=" + Handle(lp) + ";next_thread=" + nextThread + ";pid=" + pid
                + ";before=" + before + ";after=" + Volatile.Read(ref Phase));
        }
        return DefWindowProcW(h, message, wp, lp);
    }
    static IntPtr MakeWindow(string suffix)
    {
        string name = "RustShot.Owned.Ole.Probe." + Process.GetCurrentProcess().Id + "." + suffix + "." + CaseName;
        var c = new WNDCLASSEX { cbSize = (uint)Marshal.SizeOf(typeof(WNDCLASSEX)), lpfnWndProc = WindowProcRoot,
            hInstance = GetModuleHandleW(null), lpszClassName = name };
        if (RegisterClassExW(ref c) == 0) throw new Exception("注册自有窗口失败 " + Marshal.GetLastWin32Error());
        IntPtr h = CreateWindowExW(0x08000080, name, name, 0, -32000, -32000, 1, 1, IntPtr.Zero, IntPtr.Zero, c.hInstance, IntPtr.Zero);
        if (h == IntPtr.Zero) throw new Exception("创建自有隐藏窗口失败 " + Marshal.GetLastWin32Error());
        return h;
    }
    static IDataObject FileData(string path)
    {
        IntPtr full = ILCreateFromPathW(path);
        if (full == IntPtr.Zero) throw new Exception("自有 PNG PIDL 为空");
        IntPtr parent = ILClone(full), array = Marshal.AllocHGlobal(IntPtr.Size);
        try
        {
            if (parent == IntPtr.Zero) throw new Exception("父 PIDL 为空");
            Marshal.WriteIntPtr(array, ILFindLastID(full));
            ILRemoveLastID(parent);
            Guid iid = typeof(IDataObject).GUID;
            IDataObject data;
            int hr = SHCreateDataObject(parent, 1, array, IntPtr.Zero, ref iid, out data);
            if (hr < 0) Marshal.ThrowExceptionForHR(hr);
            return data;
        }
        finally { Marshal.FreeHGlobal(array); if (parent != IntPtr.Zero) ILFree(parent); ILFree(full); }
    }
    [ComVisible(true), Guid("00000121-0000-0000-C000-000000000046"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IDropSource
    {
        [PreserveSig] int QueryContinueDrag([MarshalAs(UnmanagedType.Bool)] bool escape, uint keys);
        [PreserveSig] int GiveFeedback(uint effect);
    }
    delegate IntPtr WndProc(IntPtr h, uint message, UIntPtr wp, IntPtr lp);
    delegate void TimerProc(IntPtr h, uint message, UIntPtr id, uint time);
    delegate bool EnumProc(IntPtr h, IntPtr lp);
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct WNDCLASSEX { public uint cbSize, style; public WndProc lpfnWndProc; public int cbClsExtra, cbWndExtra;
        public IntPtr hInstance, hIcon, hCursor, hbrBackground; public string lpszMenuName, lpszClassName; public IntPtr hIconSm; }
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int x, y; }
    [StructLayout(LayoutKind.Sequential)] struct MSG { public IntPtr hwnd; public uint message; public UIntPtr wParam;
        public IntPtr lParam; public uint time; public POINT pt; public uint lPrivate; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern ushort RegisterClassExW(ref WNDCLASSEX c);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern IntPtr CreateWindowExW(uint ex, string cls, string title, uint style, int x, int y, int w, int h, IntPtr parent, IntPtr menu, IntPtr instance, IntPtr param);
    [DllImport("user32.dll")] static extern IntPtr DefWindowProcW(IntPtr h, uint msg, UIntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] static extern bool DestroyWindow(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr SetCapture(IntPtr h);
    [DllImport("user32.dll")] static extern IntPtr GetCapture();
    [DllImport("user32.dll")] static extern bool ReleaseCapture();
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern bool PeekMessageW(out MSG msg, IntPtr h, uint min, uint max, uint remove);
    [DllImport("user32.dll")] static extern bool TranslateMessage(ref MSG msg);
    [DllImport("user32.dll")] static extern IntPtr DispatchMessageW(ref MSG msg);
    [DllImport("user32.dll")] static extern UIntPtr SetTimer(IntPtr h, UIntPtr id, uint ms, TimerProc callback);
    [DllImport("user32.dll")] static extern bool KillTimer(IntPtr h, UIntPtr id);
    [DllImport("user32.dll")] static extern bool PostMessageW(IntPtr h, uint msg, UIntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] static extern bool EnumThreadWindows(uint thread, EnumProc callback, IntPtr lp);
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern IntPtr GetModuleHandleW(string name);
    [DllImport("ole32.dll")] static extern int OleInitialize(IntPtr reserved);
    [DllImport("ole32.dll")] static extern void OleUninitialize();
    [DllImport("ole32.dll")] static extern int DoDragDrop([MarshalAs(UnmanagedType.Interface)] IDataObject data, [MarshalAs(UnmanagedType.Interface)] IDropSource source, uint effects, out uint effect);
    [DllImport("shell32.dll")] static extern int SHDoDragDrop(IntPtr h, [MarshalAs(UnmanagedType.Interface)] IDataObject data, [MarshalAs(UnmanagedType.Interface)] IDropSource source, uint effects, out uint effect);
    [DllImport("shell32.dll", CharSet = CharSet.Unicode)] static extern IntPtr ILCreateFromPathW(string path);
    [DllImport("shell32.dll")] static extern IntPtr ILClone(IntPtr pidl);
    [DllImport("shell32.dll")] static extern IntPtr ILFindLastID(IntPtr pidl);
    [DllImport("shell32.dll")] static extern bool ILRemoveLastID(IntPtr pidl);
    [DllImport("shell32.dll")] static extern void ILFree(IntPtr pidl);
    [DllImport("shell32.dll")] static extern int SHCreateDataObject(IntPtr parent, uint count, IntPtr children, IntPtr inner, ref Guid iid, [MarshalAs(UnmanagedType.Interface)] out IDataObject data);
}
