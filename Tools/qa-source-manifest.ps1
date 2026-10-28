function Get-RustShotQaSourceManifest {
    param([Parameter(Mandatory)][string]$Root)
    $taskFiles = @(foreach ($taskDirectory in @('src', 'assets', 'dependencies', 'vendor', 'Tools', 'crates')) {
        $taskDirectoryPath = Join-Path $Root $taskDirectory
        if (Test-Path -LiteralPath $taskDirectoryPath) {
            Get-ChildItem -LiteralPath $taskDirectoryPath -Recurse -File
        }
    }) + @(foreach ($taskName in @('Cargo.toml', 'Cargo.lock', 'build.rs', 'build.ps1', '.cargo/config.toml')) {
        Get-Item -LiteralPath (Join-Path $Root $taskName)
    })
    @($taskFiles | Sort-Object FullName -Unique | ForEach-Object {
        [pscustomobject]@{
            path = $_.FullName.Substring($Root.Length + 1).Replace('\', '/')
            sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
            bytes = $_.Length
        }
    })
}
