[CmdletBinding()]
param(
    [string]$WorkspaceRoot = $env:QUICKER_WORKSPACE_ROOT,
    [string]$ActionId = '',
    [ValidateSet('region', 'tray')][string]$LaunchMode = 'region',
    [string]$OutputDirectory = (Join-Path $PSScriptRoot 'dist/quicker'),
    [ValidateSet('Auto', 'V2', 'V1')][string]$Backend = 'Auto',
    [string]$SourceDirectory = (Join-Path $PSScriptRoot 'quicker/action'),
    [string]$DevDirectory = '',
    [switch]$UpdateIcon,
    [switch]$VerifyOnly
)
$ErrorActionPreference = 'Stop'
if ($DevDirectory -and $Backend -ne 'V1') { throw '-DevDirectory 仅支持显式 -Backend V1，本机路径不应用到 V2 或账号动作。' }
if ($DevDirectory -and -not $PSBoundParameters.ContainsKey('OutputDirectory')) { $OutputDirectory = Join-Path $PSScriptRoot 'target/quicker-action/dev' }
. (Join-Path $PSScriptRoot 'quicker/ActionSource.ps1')
if (-not [IO.Path]::IsPathRooted($SourceDirectory)) { $SourceDirectory = Join-Path $PSScriptRoot $SourceDirectory }
$taskActionSource = Get-RustShotActionSource -SourceDirectory $SourceDirectory
if (-not [IO.Path]::IsPathRooted($OutputDirectory)) { $OutputDirectory = Join-Path $PSScriptRoot $OutputDirectory }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$taskDevPath = ''
if ($DevDirectory) {
    if (-not (Test-Path -LiteralPath $DevDirectory -PathType Container)) { throw 'DevDirectory 必须是已复制好的开发运行目录。' }
    $taskDevPath = (Resolve-Path -LiteralPath $DevDirectory).Path
    foreach ($taskProfile in @('debug', 'release')) {
        if ($taskDevPath.TrimEnd('\') -ieq (Join-Path $PSScriptRoot "target/$taskProfile")) { throw '请先把三个 EXE 复制到运行副本，不能直接运行 target/debug 或 target/release。' }
    }
    $taskDevOutputRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'target')).TrimEnd('\') + '\'
    if (-not $OutputDirectory.StartsWith($taskDevOutputRoot, [StringComparison]::OrdinalIgnoreCase)) { throw '开发动作的 OutputDirectory 必须位于本项目 target 内，避免本机路径进入分发文件。' }
    foreach ($taskExe in @('raishot.exe', 'raishot-gpui.exe', 'raishot-cli.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $taskDevPath $taskExe) -PathType Leaf)) { throw "开发运行副本缺少 $taskExe。" }
    }
}
$taskMcpConfigPath = Join-Path $env:USERPROFILE '.quicker/mcp/server.json'
$taskUseNative = $Backend -eq 'V2'
if ($Backend -eq 'Auto' -and (Test-Path -LiteralPath $taskMcpConfigPath)) {
    $taskMcpConfig = [IO.File]::ReadAllText($taskMcpConfigPath) | ConvertFrom-Json
    $taskUseNative = [bool]$taskMcpConfig.Enabled
}
if ($taskUseNative) {
    & (Join-Path $PSScriptRoot 'quicker/Install-NativeAction.ps1') -ActionId $ActionId -LaunchMode $LaunchMode -SourceDirectory $SourceDirectory -OutputDirectory $OutputDirectory -UpdateIcon:$UpdateIcon -VerifyOnly:$VerifyOnly
    return
}
if ($VerifyOnly -and -not $taskDevPath) {
    $taskQaPrefix = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'target/qa')).TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
    if (-not $OutputDirectory.StartsWith($taskQaPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'VerifyOnly 必须将 OutputDirectory 指向本项目 target/qa 内，避免测试副本进入正式分发目录。'
    }
}
if (-not [IO.Path]::IsPathRooted($SourceDirectory)) { $SourceDirectory = Join-Path $PSScriptRoot $SourceDirectory }
$taskSource = (Resolve-Path -LiteralPath $SourceDirectory).Path
$taskData = $taskActionSource.Data
$taskColdSteps = $taskData.steps[2].ifSteps
if ($taskColdSteps[0].key -cne 'sys:dependencycheck' -or
    $taskColdSteps[2].inputs.condition -cne '$={launch_args} == "verify"' -or
    $taskColdSteps[3].inputs.condition -cne '$={launch_args} != "verify"') {
    throw '只接受带标准依赖分支与 verify 启动守卫的 RaiShot 动作源。'
}
if ($taskDevPath) {
    $taskColdSteps[0] = [pscustomobject]@{
        key = 'sys:assign'; note = '仅本机开发：直接使用已复制的运行目录，不下载远程依赖。'
        inputs = @{ input = $taskDevPath; stopIfFail = $true }; outputs = @{ output = 'package_path' }
    }
    # 开发副本不要求发布清单；verify 只检查三个入口存在，仍由启动守卫跳过 EXE。
    $taskColdSteps[2].ifSteps[0].inputs = @{
        stopIfFail = $true
        input = @'
$=var root = Path.GetFullPath({package_path});
foreach (var name in new[] { "raishot.exe", "raishot-gpui.exe", "raishot-cli.exe" })
{
    if (!File.Exists(Path.Combine(root, name))) { throw new InvalidOperationException("开发运行副本缺少文件：" + name); }
}
return Path.Combine(root, "raishot.exe");
'@
    }
}
if ([string]::IsNullOrWhiteSpace($WorkspaceRoot)) { throw 'V1 适配需指定 -WorkspaceRoot 或 QUICKER_WORKSPACE_ROOT。' }
$taskWorkspace = (Resolve-Path -LiteralPath $WorkspaceRoot).Path
$taskQkrpc = (Get-Command qkrpc -ErrorAction Stop).Source
function Invoke-RustShotQkrpc([string[]]$Arguments) {
    $taskResponse = & $taskQkrpc @Arguments
    if ($LASTEXITCODE -ne 0) { throw "qkrpc 执行失败：$($taskResponse -join [Environment]::NewLine)" }
    $taskResult = ($taskResponse -join [Environment]::NewLine) | ConvertFrom-Json
    if (-not $taskResult.ok) { throw "qkrpc 请求失败：$($taskResponse -join [Environment]::NewLine)" }
    return $taskResult
}
Push-Location -LiteralPath $taskWorkspace
try {
    $taskNewAction = [string]::IsNullOrWhiteSpace($ActionId)
    if ($ActionId) {
        $taskParsedId = [guid]::Empty
        if (-not [guid]::TryParse($ActionId, [ref]$taskParsedId)) { throw 'ActionId 必须是 GUID。' }
        $taskStage = Join-Path $taskWorkspace ('.tmp/rustshot-action/export-' + [guid]::NewGuid().ToString('N'))
        $null = Invoke-RustShotQkrpc -Arguments @('action', 'export', '--id', $ActionId, '--dir', $taskStage, '--json')
        $taskInfo = Get-Content -LiteralPath (Join-Path $taskStage 'info.json') -Encoding UTF8 -Raw | ConvertFrom-Json
    } else {
        $taskCreated = Invoke-RustShotQkrpc -Arguments @('action', 'create', '--title', $taskActionSource.Info.title, '--description', $taskActionSource.Info.description, '--icon', $taskActionSource.Info.icon, '--json')
        $ActionId = $taskCreated.actionId
        $taskStage = Join-Path $taskWorkspace ('.tmp/rustshot-action/' + $ActionId)
        [IO.Directory]::CreateDirectory($taskStage) | Out-Null
        $taskInfo = [pscustomobject]@{ Id = $ActionId; Title = $taskActionSource.Info.title; EditVersion = $taskCreated.editVersion }
        [IO.File]::WriteAllText((Join-Path $taskStage 'info.json'), ($taskInfo | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
    }
    ($taskData.variables | Where-Object key -eq 'launch_mode').default = $LaunchMode
    Set-RustShotActionPresentation -Target $taskInfo -SourceInfo $taskActionSource.Info -NewAction:$taskNewAction -UpdateIcon:$UpdateIcon
    $taskInfo | Add-Member -NotePropertyName contextMenuData -NotePropertyValue $taskActionSource.Info.contextMenuData -Force
    [IO.File]::WriteAllText((Join-Path $taskStage 'info.json'), ($taskInfo | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $taskStage 'data.json'), ($taskData | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
    [IO.Directory]::CreateDirectory((Join-Path $taskStage 'files')) | Out-Null
    Copy-Item -LiteralPath (Join-Path $taskSource 'files/validate-package.value.txt') -Destination (Join-Path $taskStage 'files/validate-package.value.txt')
    Copy-Item -LiteralPath (Join-Path $taskSource 'files/resolve-command.value.txt') -Destination (Join-Path $taskStage 'files/resolve-command.value.txt')
    $null = Invoke-RustShotQkrpc -Arguments @('action', 'validate', '--dir', $taskStage, '--json')
    $taskSaved = Invoke-RustShotQkrpc -Arguments @('action', 'apply', '--dir', $taskStage, '--expected-edit-version', ([string]$taskInfo.EditVersion), '--json')
    # apply 只接管程序正文；外观与右键菜单须通过独立元数据接口写回。
    $taskMetadataArguments = @('action', 'set-metadata', '--id', $ActionId,
        '--title', $taskActionSource.Info.title, '--description', $taskActionSource.Info.description,
        '--context-menu-data', $taskActionSource.Info.contextMenuData,
        '--expected-edit-version', ([string]$taskSaved.payload.editVersion), '--json')
    if ($taskNewAction -or $UpdateIcon) { $taskMetadataArguments += @('--icon', $taskActionSource.Info.icon) }
    $taskMetadataSaved = Invoke-RustShotQkrpc -Arguments $taskMetadataArguments
    $taskSaved.payload.editVersion = $taskMetadataSaved.payload.editVersion
    # 获取本次动作的原生完整正文；Agent wire 的 data.json 不能作为普通导入文件。
    $taskExportCode = @'
var type = AppDomain.CurrentDomain.GetAssemblies()
    .Select(a => a.GetType("QuickerRpc.Plugin.Services.DataServiceActionAccess", false))
    .FirstOrDefault(t => t != null);
if (type == null) { throw new InvalidOperationException("QuickerRpc 尚未加载动作访问服务。"); }
var args = new object[] { "__ACTION_ID__", null };
var method = type.GetMethod("TryFindActionWithPayload", System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static);
if (method == null || (bool)method.Invoke(null, args) == false)
{
    throw new InvalidOperationException("找不到本次动作的完整正文，不能导出导入文件。");
}
return args[1];
'@
    $taskExportCode = $taskExportCode.Replace('__ACTION_ID__', ([guid]$ActionId).ToString())
    $taskExportCodePath = Join-Path $taskStage 'export-native.eval.cs'
    [IO.File]::WriteAllText($taskExportCodePath, $taskExportCode, [Text.UTF8Encoding]::new($false))
    $taskNativeResult = Invoke-RustShotQkrpc -Arguments @('expr', 'run', '--file', $taskExportCodePath, '--on-ui-thread', '--json')
    if (-not $taskNativeResult.success -or -not $taskNativeResult.resultJson) { throw '导出原生动作正文失败。' }
    $taskNative = $taskNativeResult.resultJson | ConvertFrom-Json
    if ($taskNative.Id -ne $ActionId -or -not $taskNative.Data) { throw '导出动作标识或正文不符合本次动作。' }
    $taskOutput = [IO.Path]::GetFullPath($OutputDirectory)
    [IO.Directory]::CreateDirectory($taskOutput) | Out-Null
    $taskV1Path = Join-Path $taskOutput 'RaiShot.v1.json'
    $taskQkaPath = Join-Path $taskOutput 'RaiShot.qka'
    [IO.File]::WriteAllText($taskV1Path, $taskNativeResult.resultJson, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText($taskQkaPath, $taskNative.Data, [Text.UTF8Encoding]::new($false))
    $taskVerifyRun = $null
    if ($VerifyOnly) {
        $taskVerifyId = [guid]::Parse($ActionId).ToString()
        $taskStart = New-Object Diagnostics.ProcessStartInfo
        $taskStart.FileName = $taskQkrpc
        $taskStart.Arguments = 'action run --id ' + $taskVerifyId + ' --param verify --trace --json --no-bootstrap'
        $taskStart.WorkingDirectory = $taskWorkspace
        $taskStart.UseShellExecute = $false
        $taskStart.CreateNoWindow = $true
        $taskStart.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
        $taskStart.RedirectStandardOutput = $true
        $taskStart.RedirectStandardError = $true
        $taskStart.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
        $taskStart.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
        $taskProcess = [Diagnostics.Process]::Start($taskStart)
        try {
            $taskStdoutRead = $taskProcess.StandardOutput.ReadToEndAsync()
            $taskStderrRead = $taskProcess.StandardError.ReadToEndAsync()
            if (-not $taskProcess.WaitForExit(60000)) { throw 'V1 verify 超时，保留运行现场，不重复运行动作。' }
            $taskStdout = $taskStdoutRead.GetAwaiter().GetResult()
            $taskStderr = $taskStderrRead.GetAwaiter().GetResult()
            [IO.File]::WriteAllText((Join-Path $taskOutput 'verify.stdout.json'), $taskStdout, [Text.UTF8Encoding]::new($false))
            [IO.File]::WriteAllText((Join-Path $taskOutput 'verify.stderr.jsonl'), $taskStderr, [Text.UTF8Encoding]::new($false))
            if ($taskProcess.ExitCode -ne 0) { throw 'V1 verify CLI 返回失败，原始日志已保留。' }
            $taskTrace = $taskStdout | ConvertFrom-Json
            $taskSteps = @($taskTrace.events | Where-Object kind -eq step_begin | ForEach-Object stepRunnerKey)
            $taskFalse = @($taskTrace.events | Where-Object { $_.stepRunnerKey -eq 'sys:simpleIf' -and $_.paramKey -eq 'condition' -and $_.paramValue -eq 'False' })
            $taskExpectedVerifySteps = @('sys:assign', 'sys:simpleIf', 'sys:simpleIf', $taskColdSteps[0].key, 'sys:assign', 'sys:simpleIf', 'sys:assign', 'sys:simpleIf', 'sys:simpleIf') -join ','
            $taskForbiddenSteps = @($taskSteps | Where-Object { $_ -in @('sys:getWindowTitle', 'sys:sendMessage', 'sys:run', 'sys:csscript') })
            if (-not $taskTrace.ok -or $taskTrace.actionId -ne $ActionId -or $taskTrace.events[-1].kind -ne 'file_end' -or
                ($taskSteps -join ',') -cne $taskExpectedVerifySteps -or $taskFalse.Count -ne 3 -or $taskForbiddenSteps.Count -ne 0) {
                throw 'V1 verify 没有完成标准校验/跳过 EXE，检查原始 trace。'
            }
            $taskVerifyRun = [pscustomobject]@{ ok = $true; exit_code = $taskProcess.ExitCode; output_drained = $true; exe_launch_skipped = $true; event_count = $taskTrace.eventCount; stdout = (Join-Path $taskOutput 'verify.stdout.json'); stderr = (Join-Path $taskOutput 'verify.stderr.jsonl') }
        } finally { $taskProcess.Dispose() }
    }
    $taskReceipt = [pscustomobject]@{ action_id = $ActionId; edit_version = $taskSaved.payload.editVersion; launch_mode = $LaunchMode; dev_directory = $taskDevPath; staging_directory = $taskStage; import_file = $taskV1Path; designer_file = $taskQkaPath; dependency_published = $false; verify_only = [bool]$VerifyOnly; verify_result = $taskVerifyRun }
    [IO.File]::WriteAllText((Join-Path $taskOutput 'RaiShot.v1-local.json'), ($taskReceipt | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    $taskReceipt | ConvertTo-Json -Depth 20
} finally {
    Pop-Location
}
