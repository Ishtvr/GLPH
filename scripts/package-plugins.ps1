$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$pluginsRoot = Join-Path $repoRoot 'plugins'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$srcDir = Join-Path $pluginsRoot 'hp-omen-4zone-lighting'
$outPkg = Join-Path $pluginsRoot 'hp-omen-4zone-lighting.perfhub-plugin'
if (Test-Path $outPkg) { Remove-Item -Force $outPkg }

$zip = [System.IO.Compression.ZipFile]::Open($outPkg, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    Get-ChildItem -Path $srcDir -File -Recurse | ForEach-Object {
        $rel = $_.FullName.Substring($srcDir.Length + 1).Replace('\', '/')
        $entryName = "hp-omen-4zone-lighting/$rel"
        [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $zip,
            $_.FullName,
            $entryName,
            [System.IO.Compression.CompressionLevel]::Optimal
        ) | Out-Null
    }
} finally {
    $zip.Dispose()
}
Write-Host "Packaged $outPkg ($((Get-Item $outPkg).Length) bytes)"
