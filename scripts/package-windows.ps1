$ErrorActionPreference = "Stop"
$version = if ($args.Count -gt 0) { $args[0] } else { "0.1.0" }
New-Item -ItemType Directory -Force dist | Out-Null
Compress-Archive -Force -Path target/release/idmcraft.exe, target/release/idmcraft-host.exe -DestinationPath "dist/idmcraft-$version-windows-x86_64.zip"
$nsis = Get-Command makensis -ErrorAction SilentlyContinue
if ($nsis) {
  Push-Location packaging
  makensis installer.nsi
  Pop-Location
  if (Test-Path packaging/idmcraft-setup.exe) { Move-Item -Force packaging/idmcraft-setup.exe "dist/idmcraft-$version-windows-setup.exe" }
} else {
  Write-Host "makensis not found; zip artifact only (NSIS script in packaging/installer.nsi)"
}
Get-ChildItem dist
