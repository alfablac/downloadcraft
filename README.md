# downloadcraft

downloadcraft is a download manager written in Rust with egui. It covers the parts of Internet Download Manager that can be observed from outside: segmented downloads, queues, schedules, DASH and HLS, a site grabber, and browser integration. The code was written from scratch. It has no IDM code, binaries, icons, or activation logic, and it is not affiliated with Tonec.

Runs on Windows 10/11 x86_64, macOS 13 or later (Intel and Apple Silicon), and Linux x86_64 (Ubuntu 22.04 or later, X11 and Wayland).

## What it does

- Segmented HTTP downloads that resume from part files after a restart
- FTP downloads with PASV and REST resume
- Queues with priority, retry counts, and per-queue concurrency limits
- Schedules with start and stop windows, weekday masks, and download quotas
- DASH (MPD) and HLS downloads, including AES-128 HLS. DRM-protected streams are refused
- Site Grabber with depth, scope, size, and extension filters, plus a site mirror that rewrites links to local relative paths
- Chromium MV3 and Firefox extensions that hand downloads to the desktop app through a native messaging host
- System tray, autostart, single-instance lock, and the `downloadcraft://` URL protocol
- `--smoke-test` for CI

Multi-track downloads are muxed with ffmpeg when ffmpeg is on PATH or beside the exe. Single-track downloads do not need it. The app never downloads ffmpeg at runtime.

## Build

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
./target/release/downloadcraft --smoke-test
```

On Windows, cargo may not be on PATH in a fresh PowerShell session. Use `$HOME\.cargo\bin\cargo.exe` or add `$HOME\.cargo\bin` to PATH first.

## Install

Windows: run `downloadcraft-*-windows-setup.exe` (NSIS, per-user, no admin) or unpack the portable zip. Uninstall from Settings, Apps, downloadcraft, or run `$LOCALAPPDATA\Programs\downloadcraft\Uninstall.exe`. Uninstall removes autostart and the browser host registration.

macOS: open the `.dmg` and drag `downloadcraft.app` to Applications. Builds are unsigned, so first launch needs right-click, Open. To uninstall, delete the app, `~/Library/LaunchAgents/com.downloadcraft.plist`, and the `com.downloadcraft.host.json` files under `~/Library/Application Support/*/NativeMessagingHosts`. Running Options, Remove browser integration first does the host part for you.

Linux: make the AppImage executable and run it, unpack the tarball, or install the package with `sudo apt install ./downloadcraft_*_amd64.deb`. The .deb registers the desktop entry and protocol handler. Tray on GNOME needs AppIndicator support. Uninstall with `sudo apt remove downloadcraft` and remove `~/.config/autostart/downloadcraft.desktop`, or turn autostart off in Options first.

Older installs under the previous name still work. On first launch, downloadcraft copies `state.json` and any pending inbox messages from the old idmcraft config directory if the new one has no state yet. It does not overwrite existing downloadcraft state.

## Browser integration

The desktop app registers the native messaging host and protocol every time it starts, so moving the exe does not leave a stale host path behind. Options, Repair browser integration does the same thing by hand.

Chromium: load `extensions/chromium` as an unpacked extension.

Firefox: a ready XPI is at `extensions/downloadcraft-firefox.xpi`. Remove the old idmcraft add-on first; it asks for the old native host name, which no longer exists. Start downloadcraft once so it registers the current host, then install the XPI from about:addons, gear menu, Install Add-on From File. Firefox release builds refuse unsigned add-ons permanently; use Firefox Developer Edition or Nightly, or set `xpinstall.signatures.required` to false in about:config on builds that allow it. For a temporary install, load `extensions/firefox` from about:debugging, This Firefox, Load Temporary Add-on. Rebuild the XPI with `scripts/package-firefox.ps1`.

Native host name: `com.downloadcraft.host`.

## Docs

- `docs/reverse-engineering.md` records what was observed in IDM, the DASH and HLS experiment tables, and design decisions
- `docs/parity.md` lists feature status with the tests that back each claim
- `docs/release-checklist.md` is the manual per-OS sign-off list

## Limits

DRM-protected streams are refused on purpose. There is no Safari extension because that needs Xcode signing. `docs/parity.md` lists the partial items.
