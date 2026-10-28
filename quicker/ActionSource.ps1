#requires -Version 5.1
# 动作源的公共读取入口；维护工具使用，目标电脑不依赖此脚本。
function Get-RustShotActionSource {
    param([string]$SourceDirectory = (Join-Path $PSScriptRoot 'action'))
    $taskRoot = (Resolve-Path -LiteralPath $SourceDirectory -ErrorAction Stop).Path
    $taskData = [IO.File]::ReadAllText((Join-Path $taskRoot 'data.json')) | ConvertFrom-Json
    $taskYaml = [IO.File]::ReadAllText((Join-Path $taskRoot 'program.source.yaml'))
    foreach ($taskKey in @('package_name', 'package_version')) {
        $taskValues = @($taskData.variables | Where-Object key -eq $taskKey)
        if ($taskValues.Count -ne 1) { throw "动作变量不唯一：$taskKey" }
        $taskMatch = [regex]::Match($taskYaml, ('(?m)^  - key: ' + $taskKey + '\r?\n    varType: text\r?\n    default: (?<value>[^\r\n]+)'))
        if (-not $taskMatch.Success -or $taskMatch.Groups['value'].Value.Trim("'", '"') -cne $taskValues[0].default) {
            throw "V1/V2 动作的 $taskKey 不一致，请先同步动作源。"
        }
    }
    $taskInfoYaml = [IO.File]::ReadAllText((Join-Path $taskRoot 'info.yaml'))
    $taskInfo = @{}
    foreach ($taskKey in @('title', 'description', 'icon')) {
        $taskMatch = [regex]::Match($taskInfoYaml, ('(?m)^' + $taskKey + ": '([^\r\n]*)'\r?$"))
        if (-not $taskMatch.Success) { throw "info.yaml 缺少单行 $taskKey" }
        $taskInfo[$taskKey] = $taskMatch.Groups[1].Value.Replace("''", "'")
    }
    $taskMenu = [regex]::Match($taskInfoYaml, '(?m)^contextMenu: \|-\r?\n(?<lines>(?:  [^\r\n]*(?:\r?\n|$))+)')
    $taskInfo.contextMenuData = if ($taskMenu.Success) { (($taskMenu.Groups['lines'].Value.TrimEnd("`r", "`n") -split '\r?\n' | ForEach-Object { $_.Substring(2) }) -join "`r`n") } else { '' }
    [pscustomobject]@{ Root = $taskRoot; Data = $taskData; ProgramYaml = $taskYaml; InfoYaml = $taskInfoYaml; Info = $taskInfo }
}

function Set-RustShotActionPresentation {
    param([object]$Target, [hashtable]$SourceInfo, [switch]$NewAction, [switch]$UpdateIcon)
    foreach ($taskKey in @('title', 'description')) {
        $Target | Add-Member -NotePropertyName $taskKey -NotePropertyValue $SourceInfo[$taskKey] -Force
    }
    # 用户可单独修改图标；更新程序正文不代表要重置它。
    if ($NewAction -or $UpdateIcon) {
        $Target | Add-Member -NotePropertyName icon -NotePropertyValue $SourceInfo.icon -Force
    }
}

function Get-RustShotActionInfoEdits {
    param([string]$ExistingYaml, [string]$SourceYaml)
    # 只编辑非图标字段，既不解析也不重写 icon 的长 data URL 或多行 YAML。
    $taskPattern = '(?m)^(?<key>[A-Za-z_][A-Za-z0-9_-]*):[^\r\n]*(?:\r?\n|$)'
    $taskOldHeaders = [regex]::Matches($ExistingYaml, $taskPattern)
    $taskNewHeaders = [regex]::Matches($SourceYaml, $taskPattern)
    $taskEdits = @()
    foreach ($taskKey in @('title', 'description', 'contextMenu')) {
        $taskOld = @($taskOldHeaders | Where-Object { $_.Groups['key'].Value -ceq $taskKey })
        $taskNew = @($taskNewHeaders | Where-Object { $_.Groups['key'].Value -ceq $taskKey })
        if ($taskOld.Count -ne 1 -or $taskNew.Count -ne 1) { throw "info.yaml 的 $taskKey 不唯一或缺失，停止更新元数据。" }
        $taskOldEnd = @($taskOldHeaders | Where-Object Index -gt $taskOld[0].Index | Select-Object -First 1)
        $taskNewEnd = @($taskNewHeaders | Where-Object Index -gt $taskNew[0].Index | Select-Object -First 1)
        $taskOldLength = $(if ($taskOldEnd.Count) { $taskOldEnd[0].Index } else { $ExistingYaml.Length }) - $taskOld[0].Index
        $taskNewLength = $(if ($taskNewEnd.Count) { $taskNewEnd[0].Index } else { $SourceYaml.Length }) - $taskNew[0].Index
        $taskOldText = $ExistingYaml.Substring($taskOld[0].Index, $taskOldLength).TrimEnd("`r", "`n")
        $taskNewText = $SourceYaml.Substring($taskNew[0].Index, $taskNewLength).TrimEnd("`r", "`n")
        if ($taskOldText -cne $taskNewText) {
            $taskEdits += [pscustomobject]@{
                lineStart = 1 + [regex]::Matches($ExistingYaml.Substring(0, $taskOld[0].Index), '\n').Count
                oldText = $taskOldText
                newText = $taskNewText
            }
        }
    }
    # 从后往前更新，前面的行号不会受多行右键菜单长度变化影响。
    $taskEdits | Sort-Object lineStart -Descending
}

function ConvertTo-RustShotNativeSteps {
    param([object[]]$Steps, [string]$SourceRoot)
    foreach ($taskStep in $Steps) {
        $taskInputs = [ordered]@{}
        foreach ($taskInput in $taskStep.inputs.PSObject.Properties) {
            $taskParts = $taskInput.Name -split '\.', 2
            $taskValue = $taskInput.Value
            $taskParam = if ($taskParts.Count -eq 2 -and $taskParts[1] -eq 'var') {
                @{ varKey = [string]$taskValue }
            } elseif ($taskParts.Count -eq 2 -and $taskParts[1] -eq 'file') {
                $taskPath = [IO.Path]::GetFullPath((Join-Path $SourceRoot $taskValue))
                if (-not $taskPath.StartsWith($SourceRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw '动作资源路径越界。' }
                @{ value = [IO.File]::ReadAllText($taskPath) }
            } else {
                @{ value = if ($taskValue -is [bool]) { $taskValue.ToString().ToLowerInvariant() } else { [string]$taskValue } }
            }
            if ($taskInputs.Contains($taskParts[0])) { throw "步骤输入重复：$($taskParts[0])" }
            $taskInputs[$taskParts[0]] = $taskParam
        }
        $taskNative = [ordered]@{ stepRunnerKey = $taskStep.key; inputParams = $taskInputs; outputParams = @{} }
        if ($taskStep.outputs) { $taskNative.outputParams = $taskStep.outputs }
        if ($taskStep.note) { $taskNative.note = $taskStep.note }
        foreach ($taskBranch in @('ifSteps', 'elseSteps')) {
            if ($taskStep.$taskBranch) { $taskNative[$taskBranch] = @(ConvertTo-RustShotNativeSteps -Steps $taskStep.$taskBranch -SourceRoot $SourceRoot) }
        }
        [pscustomobject]$taskNative
    }
}

function Resolve-RustShotQuickerCli {
    param([string]$QuickerCli = '')
    if ($QuickerCli) { return (Get-Command $QuickerCli -ErrorAction Stop).Source }
    $taskCommand = Get-Command quicker -ErrorAction SilentlyContinue
    if ($taskCommand) { return $taskCommand.Source }
    $taskCandidate = Join-Path $env:LOCALAPPDATA 'quicker-cli/bin/quicker.exe'
    if (Test-Path -LiteralPath $taskCandidate -PathType Leaf) { return $taskCandidate }
    throw '找不到独立 quicker CLI 0.3.1+，请安装或指定 -QuickerCli。'
}
