#requires -Version 7.0
<#
.SYNOPSIS
预览或更新已有 V2 账号动作；Quicker 下次同步拉取。不会公开分享动作。
.PARAMETER MetadataOnly
仅更新外观和右键菜单，保留现有依赖版本、变量和执行步骤。
.PARAMETER UpdateIcon
显式采用动作源图标；默认保留账号动作的现有图标。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][guid]$ActionId,
    [string]$SourceDirectory = (Join-Path $PSScriptRoot 'action'),
    [string]$OutputDirectory = (Join-Path (Split-Path $PSScriptRoot -Parent) 'target/quicker-action'),
    [string]$QuickerCli = '',
    [switch]$MetadataOnly,
    [switch]$UpdateIcon,
    [switch]$Apply
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'ActionSource.ps1')
if (-not [IO.Path]::IsPathRooted($SourceDirectory)) { $SourceDirectory = Join-Path (Split-Path $PSScriptRoot -Parent) $SourceDirectory }
$taskSource = Get-RustShotActionSource -SourceDirectory $SourceDirectory
$taskCli = Resolve-RustShotQuickerCli -QuickerCli $QuickerCli
$taskOutput = [IO.Path]::GetFullPath($OutputDirectory)
[void][IO.Directory]::CreateDirectory($taskOutput)
$taskOriginal = Join-Path $taskOutput 'account-original.json'
$taskEdited = Join-Path $taskOutput 'account-update.json'
if ((Test-Path -LiteralPath $taskOriginal) -or (Test-Path -LiteralPath $taskEdited)) { throw '输出目录已有账号导出；请指定新目录，避免覆盖待审阅或已应用的版本。' }
& $taskCli action catalog-export --id $ActionId.ToString() --out $taskOriginal
if ($LASTEXITCODE -ne 0) { throw '导出账号动作失败。' }
$taskExport = [IO.File]::ReadAllText($taskOriginal) | ConvertFrom-Json
$taskBody = $taskExport.itemData
$taskPayload = $taskBody.operationPayload
if ($taskExport.itemId -ne $ActionId.ToString() -or $taskBody.operationType -ne 'xAction' -or
    @($taskPayload.variables | Where-Object { $_.key -eq 'package_name' -and $_.defaultValue -eq 'cea.rustshot' }).Count -ne 1) {
    throw '目标不是已有 RaiShot 复合动作，停止写入。'
}
# 新依赖尚未发布时可先同步外观；执行正文留到依赖可下载后整体应用。
if (-not $MetadataOnly) {
    # 只接管动作声明的变量和步骤，保留身份、同步修订、保护标记及其它用户设置。
    foreach ($taskVariable in $taskSource.Data.variables) {
        $taskExisting = @($taskPayload.variables | Where-Object key -eq $taskVariable.key)
        if ($taskExisting.Count -gt 1) { throw "账号动作变量重复：$($taskVariable.key)" }
        if ($taskExisting.Count -eq 0) {
            $taskTarget = [pscustomobject]@{ key = $taskVariable.key; type = 0; saveState = $false; isInput = $false; isOutput = $false }
            $taskPayload.variables += $taskTarget
        } else { $taskTarget = $taskExisting[0] }
        if ($taskVariable.key -ne 'launch_mode' -or $taskExisting.Count -eq 0) {
            $taskTarget | Add-Member -NotePropertyName defaultValue -NotePropertyValue $taskVariable.default -Force
        }
        $taskTarget | Add-Member -NotePropertyName isLocked -NotePropertyValue ([bool]$taskVariable.isLocked) -Force
        if ($taskVariable.desc) { $taskTarget | Add-Member -NotePropertyName desc -NotePropertyValue $taskVariable.desc -Force }
    }
    $taskPayload.steps = @(ConvertTo-RustShotNativeSteps -Steps $taskSource.Data.steps -SourceRoot $taskSource.Root)
}
Set-RustShotActionPresentation -Target $taskBody.presentation -SourceInfo $taskSource.Info -UpdateIcon:$UpdateIcon
$taskBody.options.contextMenuData = $taskSource.Info.contextMenuData
[IO.File]::WriteAllText($taskEdited, ($taskExport | ConvertTo-Json -Depth 70), [Text.UTF8Encoding]::new($false))
& $taskCli action catalog-apply --file $taskEdited --dry-run
if ($LASTEXITCODE -ne 0) { throw '账号动作预检失败，未应用。' }
if ($Apply) {
    & $taskCli action catalog-apply --file $taskEdited
    if ($LASTEXITCODE -ne 0) { throw '账号动作应用失败或结果未知；保留导出文件，先重新读取账号状态，不自动重试。' }
}
