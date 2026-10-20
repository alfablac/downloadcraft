# Changelog

All notable changes to this project will be documented in this file.

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

[0.13.0]: https://github.com/alfablac/webcraft/releases/tag/v0.13.0
