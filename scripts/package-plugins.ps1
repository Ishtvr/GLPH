$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$pluginsRoot = Join-Path $repoRoot 'plugins'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Build-GlphPlugin {
    param([string]$PluginId)
    $srcDir = Join-Path $pluginsRoot $PluginId
    $outPkg = Join-Path $pluginsRoot ($PluginId + '.glph')
    if (Test-Path $outPkg) { Remove-Item -Force $outPkg }
    $legacyPkg = Join-Path $pluginsRoot ($PluginId + '.glph-plugin')
    if (Test-Path $legacyPkg) { Remove-Item -Force $legacyPkg }
    $zip = [System.IO.Compression.ZipFile]::Open($outPkg, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        Get-ChildItem -Path $srcDir -File -Recurse | ForEach-Object {
            $rel = $_.FullName.Substring($srcDir.Length + 1).Replace('\', '/')
            $entryName = "$PluginId/$rel"
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
}

# Copy official HP OMEN logo into the plugin bundle alongside plugin.json
$omenIconSrc = Join-Path $repoRoot 'crates\app\ui\icons\omen.png'
$omenIconDst = Join-Path $pluginsRoot 'hp-omen-control\icon.png'
if (Test-Path -LiteralPath $omenIconSrc) {
    Copy-Item -LiteralPath $omenIconSrc -Destination $omenIconDst -Force
}

Build-GlphPlugin 'hp-omen-control'
Build-GlphPlugin 'hp-omen-4zone-lighting'
