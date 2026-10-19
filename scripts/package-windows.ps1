$ErrorActionPreference = "Stop"
$version = if ($args.Count -gt 0) { $args[0] } else { "0.2.0" }
New-Item -ItemType Directory -Force dist | Out-Null
Compress-Archive -Force -Path target/release/webcraft.exe, target/release/webcraft-host.exe -DestinationPath "dist/webcraft-$version-windows-x86_64.zip"
$nsis = Get-Command makensis -ErrorAction SilentlyContinue
if (-not $nsis -and (Test-Path "C:\Program Files (x86)\NSIS\makensis.exe")) { $env:PATH += ";C:\Program Files (x86)\NSIS"; $nsis = Get-Command makensis }
if ($nsis) {
  Push-Location packaging
  makensis installer.nsi
  Pop-Location
  if (Test-Path packaging/webcraft-setup.exe) { Move-Item -Force packaging/webcraft-setup.exe "dist/webcraft-$version-windows-setup.exe" }
} else {
  Write-Host "makensis not found; zip artifact only (NSIS script in packaging/installer.nsi)"
}
Get-ChildItem dist
