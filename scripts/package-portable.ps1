param([string]$Executable, [string]$OutputDirectory)

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$version = (Get-Content -LiteralPath (Join-Path $repoRoot 'package.json') -Raw | ConvertFrom-Json).version
if ($version -notmatch '^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$') { throw 'Invalid package version' }
if (-not $Executable) { $Executable = Join-Path $repoRoot 'target\release\pinkbin.exe' }
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repoRoot 'release' }
$Executable = (Resolve-Path -LiteralPath $Executable).ProviderPath
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (-not (Test-Path -LiteralPath $OutputDirectory)) {
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
}
$zipPath = Join-Path $OutputDirectory "Pinkbin_$($version)_windows_x64.zip"
if (Test-Path -LiteralPath $zipPath) { throw "Archive already exists: $zipPath" }
$productVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($Executable).ProductVersion
if ($productVersion -ne $version) { throw "Executable version $productVersion does not match $version" }

Add-Type -AssemblyName System.IO.Compression
$stream = [IO.File]::Open($zipPath, [IO.FileMode]::CreateNew)
try {
    $archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
    try {
        # An explicit file list excludes existing portable settings and caches.
        foreach ($file in @(
            @{ Source=$Executable; Name="Pinkbin_$version.exe" },
            @{ Source=(Join-Path $repoRoot 'docs\PORTABLE_README.txt'); Name='使用说明.txt' },
            @{ Source=(Join-Path $repoRoot 'LICENSE'); Name='LICENSE.txt' }
        )) {
            $entry = $archive.CreateEntry($file.Name, [IO.Compression.CompressionLevel]::Optimal)
            $inputFile = [IO.File]::OpenRead($file.Source)
            $outputFile = $entry.Open()
            try { $inputFile.CopyTo($outputFile) }
            finally { $inputFile.Dispose(); $outputFile.Dispose() }
        }
    } finally { $archive.Dispose() }
} finally { $stream.Dispose() }
[pscustomobject]@{
    Path=$zipPath; Version=$version; Bytes=(Get-Item -LiteralPath $zipPath).Length
    SHA256=(Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash
}
