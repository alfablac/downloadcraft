# idmcraft

Clean-room, cross-platform reimplementation of Internet Download Manager's observable feature set. Rust + egui. Not affiliated with Tonec/IDM. No proprietary code, binaries, icons, or activation logic.

Targets: Windows 10/11 x86_64, macOS 13+ universal (Intel + Apple Silicon), Linux x86_64 (Ubuntu 22.04+, X11/Wayland).

## Features
Segmented resumable HTTP downloads, queues with priority/scheduling, DASH (MPD) + HLS downloading with DRM refusal, Site Grabber, browser extensions (Chromium MV3 + Firefox) with native-messaging host, tray + autostart + `idmcraft://` protocol, `--smoke-test` for CI.

## Build
```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
./target/release/idmcraft --smoke-test
```
ffmpeg on PATH (or beside the exe) enables multi-track (audio+video) muxing; single-track downloads need no ffmpeg. Never downloaded at runtime.

## Install

### Windows
1. Download `idmcraft-*-windows-setup.exe` (NSIS, per-user, no admin) or portable zip from Releases. 2. Run installer. 3. Launch from Start Menu; app appears in tray.
Uninstall: Settings → Apps → idmcraft, or `$LOCALAPPDATA\Programs\idmcraft\Uninstall.exe`. Removes autostart + browser host registration.

### macOS
1. Download `.dmg`, open, drag `idmcraft.app` to Applications. 2. First launch: right-click → Open (unsigned builds; notarization is a release-checklist item when certificates are available). 3. App runs with menu-bar tray icon.
Uninstall: delete app from Applications, `~/Library/LaunchAgents/com.idmcraft.plist`, and `com.idmcraft.host.json` files under `~/Library/Application Support/*/NativeMessagingHosts` (or in-app Options → Remove browser integration first).

### Linux
AppImage/tarball: `chmod +x` and run, or `sudo apt install ./idmcraft_*_amd64.deb`. .deb registers desktop entry + protocol; tray on GNOME needs AppIndicator support. Uninstall: `sudo apt remove idmcraft`; remove `~/.config/autostart/idmcraft.desktop` (or disable in Options first).

## Browser integration
Load unpacked: Chromium `extensions/chromium`, Firefox `extensions/firefox` (about:debugging). Then Options → "Repair browser integration" registers native hosts per-user for Chrome/Edge/Brave/Chromium/Vivaldi/Firefox (paths per OS in `docs/reverse-engineering.md`). Native host name: `com.idmcraft.host`.

## Docs
- `docs/reverse-engineering.md` — IDM observation log, DASH/HLS experiment tables, ADRs
- `docs/parity.md` — feature parity checklist with test evidence
- `docs/release-checklist.md` — manual per-OS sign-off

## Limits
DRM-protected streams refused (by design). Safari extension out of scope (Xcode signing). See parity doc for partial items.
