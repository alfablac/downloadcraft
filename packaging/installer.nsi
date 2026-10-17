; idmcraft NSIS per-user installer
Name "idmcraft"
OutFile "idmcraft-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\idmcraft"
RequestExecutionLevel user
Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File "..\target\release\idmcraft.exe"
  File "..\target\release\idmcraft-host.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\idmcraft"
  CreateShortcut "$SMPROGRAMS\idmcraft\idmcraft.lnk" "$INSTDIR\idmcraft.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\idmcraft" "DisplayName" "idmcraft"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\idmcraft" "UninstallString" "$INSTDIR\Uninstall.exe"
  Exec '"$INSTDIR\idmcraft.exe" --register-hosts'
SectionEnd

Section "Uninstall"
  Exec '"$INSTDIR\idmcraft.exe" --unregister'
  Delete "$INSTDIR\idmcraft.exe"
  Delete "$INSTDIR\idmcraft-host.exe"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\idmcraft\idmcraft.lnk"
  RMDir "$SMPROGRAMS\idmcraft"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\idmcraft"
  RMDir "$INSTDIR"
SectionEnd
