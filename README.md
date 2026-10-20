<p align="center">
  <img src="assets/webcraft.png" alt="webcraft" width="128">
</p>

<h1 align="center">webcraft</h1>

<p align="center">
  A fast, cross-platform download manager written in Rust — segmented downloads,
  browser capture, debrid support, and built-in video playback.
</p>

<p align="center">
  <img alt="Version" src="https://img.shields.io/badge/version-0.13.0-blue">
  <img alt="Platforms" src="https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey">
  <img alt="Language" src="https://img.shields.io/badge/built%20with-Rust%20%2B%20egui-orange">
</p>

---

webcraft reimplements the observable behavior of Internet Download Manager from scratch: segmented downloads that resume, queues and schedules, DASH/HLS handling, a site grabber, and deep browser integration. No IDM code, binaries, icons, or activation logic — and no affiliation with Tonec.

**Current version:** 0.13.0 · Changelog lives in GitHub Releases.

**Runs on:** Windows 10/11 x86_64 · macOS 13+ (Intel and Apple Silicon) · Linux x86_64 (Ubuntu 22.04+, X11 and Wayland).

## Highlights

- **Fast starts** — links are probed and segmented in the background as soon as they are captured or pasted into the Add dialog, so confirming a download goes straight to the first byte. Queued tasks wake instantly instead of waiting for the next poll.
- **Segmented HTTP/S** — parallel connections with resume, dynamic tail-splitting when a connection stalls, and a hard cap so segments never multiply into dozens of tiny parts.
- **Debrid built in** — Real-Debrid, AllDebrid, and TorBox resolve hoster links and magnets; the list and progress dialog show which provider and hoster served each file.
- **Every protocol** — FTP/FTPS/SFTP, BitTorrent, NNTP/NZB with PAR2 repair, Metalink, WebDAV, DASH, and HLS.
- **Watch while you download** — video pages resolve through yt-dlp with a format/subtitle picker and browser cookies, and streams can play directly in mpv (embedded or external).
- **Browser capture** — Chromium and Firefox extensions detect media on the page, pass cookies, and hand downloads to the app silently.

## Features

### Download engine

- Segmented HTTP/S downloads that resume from part files after a restart, with work-stealing across a fixed connection pool (configurable in Options) and correct byte accounting — downloaded never exceeds total
- Pre-warm probing on add/capture: range probe, redirect resolution, and segment planning happen before you confirm; probe responses are reused instead of re-downloaded
- Custom HTTP headers per download (User-Agent, Referer, Authorization Bearer, or any header) from the Add dialog's *Headers (advanced)* section — HTTP/S only
- Per-download proxy override (Add URL *Transfer → Proxy*, browser Capture, or Progress dialog — Progress edits apply on restart); empty inherits the global Settings proxy. Applies to HTTP/HTTPS, HLS/DASH, Metalink, debrid, fetching NZB/.torrent files, and yt-dlp probe/download (`--proxy`); FTP/SFTP, NNTP articles, and torrent swarm stay direct. HTTP and SOCKS5 proxies work, with credentials in the URL
- Mirrors per task with failover, per-task and global speed limits, SHA-256 verification, rename templates (`{name}`, `{ext}`, `{date}`, `{host}`, `{category}`), and category subfolders

### Protocols

| Protocol | Notes |
| --- | --- |
| HTTP / HTTPS | Segmented, resumable, mirror failover |
| FTP | EPSV → PASV → active (PORT) fallback, NAT-aware PASV address handling, segmented downloads when the server accepts `REST` |
| FTPS | Explicit (`ftps://`) and implicit (`ftpsi://`) TLS; login happens after `AUTH TLS` |
| SFTP | Password, key, and keyboard-interactive auth with fallback; trust-on-first-use host keys; seek resume |
| BitTorrent | Magnet links and `.torrent` files, per-file progress, optional seeding after completion |
| NNTP / NZB | yEnc decoding, CRC32 checks, TLS, in-process PAR2 repair when the NZB includes `.par2` files |
| Metalink | v3 and v4, with bundled mirrors and SHA-256 checksums |
| WebDAV | `webdav://` and `webdavs://`, mapped to HTTP/HTTPS |
| DASH / HLS | MPD and playlist downloads, AES-128 HLS, subtitle sidecars. DRM-protected streams are refused |

### Debrid

- Real-Debrid, AllDebrid, and TorBox: hoster links and magnets are unrestricted through your account, in your preferred provider order
- Magnet results fan out to one task per file; hoster checks are cached so repeat adds start fast
- The downloads list shows a Debrid badge and the progress dialog names the provider, hoster, and cache status — no silent fallbacks

### Video and playback

- Video pages on YouTube, Vimeo, TikTok, Twitch, Dailymotion, Instagram, X, Facebook, Reddit, SoundCloud, Bilibili, and short-link hosts, resolved by a yt-dlp sidecar (native DASH/HLS fallback)
- Format, audio, and subtitle picker before downloading; browser cookies passed from the extension for login-gated pages; yt-dlp runs silently with no console windows
- Direct playback with mpv: Streams page, embedded player controls (pause, seek, volume, speed), and external mpv with your own options

### Automation and interface

- Queues with priority, retry counts, and per-queue concurrency; schedules with start/stop windows, weekday masks, quotas, missed-run catch-up, and completion actions (including shutdown with confirmation)
- Site Grabber with depth, scope, size, and extension filters and a mirror mode that rewrites links to local relative paths — including SharePoint via browser cookies
- Progress dialog with per-part status, speed graph, and live details; downloads table with saved sort/widths, protocol badges, and type filtering from the side rail
- Desktop notifications, system tray, autostart, single-instance lock, clipboard URL monitoring, drag-and-drop, HTTP/SOCKS5 proxy with authentication (global default, per-download override — see Download engine), and atomic state saving with a `.bak` fallback

> [!NOTE]
> Multi-track downloads are muxed with ffmpeg when ffmpeg is on PATH or beside the exe; single-track downloads do not need it. The app never downloads ffmpeg at runtime. yt-dlp is fetched from its GitHub releases when no binary is found, and updated after 30 days.

## Getting started

### Install

- **Windows:** run `webcraft-*-windows-setup.exe` (NSIS, per-user, no admin) or unpack the portable zip. Uninstall from Settings → Apps, or run `$LOCALAPPDATA\Programs\webcraft\Uninstall.exe`.
- **macOS:** open the `.dmg` and drag `webcraft.app` to Applications. Builds are unsigned, so the first launch needs right-click → Open.
- **Linux:** run the AppImage, unpack the tarball, or `sudo apt install ./webcraft_*_amd64.deb`. The .deb registers the desktop entry and protocol handler; GNOME tray needs AppIndicator support.

Older installs keep working: on first launch, webcraft copies `state.json`, `capture-filter.json`, and pending inbox messages from the previous (downloadcraft, or older idmcraft) config directory if the new one has no state yet. Existing webcraft state is never overwritten.

### Build from source

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
./target/release/webcraft --smoke-test
```

> [!TIP]
> On Windows, cargo may not be on PATH in a fresh PowerShell session. Use `$HOME\.cargo\bin\cargo.exe` or add `$HOME\.cargo\bin` to PATH first.

Artifacts land in `target/release/` (`webcraft` app, `webcraft-host` native messaging host) and `dist/` (portable zip via `scripts/package-windows.ps1`, Firefox XPI via `scripts/package-firefox.ps1`).

## Browser integration

The desktop app registers the native messaging host and the `webcraft://` protocol every time it starts, so moving the exe never leaves a stale host path. *Options → Repair browser integration* does the same by hand. The legacy `com.downloadcraft.host` name stays registered for one release cycle so already-installed extensions keep working.

- **Chromium:** load `extensions/chromium` as an unpacked extension.
- **Firefox:** install `extensions/webcraft-firefox.xpi` from about:addons → gear → Install Add-on From File (start webcraft once first so the host is registered). Release Firefox refuses unsigned add-ons permanently — use Developer Edition/Nightly, set `xpinstall.signatures.required` to false where allowed, or load `extensions/firefox` temporarily from about:debugging. Rebuild the XPI with `scripts/package-firefox.ps1`.

The extension watches downloads passively and ranks what it finds, so a page with many small segments offers the playlist or file first. Playlist candidates (.m3u8/.mpd) are kept for 30 minutes (other candidates 5), evicted last when the candidate cap is hit, and partial (206) responses count as candidates too — so a stream detected early is still offered when you click later. It hands video-page links to the app with the page's cookies, where yt-dlp resolves formats. Capture can be limited by an extension allowlist and minimum size in Options (`capture-filter.json`). Video buttons stay hidden until playback starts.

## Interface

The main window opens at 1440×900 and groups related actions and labels each group: download actions stay with the selected download, queue actions with all downloads. Narrow windows switch to a compact column set, persisted column widths shrink to fit instead of forcing horizontal scrolling, and the side rail scrolls when it runs out of height. Column widths and sort order persist between launches. Add (620×840), Capture (580×700), Progress (700×860), Collision (560×280), and Player (1024×640) dialogs open as separate windows; opening Add or a browser Capture grows a smaller restored main window back to full size. Delete confirmations keep a fixed size so buttons never jump under your cursor. Options save as soon as a setting changes.

## Documentation

- `docs/reverse-engineering.md` — observations of IDM behavior, DASH/HLS experiment tables, design decisions
- `docs/parity.md` — feature status with the tests backing each claim
- `docs/release-checklist.md` — manual per-OS sign-off list

## Limits

DRM-protected streams are refused on purpose. NNTP repair uses PAR2 files included in the NZB; archive unpacking is not done yet. Segmented FTP restarts the file when resumed (single-connection FTP resumes normally). There is no Safari extension — that needs Xcode signing. `docs/parity.md` lists the remaining partial items.
