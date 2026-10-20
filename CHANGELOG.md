# Changelog

All notable changes to webcraft. Versions follow the app version in Cargo.toml.

## [0.15.0] - 2026-10-20

### Added

- Drives tab: dual-pane cloud storage (next to Streams / Site Grabber) - Google Drive, OneDrive, Dropbox (OAuth, own client_id), WebDAV, S3-compatible, SFTP, FTP/FTPS, Telegram via self-hosted teldrive, local folders. Browse/upload/download/copy/move with pause/resume/cancel, multithreaded ranged/session transfers where supported, sync (compare preview, copy/update/mirror, saved jobs), per-account rclone-crypt encryption.
- Drives account setup wizard (rclone config order) from zero accounts: Provider > Details (per-provider console steps, fixed redirect http://127.0.0.1:53682/) > Connect/Test in draft (browser auto-capture, paste-code fallback, cancel) > Root pick after sign-in (Google Team Drive separate from optional start folder; Shared Drives auto-list) > Review final test > Finish saves once and opens selected account in a pane.
- Drives Accounts: Edit anytime with Test connection, Duplicate (clone credentials to another Team Drive/root), and Import rclone.conf paste (obscured passwords revealed; token/team_drive mapped).
- Drives transfers panel: multi-file concurrency via per-account Parallel streams (concurrent files + segments per file), S3 multipart parallel parts, job summaries (source -> destination), (i) info (bytes/%/speed/ETA/elapsed), per-job Remove (deletes destination data for stopped jobs), red full error text. Shift-range and Ctrl/Cmd multi-select; Refresh and Up on both panes.

### Fixed

- Windows browser open no longer truncates OAuth URL at & (Google Missing required parameter: client_id 400).
- Wizard/accounts/sync windows fixed max 580/620x700 (content no longer pushes window big).
- Destination folders created once per job after existence check (Google Drive create-folder was duplicating per file/segment).
- Upload progress counts destination-upload bytes only (local staging copy no longer jumps to 100%).
- Upload cancel now interrupts in-flight upload; partial destinations removed on error.

## [0.14.0] - 2026-10-20

### Added


- Per-download proxy override (Add URL *Transfer → Proxy*, browser Capture, Progress dialog; Progress edits apply on restart). Empty inherits global Settings proxy. Drives reqwest downloads, probing (incl. yt-dlp `--proxy`), torrent/NZB/debrid fetches; SOCKS5 proxies now work (reqwest `socks` feature). FTP/SFTP, NNTP articles, torrent swarm stay direct.
- NZB post-download unpack: archive extraction with NZB-embedded passwords (`<meta type="password">`, `{{pw}}` in NZB/archive names, per-archive override); `{{pw}}` secrets stripped from output names.

### Changed

- Main window opens 1440×900 (was 1180×720); Capture/Add/Progress/Collision/Player dialogs larger; browser Capture/Add reveal grows a smaller restored main window back to full size.
- Extension candidates: playlist URLs (.m3u8/.mpd) retained 30 min and evicted last at cap; 206 partial responses count as candidates.

### Fixed

- Release pipeline: all clippy warnings fixed under `-D warnings` (incl. platform-only Linux/macOS warnings), Rust toolchain pinned in CI — tag builds publish again (v0.13.0 tag built but failed clippy, so no Release was created).
- Downloads table: persisted column widths shrink to fit instead of overflowing; side rail scrolls; NNTP `{{pw}}` percent-encoded names decode correctly.

## [0.13.0] - 2026-10-19

### Added

- Streams app function: Streams tab plays direct video links in embedded mpv player inside app window (native child surface + IPC controls: play/pause, seek, volume, speed, mute).
- Options mpv tab: mpv path, fullscreen, keep-open, loop, volume, speed, start position, hwdec, cache, subtitle auto-load, extra flags.
- Duplicate filename prompt on Add/Capture: Overwrite / Rename name_N.ext / Cancel; batch, grabber, clipboard adds auto-rename.

### Changed

- Streams removed Downloads type filter (now dedicated tab).
- New URL / capture modals always open in front main window keyboard focus in dialog.
- yt-dlp warnings in dialogs compacted: short summary + collapsed Details instead raw traceback block.

### Fixed

- yt-dlp downloads saved x.mp4 (host-named) on X/Twitter etc: extracted video title now used filename.
- Embedded mpv: no-audio/no-controls caused mpv rendering over app GL surface; forced volume/mute settings, mpv log file, audio-track state shown in player.

## [0.12.1] - 2026-10-17

### Added

- Video download button top-left; app-styled options panel.

[0.14.0]: https://github.com/alfablac/webcraft/releases/tag/v0.14.0
[0.13.0]: https://github.com/alfablac/webcraft/releases/tag/v0.13.0
