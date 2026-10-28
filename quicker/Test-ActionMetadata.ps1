#requires -Version 7.0
<#
.SYNOPSIS
离线验证动作更新保留用户图标，不访问 Quicker、账号或网络。
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'ActionSource.ps1')
$taskSource = Get-RustShotActionSource
$taskPassed = 0

foreach ($taskIcon in @('fa:Solid_Rabbit:#123456', ('data:image/png;base64,' + ('A' * 12000)), '')) {
    foreach ($taskKind in @('V1', 'V2')) {
        $taskTarget = [pscustomobject]@{ title = 'old'; description = 'old'; icon = $taskIcon }
        Set-RustShotActionPresentation -Target $taskTarget -SourceInfo $taskSource.Info
        if ($taskTarget.icon -cne $taskIcon -or $taskTarget.title -cne $taskSource.Info.title) {
            throw "$taskKind 更新没有同时保留图标、更新名称。"
        }
        Set-RustShotActionPresentation -Target $taskTarget -SourceInfo $taskSource.Info -UpdateIcon
        if ($taskTarget.icon -cne $taskSource.Info.icon) { throw "$taskKind 的 UpdateIcon 未采用源图标。" }
        $taskPassed += 2
    }
}
$taskNew = [pscustomobject]@{}
Set-RustShotActionPresentation -Target $taskNew -SourceInfo $taskSource.Info -NewAction
if ($taskNew.icon -cne $taskSource.Info.icon) { throw '新建动作没有采用源图标。' }
$taskPassed++

function Invoke-TestMetadataEdits([string]$ActualYaml, [string]$ReadView) {
    $taskResult = $ActualYaml
    foreach ($taskEdit in @(Get-RustShotActionInfoEdits -ExistingYaml $ReadView -SourceYaml $taskSource.InfoYaml)) {
        $taskLines = $taskResult -split '\r?\n'
        $taskOffset = ($taskLines | Select-Object -First ($taskEdit.lineStart - 1) | ForEach-Object { $_.Length + 1 } | Measure-Object -Sum).Sum
        if ($null -eq $taskOffset) { $taskOffset = 0 }
        $taskResult = $taskResult.Replace("`r`n", "`n")
        if ($taskResult.Substring($taskOffset, $taskEdit.oldText.Length) -cne $taskEdit.oldText) {
            throw '元数据编辑未精确匹配原行，可能引用了截断的图标。'
        }
        $taskResult = $taskResult.Remove($taskOffset, $taskEdit.oldText.Length).Insert($taskOffset, $taskEdit.newText)
    }
    return $taskResult
}

$taskIconBlocks = @(
    "icon: 'fa:Solid_Rabbit:#123456'",
    ("icon: 'data:image/png;base64," + ('A' * 12000) + "'"),
    ("icon: |-`n" + ((1..260 | ForEach-Object { '  ' + ('B' * 76) }) -join "`n")),
    "icon: >-`n  data:image/png;base64,`n  ABCDEFGH"
)
foreach ($taskBlock in $taskIconBlocks) {
    $taskExisting = "title: 'old'`ndescription: 'old'`n$taskBlock`ncontextMenu: |-`n  old menu`nextra: keep`n"
    # MCP 会截断超长单行；只能在原字段上进行精确窄编辑，不能回写整个读取结果。
    $taskView = ($taskExisting -split "`n" | ForEach-Object { if ($_.Length -gt 2000) { $_.Substring(0, 2000) + '…' } else { $_ } }) -join "`n"
    $taskResult = Invoke-TestMetadataEdits -ActualYaml $taskExisting -ReadView $taskView
    if (-not $taskResult.Contains($taskBlock + "`n") -or -not $taskResult.Contains('extra: keep') -or
        -not $taskResult.Contains("title: '$($taskSource.Info.title)'")) {
        throw '原生 V2 更新改动了完整图标段，或没有更新其它元数据。'
    }
    if (@(Get-RustShotActionInfoEdits -ExistingYaml $taskResult -SourceYaml $taskSource.InfoYaml).Count -ne 0) {
        throw '元数据更新结果不稳定。'
    }
    $taskPassed++
}
Write-Host "图标保留离线测试通过：$taskPassed 项。"
