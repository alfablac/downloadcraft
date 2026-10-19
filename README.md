# webcraft

webcraft is a download manager written in Rust with egui. It reimplements the observable parts of Internet Download Manager: segmented downloads, queues, schedules, DASH and HLS, a site grabber, and browser integration. The code was written from scratch. It has no IDM code, binaries, icons, or activation logic, and it is not affiliated with Tonec.

Current version: 0.10.1.

Runs on Windows 10/11 x86_64, macOS 13 or later (Intel and Apple Silicon), and Linux x86_64 (Ubuntu 22.04 or later, X11 and Wayland).

## What it does

- Segmented HTTP downloads that resume from part files after a restart, with dynamic segment splitting when a connection stalls
- FTP downloads with PASV and REST resume, plus FTPS (explicit ftps:// and implicit ftpsi://) over TLS
- SFTP over SSH, with password or key login, trust on first use host keys, and seek resume
- BitTorrent from magnet links or .torrent files, with per-file progress and optional seeding after the download finishes
- NNTP and NZB downloads, with yEnc decoding, CRC32 checks, TLS, and in-process PAR2 repair (when the NZB includes .par2 files). Archive unpacking is not done
- Metalink v3 and v4 files, which bring their own mirrors and SHA-256 checksum
- WebDAV through webdav:// and webdavs:// URLs, mapped to plain HTTP or HTTPS
- Video pages on YouTube, Vimeo, TikTok, Twitch, Dailymotion, Instagram, X, Facebook, Reddit, SoundCloud, Bilibili, and a few short link hosts, handled by a yt-dlp sidecar when one is available and by the built in DASH and HLS code when it is not
- Queues with priority, retry counts, and per-queue concurrency limits
- Schedules with start and stop windows, weekday masks, download quotas, an optional catch up for runs missed while the app was closed, and completion actions that can shut the machine down, with an optional confirmation prompt first
- Speed limits per task and one global limit shared across running tasks, editable from the progress dialog
- DASH (MPD) and HLS downloads, including AES-128 HLS and subtitle sidecars. DRM-protected streams are refused
- Site Grabber with depth, scope, size, and extension filters, plus a site mirror that rewrites links to local relative paths
- Chromium MV3 and Firefox extensions that hand downloads to the desktop app through a native messaging host
- Drag-and-drop URLs or .url files onto the main window
- Clipboard URL monitoring, proxy with authentication, and SHA-256 verification when a checksum is supplied
- Mirrors per task, with one shared username and password, and failover when a mirror does not answer
- Rename templates using {name}, {ext}, {date}, {host}, and {category}, and an option to file new downloads into a subfolder named after their category
- Desktop notifications when a download finishes or fails and when a queue drains, an update checker that fetches a newer release as a normal download, and state saved atomically with a .bak copy to fall back on after a crash
- System tray, autostart, single-instance lock, and the webcraft:// URL protocol (legacy downloadcraft:// links still accepted)
- Downloads table that remembers its sort column and dragged column widths, shows the protocol of each row (TORRENT, FTP, NNTP, HLS, and so on), and can be filtered by that type from the side rail
- Tag badges colored by category and protocol type, in the download list and in dialogs
- Progress, Add, and Capture dialogs that open as separate windows, with a per-task speed graph, a status word per part, torrent swarm figures (peers, trackers, pieces), and a resume row
- Panics are written to crash.log in the config folder instead of taking the whole app down silently
- --smoke-test for CI

Multi-track downloads are muxed with ffmpeg when ffmpeg is on PATH or beside the exe. Single-track downloads do not need it. The app never downloads ffmpeg at runtime. yt-dlp is different: if no yt-dlp binary is found for a video page, the app fetches one from the yt-dlp GitHub releases, and updates an existing copy once it is more than 30 days old.

## Recent versions

- 0.10.1: fix output validation (empty/playlist results now quarantined correctly, checked before renaming over existing files), bare `.ts` stream URLs allowed through probe, strict check after muxing
- 0.10.0: NZB downloads in parallel over several NNTP connections (1-64 in Options) with in-process PAR2 repair and a file list preview before starting; pick files inside torrents before and during download; video pages probe formats first (quality, audio track, audio-only MP3/M4A, subtitles); extension matches streams to the clicked video only and adds a "This video" quality entry; downloads list gains multi-select, search shortcut, time-left/rate/progress sorting, and delete confirmation with delete-from-disk; colored count badges in the side bar; finished files validated (bad output quarantined as `.bad`) and resumed HLS/DASH cache poisoning fixed. See git history for full list
- 0.9.0: crash fixes (tray menu pointer, progress bar NaN, poisoned locks, worker panics turned into task errors, duplicate magnets), protocol labels and a type filter in the downloads table, Progress, Add, and Capture as separate windows, per-file torrent parts and swarm figures, and a browser extension that only picks up real downloads and leaves JSON, scripts, and stylesheets alone
- 0.8.0: per-task speed graph, part status words, and a resume row in the progress dialog; extension starts ranking stream candidates (files before playlists before segments) and preselecting the top one. Scheduler catch up for missed runs, an optional confirmation before power actions, and NNTP tolerance fixes landed after this tag, on the way to 0.9.0
- 0.7.0: spacing and form layout pass over the dialogs, torrent options in Settings (listen port, seed after complete), and desktop notifications
- 0.6.0: folder rules and category subfolders, rename templates, update checker, retry and import fixes, and speed limit windows
- 0.5.0: FTPS and BitTorrent added; state file and filename handling hardened against reserved names and missing fields
- 0.4.0: renamed from downloadcraft to webcraft; yt-dlp video pages, Metalink, WebDAV, SFTP, and NNTP/NZB added

## Build

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
./target/release/webcraft --smoke-test
```

On Windows, cargo may not be on PATH in a fresh PowerShell session. Use `$HOME\.cargo\bin\cargo.exe` or add `$HOME\.cargo\bin` to PATH first.

## Install

Windows: run `webcraft-*-windows-setup.exe` (NSIS, per-user, no admin) or unpack the portable zip. Uninstall from Settings, Apps, webcraft, or run `$LOCALAPPDATA\Programs\webcraft\Uninstall.exe`. Uninstall removes autostart and the browser host registration.

macOS: open the `.dmg` and drag `webcraft.app` to Applications. Builds are unsigned, so the first launch needs right-click, Open. To uninstall, delete the app, `~/Library/LaunchAgents/com.webcraft.plist`, and the `com.webcraft.host.json` files under `~/Library/Application Support/*/NativeMessagingHosts`. Running Options, Remove browser integration first does the host part for you.

Linux: make the AppImage executable and run it, unpack the tarball, or install the package with `sudo apt install ./webcraft_*_amd64.deb`. The .deb registers the desktop entry and protocol handler. Tray on GNOME needs AppIndicator support. Uninstall with `sudo apt remove webcraft` and remove `~/.config/autostart/webcraft.desktop`, or turn autostart off in Options first.

Older installs still work. On first launch, webcraft copies `state.json`, `capture-filter.json`, and any pending inbox messages from the previous (downloadcraft, or older idmcraft) config directory if the new one has no state yet. It does not overwrite existing webcraft state.

## Browser integration

The desktop app registers the native messaging host and protocol every time it starts, so moving the exe does not leave a stale host path behind. Options, Repair browser integration does the same thing by hand.

Chromium: load `extensions/chromium` as an unpacked extension.

Firefox: a ready XPI is at `extensions/webcraft-firefox.xpi`. Remove the previous add-on first; it asks for a native host name that no longer exists. Start webcraft once so it registers the current host, then install the XPI from about:addons, gear menu, Install Add-on From File. Firefox release builds refuse unsigned add-ons permanently; use Firefox Developer Edition or Nightly, or set `xpinstall.signatures.required` to false in about:config on builds that allow it. For a temporary install, load `extensions/firefox` from about:debugging, This Firefox, Load Temporary Add-on. Rebuild the XPI with `scripts/package-firefox.ps1`.

The extension watches for downloads passively and ranks what it finds, so a page that loads many small segments offers the playlist or file first. Capture can be limited by extension allowlist and minimum size in Options, stored in `capture-filter.json`. Video buttons on pages stay out of the way until playback starts.

Native host name: `com.webcraft.host`. The legacy name `com.downloadcraft.host` is also registered for one release cycle so already-installed extensions keep working.

## Interface

The main window groups related actions and labels each group. Download actions stay with the selected download. Queue actions stay with all downloads. Narrow windows scroll the download table instead of squeezing its columns together, and switch the table to a compact set of columns. Column widths and the sort column are saved and restored on the next launch.

Options save as soon as a setting changes. Site Grabber fields use the full panel width for the start URL and extensions list.

## Docs

- `docs/reverse-engineering.md` records what was observed in IDM, the DASH and HLS experiment tables, and design decisions
- `docs/parity.md` lists feature status with the tests that back each claim
- `docs/release-checklist.md` is the manual per-OS sign-off list

## Limits

DRM-protected streams are refused on purpose. NNTP downloads repair with included PAR2 files in-process; archive unpacking is not done yet. There is no Safari extension because that needs Xcode signing. `docs/parity.md` lists the partial items.
