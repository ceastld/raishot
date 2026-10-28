param(
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug','release')][string]$Configuration = 'debug',
    [ValidateSet('all','remaining')][string]$Phase = 'all',
    [switch]$WaitForActivation,
    [switch]$OwnedBackground,
    [switch]$GpuFramebuffer
)
$ErrorActionPreference = 'Stop'
if ($GpuFramebuffer -and ($OwnedBackground -or $WaitForActivation)) { throw 'GPU 帧缓冲检查请单独使用 -GpuFramebuffer。' }
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExe = Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe"
if (-not (Test-Path -LiteralPath $taskExe)) { throw "验收工作进程不存在：$taskExe" }
if (Get-Process -Name 'raishot-gpui' -ErrorAction SilentlyContinue) { throw '已有 GPUI 会话，停止验收；不会结束用户程序。' }
$taskDir = Join-Path $taskRoot ('target/qa/highlighter-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskDir)
$taskHash = (Get-FileHash -LiteralPath $taskExe).Hash
$taskVariables = @('RUSTSHOT_DATA_DIR','RUSTSHOT_SMOKE_CASE','RUSTSHOT_HIGHLIGHTER_QA_DIRECTORY','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_HIGHLIGHTER_QA_PHASE','RUSTSHOT_HIGHLIGHTER_QA_WAIT_ACTIVATION','RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND','RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER')
$taskOldValues = @{}
foreach ($taskVariable in $taskVariables) { $taskOldValues[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable,'Process') }
$taskProcess = $null
try {
    $env:RUSTSHOT_DATA_DIR = Join-Path $taskDir 'settings'
    $env:RUSTSHOT_SMOKE_CASE = 'highlighter'
    $env:RUSTSHOT_HIGHLIGHTER_QA_DIRECTORY = $taskDir
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $env:RUSTSHOT_HIGHLIGHTER_QA_PHASE = $Phase
    if ($WaitForActivation) { $env:RUSTSHOT_HIGHLIGHTER_QA_WAIT_ACTIVATION = '1' } else { Remove-Item Env:RUSTSHOT_HIGHLIGHTER_QA_WAIT_ACTIVATION -ErrorAction SilentlyContinue }
    if ($OwnedBackground) { $env:RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND = '1' } else { Remove-Item Env:RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND -ErrorAction SilentlyContinue }
    if ($GpuFramebuffer) { $env:RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER = '1' } else { Remove-Item Env:RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER -ErrorAction SilentlyContinue }
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskDir 'stdout.log') -RedirectStandardError (Join-Path $taskDir 'stderr.log')
    $taskDeadline = (Get-Date).AddSeconds($(if ($WaitForActivation) { 105 } else { 45 }))
    while (-not $taskProcess.WaitForExit(500)) {
        if ((Get-Date) -ge $taskDeadline) { throw "自有高亮验收超时：PID $($taskProcess.Id)，记录 $taskDir" }
    }
    $taskProcess.Refresh()
    $taskSummaryPath = Join-Path $taskDir 'summary.json'
    if ($taskProcess.ExitCode -ne 0) {
        Get-Content -LiteralPath (Join-Path $taskDir 'stderr.log')
        if (Test-Path -LiteralPath $taskSummaryPath) { Get-Content -LiteralPath $taskSummaryPath }
        throw "高亮原生验收失败：$taskDir"
    }
    $taskSummary = Get-Content -LiteralPath $taskSummaryPath -Raw | ConvertFrom-Json
    if ($taskSummary.ok -ne $true) { throw "高亮验收报告未通过：$taskDir" }
    if ($taskSummary.foreground_required -ne (-not ([bool]$OwnedBackground -or [bool]$GpuFramebuffer))) { throw '原生验收前台模式与请求不匹配。' }
    if ($GpuFramebuffer -and ($taskSummary.pixel_backend -ne 'gpu_framebuffer' -or $taskSummary.roi_ownership -ne 'owned_gpu_texture_crop')) { throw 'GPU 验收缺少自有帧缓冲范围检查。' }
    if ($OwnedBackground -and ($taskSummary.roi_ownership -ne 'every_physical_pixel_before_and_after' -or $taskSummary.reject_visible_windows_above_roi -ne $true)) { throw '后台原生验收缺少完整窗口归属检查。' }
    $taskExpected = @('native_highlighter_line_source_multiply_overlap_alpha','native_highlighter_selected_modes_shape_undo_redo','native_highlighter_freehand_union_gpu_png_above_stable','native_highlighter_zoomed_source_brush_tile_seam_bilinear')
    if ($Phase -eq 'remaining') { $taskExpected = $taskExpected[2..3] }
    $taskReports = @(Get-Content -LiteralPath (Join-Path $taskDir 'stdout.log') | ForEach-Object {
        try { $taskLine = $_ | ConvertFrom-Json; if ($taskLine.test -and $taskLine.ok -eq $true) { $taskLine.test } } catch { }
    })
    if ($taskReports.Count -ne $taskExpected.Count -or (Compare-Object ($taskExpected | Sort-Object) ($taskReports | Sort-Object))) { throw "高亮验收报告数量/名称错误：$($taskReports -join ', ')" }
    if ((Get-FileHash -LiteralPath $taskExe).Hash -ne $taskHash) { throw '验收期间可执行文件被替换，结果无效。' }
    [pscustomobject]@{path=$taskExe;sha256=$taskHash;configuration=$Configuration;report=$taskSummaryPath;pid=$taskProcess.Id;exitCode=$taskProcess.ExitCode;ok=$true} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskDir 'binary.json') -Encoding utf8
    $taskSummary | ConvertTo-Json -Compress -Depth 10
    Write-Output $taskDir
} finally {
    if ($taskProcess -and -not $taskProcess.HasExited) {
        # 只关闭本脚本创建的验收进程，不操作已有截图或贴图。
        [void]$taskProcess.CloseMainWindow()
        if (-not $taskProcess.WaitForExit(5000)) { Write-Warning "自有验收进程 $($taskProcess.Id) 尚未退出；记录 $taskDir" }
    }
    foreach ($taskVariable in $taskVariables) { [Environment]::SetEnvironmentVariable($taskVariable,$taskOldValues[$taskVariable],'Process') }
}
