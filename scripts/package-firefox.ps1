# Build Firefox XPI (manifest.json must sit at archive root).
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root "extensions\webcraft-firefox.xpi"
Compress-Archive -Path (Join-Path $root "extensions\firefox\*") -DestinationPath $out -Force
Write-Host "Wrote $out"
Write-Host "Install: Firefox about:addons > gear > Install Add-on From File (unsigned XPI needs Developer/Nightly or xpinstall.signatures.required=false; permanent signed install requires AMO signing)."
