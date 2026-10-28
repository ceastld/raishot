[CmdletBinding()]
param(
    [ValidateRange(1, 100)][int]$Count = 5,
    [string]$Region = '0,0,800,600'
)

$ErrorActionPreference = 'Stop'
Push-Location -LiteralPath $PSScriptRoot
try {
    $taskCli = Join-Path $PSScriptRoot 'target\release\raishot-cli.exe'
    if (-not (Test-Path -LiteralPath $taskCli)) { throw '请先运行 build.ps1。' }
    $taskQa = Join-Path $PSScriptRoot 'target\qa'
    New-Item -ItemType Directory -Path $taskQa -Force | Out-Null
    $taskLog = Join-Path $taskQa 'isolated-benchmark.jsonl'
    $taskImage = Join-Path $taskQa 'isolated-benchmark.png'
    $taskResults = @()
    $taskJsonLines = @()
    for ($taskIndex = 1; $taskIndex -le $Count; $taskIndex++) {
        $taskWatch = [System.Diagnostics.Stopwatch]::StartNew()
        $taskLines = & $taskCli --benchmark 1 --region $Region --output $taskImage
        if ($LASTEXITCODE -ne 0) { throw "第 $taskIndex 次截图失败" }
        $taskWatch.Stop()
        $taskRow = $taskLines | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.PSObject.Properties.Name -contains 'iteration' } | Select-Object -First 1
        if ($null -eq $taskRow) { throw '截图进程未返回测量结果' }
        $taskRow.iteration = $taskIndex
        $taskRow | Add-Member -NotePropertyName process_wall_ms -NotePropertyValue ([math]::Round($taskWatch.Elapsed.TotalMilliseconds, 3))
        $taskResults += $taskRow
        $taskJson = $taskRow | ConvertTo-Json -Compress
        $taskJsonLines += $taskJson
        Write-Output $taskJson
    }
    $taskSorted = @($taskResults.process_wall_ms | Sort-Object)
    $taskSummary = [pscustomobject]@{
        summary = $true
        count = $Count
        process_wall_mean_ms = [math]::Round(($taskSorted | Measure-Object -Average).Average, 3)
        process_wall_median_ms = $taskSorted[[int][math]::Floor($Count / 2)]
        process_wall_p95_ms = $taskSorted[[int][math]::Ceiling($Count * 0.95) - 1]
    } | ConvertTo-Json -Compress
    $taskJsonLines += $taskSummary
    $taskJsonLines | Set-Content -LiteralPath $taskLog -Encoding utf8
    Write-Output $taskSummary
    Write-Output "测量记录：$taskLog"
} finally {
    Pop-Location
}
