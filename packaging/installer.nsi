; downloadcraft NSIS per-user installer
Name "downloadcraft"
OutFile "downloadcraft-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\downloadcraft"
RequestExecutionLevel user
Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File "..\target\release\downloadcraft.exe"
  File "..\target\release\downloadcraft-host.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\downloadcraft"
  CreateShortcut "$SMPROGRAMS\downloadcraft\downloadcraft.lnk" "$INSTDIR\downloadcraft.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\downloadcraft" "DisplayName" "downloadcraft"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\downloadcraft" "UninstallString" "$INSTDIR\Uninstall.exe"
  Exec '"$INSTDIR\downloadcraft.exe" --register-hosts'
SectionEnd

Section "Uninstall"
  Exec '"$INSTDIR\downloadcraft.exe" --unregister'
  Delete "$INSTDIR\downloadcraft.exe"
  Delete "$INSTDIR\downloadcraft-host.exe"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\downloadcraft\downloadcraft.lnk"
  RMDir "$SMPROGRAMS\downloadcraft"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\downloadcraft"
  RMDir "$INSTDIR"
SectionEnd
