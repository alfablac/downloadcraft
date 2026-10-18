# Feature parity checklist

| IDM feature | Clone status | Evidence |
|---|---|---|
| Segmented multi-connection HTTP download (default 8) | Done | `engine::plan_segments`, e2e `http_segmented_download_and_resume` |
| Resume / pause / crash recovery, ETag validation | Done | `.downloadcraft-<id>/state.json`, preseeded-resume e2e |
| Retry with backoff (engine), queue retry policy | Done | engine attempt loop; app queue retry requeue |
| Speed limiter (global aggregate-divided + per task, Progress dialog field) | Done | `Controls.limit_kbps` throttle; `app::effective_limit_kbps` |
| Proxy + auth (URL form) | Done | `build_client(proxy)` reqwest |
| Checksum SHA-256 verify | Done | `finish_check` |
| Queues: caps, priority, drag order (Up/Down), FIFO order field | Done | `queue::plan_starts` unit + GUI buttons |
| Categories by extension | Done | `model::category_for` |
| Clipboard URL monitor | Done | app poll (arboard), per-OS via crate |
| Drag-and-drop URLs | Done | Window drop handler (URL text + `.url` files, `raw.dropped_files`) + clipboard + inbox + Add box; browser drag via extension |
| Add by URL with cookies/headers/referrer | Done | Add dialog fields + task model |
| DASH SegmentTemplate / Timeline / List / SegmentBase | Done | mpd unit + e2e; SegmentBase = single-file download once (init_range parsed, no double-fetch) |
| HLS BYTERANGE implicit offset, audio rendition mux | Done | hls `resolve_implicit_ranges` unit; stream mux via ffmpeg, video kept + error on mux fail |
| Engine resume correctness (ETag saved, intra-segment truncate, short-range reject, Content-Range check, global throttle, timeouts) | Done | engine fixes + existing resume e2e |
| GUI modern shell (dark design tokens, sidebar categories, toolbar, sortable table, progress/complete dialogs, speed sparkline) | Done | `src/theme.rs` + `src/app.rs`; design-system/webcraft/MASTER.md |
| Chromium extension ID pinned (`key` in manifest) | Done | `platform::CHROME_EXT_ID` = `ajkokpbgjajmkenllekaokjbglbcingk` + drift test; private key NOT in repo |
| Queue retry count/interval enforced, scheduler respects caps, completion only when queue drained | Done | task `attempts`/`next_retry_at`, planner-routed schedule starts |
| Dynamic segment splitting / work stealing (IDM dynamic segmentation) | Done | engine `split_point`/`claim_or_split`, split tests |
| Mirrors (Url0..UrlN) + shared task Basic auth | Done | task `mirrors`/`username`/`password` (one credential pair per task), engine probe failover |
| FTP (PASV, REST resume) | Done | `src/ftp.rs` + fake-server tests |
| Desktop notifications (complete/failed/queue-drained, toggle) | Done | `platform::notify`, `app.rs` poll |
| Update checker (GitHub latest, download-as-task, no self-replace) | Done | `src/update.rs`, About tab |
| Rename templates `{name}{ext}{date}{host}{category}` + category subfolders | Done | `model::expand_template`, `main.rs` funnel |
| Extension candidate ranking (file>playlist>segment, dedupe) | Done | `extensions/*/rank.js`, `extensions/rank.test.mjs` (node) |
| Browser capture filter (ext allowlist + min size) | Done | `src/bin/webcraft_host.rs`, `capture-filter.json` |
| State crash recovery (atomic save + `.bak` fallback) | Done | `main::load_state`/`save_state` |
| Progress dialog extras (per-task speed graph, part status words, resume row) | Done | `app.rs` progress dialog |
| Scheduler quota (no more than X MB every Y hours) | Done | schedule `quota_*`, `quota_exhausted` gate |
| Scheduler IDM dialog layout (named list New/Delete, Turn-on master, HH:MM AM/PM, Once date, Repeat N days, weekdays, Stop at, files-at-once, completion/power confirm) | Done | IDM two-pane page; `Schedule.name/repeat_days`, `Settings.scheduler_enabled`, repeat logic in `scheduler.rs`; hang-up modem N/A |
| Row interaction (aligned fixed-height rows, working Start/Stop, pointer cursor) | Done | allocate-first `icon_btn_enabled`, 20px `cell` (22px pitch, IDM 17px parity), labels `selectable(false)`; engine cancel e2e |
| Version onscreen (title, top-bar chip, About, `--version`) | Done | `env!("CARGO_PKG_VERSION")` single source |
| Browser manifest sniffing (webRequest) → download panel candidates | Done | Chromium + Firefox candidates map, panel merge |
| Content-Type manifest dispatch (no extension in URL) | Done | `stream::probe_manifest` |
| Current release published (matrix + release job green, SHA256SUMS verified) | Done | GitHub Releases (latest v0.8.0); NSIS/AppImage best-effort in CI |
| yt-dlp sidecar (14 video hosts, format pick, 30d auto-update) | Done | `src/ytdlp.rs`; native fallback in `stream.rs` |
| Metalink v3/v4 (first file, mirrors + SHA-256) | Done | `src/metalink.rs`, `stream.rs`, `tests/metalink.rs` |
| WebDAV (`webdav://`/`webdavs://` scheme map, Add dialog + capture ingress) | Done | `app::normalize_webdav` |
| SFTP (russh, TOFU known-hosts, key/password, seek resume) | Done | `src/sftp.rs` |
| FTPS explicit `ftps://` + implicit `ftpsi://` (TLS data, resume; no live TLS fixture) | Done | `src/ftp.rs` |
| NNTP/NZB (yEnc + CRC32, TLS, fake-server e2e; PAR2/unpack/per-segment resume deferred) | Done | `src/nntp.rs`, `tests/nntp.rs` |
| BitTorrent magnet + .torrent (librqbit, persistence, seed-after-complete opt-in) | Done | `src/torrent.rs`, `model::Settings` torrent fields |
| HLS AES-128 decrypt (plain key over HTTP, not DRM) | Done | `hls::decrypt_segment` + `stream` encrypted temp-parts path; unit vectors + `tests/hls_aes.rs` e2e (sequence IV, explicit IV); SAMPLE-AES/other methods → Protected; RE round 5: IDM manual-add saves playlist plain (KEY never fetched), browser-panel path unverified |
| DASH nested BaseURL, `$Number%05d$`/`$RepresentationID$` | Done | unit tests |
| DASH multi-Representation A/V mux | Done (ffmpeg when present) | e2e with fake segs concat; ffmpeg mux fn |
| DASH multi-Period order | Done | unit test |
| DASH dynamic/live | Done (poll model) | dynamic fixture e2e; RE round 5: IDM manual-add saves MPD as plain file, no poll; browser-panel path unverified (see RE doc) |
| DASH ContentProtection refusal | Done | e2e |
| HLS variants, audio groups, EXT-X-MAP, BYTERANGE, live, EXT-X-KEY | Done | hls unit + e2e; AES-128 decrypted (`tests/hls_aes.rs`), SAMPLE-AES→Protected |
| DASH subtitle AdaptationSets | Done | Sidecar file beside final media (`dash_subtitle_sidecar_and_media_cache_resume`); REA round 6 |
| DASH kill-resume segment cache | Done | `seg-<kind>-<i>.bin` replay, manifest snapshot before fetch (same e2e) |
| Content-Type-only manifest detection (`video/vnd.mpeg.dash.mpd`, `application/vnd.apple.mpegurl`) | Done | `stream::probe_manifest` (REA round 6 MIME map) |
| Browser ext Chromium MV3 + Firefox, native host, per-OS registration paths | Done | extensions/, `platform::native_host_manifest_paths` unit |
| Video panel (ranked candidates, top preselected) | Done | content.js panel + `rank.js`; quality labels (WxH) not surfaced |
| Queues scheduler start/stop, recurrence, missed policy flag, next-run | Done | scheduler unit + GUI |
| Per-queue completion actions incl. power (opt-in flag field) | Done | `run_completion`, power cmd construction unit |
| Site Grabber: depth/scope/filters/dedupe/robots/delay/export/project save-load | Done | grabber e2e + GUI |
| Mirror mode | Done | mirror crawl+download wired in GUI; relative links resolved against document `<base href>`; source base stripped, own base injected |
| Tray, close-to-tray, start-minimized, single hidden autostart (`--autostart`) | Done | tray-icon created + menu Show/Add/Quit wired into egui loop; autostart per-OS; single-instance pid lockfile (stale = dead pid check, forwards URL args via inbox) |
| Protocol `webcraft://` (legacy `downloadcraft://` accepted) | Done | platform register + arg parsing |
| `--smoke-test` | Done | main + CI step |
| Installers Win/macOS/Linux | Done (CI artifacts) | workflow + packaging scripts; NSIS/dmg/AppImage when tools present, zip/tar/deb fallbacks always |

Known limits: Safari extension (needs Xcode signing) — not attempted, does not block. GNOME tray needs AppIndicator extension on some distros. Unsigned macOS builds need right-click → Open. NSIS/AppImage only when respective tools exist in CI runner image (best-effort steps); IDM live-DASH panel behavior unverified without live fixture (standards-based polling implemented).
