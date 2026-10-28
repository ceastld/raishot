#requires -Version 5.1
# 原生 V2 MCP 的本机 HTTP 适配；凭据只从本机配置读取，不写入分发文件。
function Invoke-RustShotNativeMcp {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Tool,
        [hashtable]$Arguments = @{},
        [string]$ConfigPath = (Join-Path $env:USERPROFILE '.quicker/mcp/server.json'),
        [int]$TimeoutSeconds = 30
    )
    $taskConfig = [IO.File]::ReadAllText($ConfigPath) | ConvertFrom-Json
    if (-not $taskConfig.Enabled -or [string]::IsNullOrWhiteSpace($taskConfig.Token)) {
        throw 'Quicker V2 原生 MCP 尚未启用或缺少本机凭据。'
    }
    $taskEndpoint = 'http://127.0.0.1:' + [int]$taskConfig.Port + '/mcp'
    $taskRequest = @{
        jsonrpc = '2.0'
        id = [guid]::NewGuid().ToString('N')
        method = 'tools/call'
        params = @{ name = $Tool; arguments = $Arguments }
    } | ConvertTo-Json -Depth 35 -Compress
    $taskResponse = Invoke-WebRequest -Uri $taskEndpoint -Method Post -UseBasicParsing -Headers @{
        Authorization = 'Bearer ' + $taskConfig.Token
        Accept = 'application/json, text/event-stream'
        'MCP-Protocol-Version' = '2025-03-26'
    } -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($taskRequest)) -TimeoutSec $TimeoutSeconds
    $taskText = [string]$taskResponse.Content
    if ($taskResponse.Headers.'Content-Type' -match 'text/event-stream') {
        $taskText = ($taskText -split "`n" | Where-Object { $_ -match '^data:' } | ForEach-Object { $_.Substring(5).Trim() }) -join "`n"
    }
    $taskRpc = $taskText | ConvertFrom-Json
    if ($taskRpc.error) { throw ('原生 MCP 请求失败：' + ($taskRpc.error | ConvertTo-Json -Compress)) }
    $taskResult = $taskRpc.result
    $taskValue = $taskResult.structuredContent
    if ($null -eq $taskValue) {
        $taskTextBlocks = @($taskResult.content | Where-Object type -eq text | ForEach-Object text)
        $taskJoined = $taskTextBlocks -join [Environment]::NewLine
        try { $taskValue = $taskJoined | ConvertFrom-Json } catch { $taskValue = [pscustomobject]@{ text = $taskJoined } }
    }
    if ($taskResult.isError -or $taskValue.ok -eq $false) {
        throw ('原生 MCP 工具失败：' + ($taskValue | ConvertTo-Json -Depth 12 -Compress))
    }
    return $taskValue
}
