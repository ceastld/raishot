param(
    [string]$Png = (Join-Path $PSScriptRoot '..\..\target\qa\pin-export-details\owned-ole-probe.png'),
    [string]$Output = (Join-Path $PSScriptRoot '..\..\target\qa\owned-ole-capture-order')
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Path $Output -Force | Out-Null
Add-Type -Path (Join-Path $PSScriptRoot 'owned-ole-capture-order.cs')
$probeResult = [OwnedOleCaptureOrder]::Run((Resolve-Path -LiteralPath $Png).Path, (Join-Path $Output 'events.tsv'))
$probeResult | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $Output 'report.json') -Encoding utf8
$probeResult.Cases | Format-Table Name, Queries, DirectStaTransfers, NullTransfers, WakeMessages, HResult, Effect, Finished -AutoSize
