# Build Firefox XPI (manifest.json at archive root, "/" entry names).
# ponytail: System.IO.Compression stores given names verbatim; Compress-Archive
# writes "\" names in PS 5.1 and AMO rejects archive. Keep manual zip writer.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root "extensions\webcraft-firefox.xpi"
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
if (Test-Path $out) { Remove-Item $out -Force }
$src = Join-Path $root "extensions\firefox"
$fs = [System.IO.File]::Open($out, [System.IO.FileMode]::CreateNew)
try {
  $zip = New-Object System.IO.Compression.ZipArchive($fs, [System.IO.Compression.ZipArchiveMode]::Create)
  try {
    Get-ChildItem $src -Recurse -File | ForEach-Object {
      $name = $_.FullName.Substring($src.Length).TrimStart('\', '/').Replace('\', '/')
      [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $_.FullName, $name, [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
    }
  } finally { $zip.Dispose() }
} finally { $fs.Dispose() }
Write-Host "Wrote $out"
