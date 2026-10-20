# Changelog

All notable changes to this project will be documented in this file.

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
