# Release checklist (manual, per OS)

## Windows 10/11 x86_64
- [ ] Install NSIS setup (or unzip portable), launch, tray icon visible
- [ ] Test download (HTTP segmented) completes, file opens
- [ ] DASH test stream downloads, plays
- [ ] Enable "Launch at OS startup", sign out/in: app in tray, no window flash
- [ ] Chrome/Edge/Brave: extension loaded, native host ping OK, download intercepted
- [ ] Firefox: extension + native host OK
- [ ] Uninstall removes app, autostart, native-host registry, protocol
Sign-off: __________ Date: ______

## macOS 13+ (Intel + Apple Silicon)
- [ ] Open .dmg (or .zip), drag to Applications, first launch (right-click Open if unsigned)
- [ ] Menu-bar/tray icon visible, test download OK
- [ ] Login Item/LaunchAgent at login: hidden start
- [ ] Chrome/Edge/Brave/Firefox native hosts in ~/Library/Application Support
- [ ] Uninstall: remove app, LaunchAgent, host manifests
Sign-off: __________ Date: ______

## Linux Ubuntu 22.04+ (X11 and Wayland noted separately)
- [ ] .deb installs / .AppImage (or tarball) runs, tray (AppIndicator on GNOME: note extension need)
- [ ] Test download OK on X11: ___ Wayland: ___
- [ ] XDG autostart after re-login: hidden in tray
- [ ] Native hosts in ~/.config/*, ~/.mozilla/native-messaging-hosts
- [ ] Uninstall removes autostart .desktop, hosts, protocol handler
Sign-off: __________ Date: ______
