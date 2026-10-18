; webcraft NSIS per-user installer
Name "webcraft"
OutFile "webcraft-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\webcraft"
RequestExecutionLevel user
Page directory
Page instfiles
UninstPage uninstConfirm
Icon "..\assets\webcraft.ico"
UninstallIcon "..\assets\webcraft.ico"
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File "..\target\release\webcraft.exe"
  File "..\target\release\webcraft-host.exe"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  CreateDirectory "$SMPROGRAMS\webcraft"
  CreateShortcut "$SMPROGRAMS\webcraft\webcraft.lnk" "$INSTDIR\webcraft.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\webcraft" "DisplayName" "webcraft"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\webcraft" "DisplayIcon" "$INSTDIR\webcraft.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\webcraft" "UninstallString" "$INSTDIR\Uninstall.exe"
  Exec '"$INSTDIR\webcraft.exe" --register-hosts'
SectionEnd

Section "Uninstall"
  Exec '"$INSTDIR\webcraft.exe" --unregister'
  Delete "$INSTDIR\webcraft.exe"
  Delete "$INSTDIR\webcraft-host.exe"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\webcraft\webcraft.lnk"
  RMDir "$SMPROGRAMS\webcraft"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\webcraft"
  RMDir "$INSTDIR"
SectionEnd
