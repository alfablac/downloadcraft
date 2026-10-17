$ErrorActionPreference = "Stop"
$version = if ($args.Count -gt 0) { $args[0] } else { "0.2.0" }
New-Item -ItemType Directory -Force dist | Out-Null
Compress-Archive -Force -Path target/release/downloadcraft.exe, target/release/downloadcraft-host.exe -DestinationPath "dist/downloadcraft-$version-windows-x86_64.zip"
$nsis = Get-Command makensis -ErrorAction SilentlyContinue
if ($nsis) {
  Push-Location packaging
  makensis installer.nsi
  Pop-Location
  if (Test-Path packaging/downloadcraft-setup.exe) { Move-Item -Force packaging/downloadcraft-setup.exe "dist/downloadcraft-$version-windows-setup.exe" }
} else {
  Write-Host "makensis not found; zip artifact only (NSIS script in packaging/installer.nsi)"
}
Get-ChildItem dist
