# Feature parity checklist

| IDM feature | Clone status | Evidence |
|---|---|---|
| Segmented multi-connection HTTP download (default 8) | Done | `engine::plan_segments`, e2e `http_segmented_download_and_resume` |
| Resume / pause / crash recovery, ETag validation | Done | `.idmcraft-<id>/state.json`, preseeded-resume e2e |
| Retry with backoff (engine), queue retry policy | Done | engine attempt loop; app queue retry requeue |
| Speed limiter (global + per task) | Done | `Controls.limit_kbps` throttle |
| Proxy + auth (URL form) | Done | `build_client(proxy)` reqwest |
| Checksum SHA-256 verify | Done | `finish_check` |
| Queues: caps, priority, drag order (Up/Down), FIFO order field | Done | `queue::plan_starts` unit + GUI buttons |
| Categories by extension | Done | `model::category_for` |
| Clipboard URL monitor | Done | app poll (arboard), per-OS via crate |
| Drag-and-drop URLs | Partial | Clipboard + inbox + Add box (egui text-drop limited); browser drag via extension |
| Add by URL with cookies/headers/referrer | Done | Add dialog fields + task model |
| DASH SegmentTemplate / Timeline / List / Base | Done | mpd unit + e2e |
| DASH nested BaseURL, `$Number%05d$`/`$RepresentationID$` | Done | unit tests |
| DASH multi-Representation A/V mux | Done (ffmpeg when present) | e2e with fake segs concat; ffmpeg mux fn |
| DASH multi-Period order | Done | unit test |
| DASH dynamic/live | Done (poll model) | dynamic fixture e2e; IDM verdict unverified (see RE doc) |
| DASH ContentProtection refusal | Done | e2e |
| HLS variants, audio groups, EXT-X-MAP, BYTERANGE, live, EXT-X-KEY refusal | Done | hls unit + e2e; KEY→Protected |
| Browser ext Chromium MV3 + Firefox, native host, per-OS registration paths | Done | extensions/, `platform::native_host_manifest_paths` unit |
| Video panel (qualities) | Done | content.js panel + rep_label |
| Queues scheduler start/stop, recurrence, missed policy flag, next-run | Done | scheduler unit + GUI |
| Per-queue completion actions incl. power (opt-in flag field) | Done | `run_completion`, power cmd construction unit |
| Site Grabber: depth/scope/filters/dedupe/robots/delay/export/project save-load | Done | grabber e2e + GUI |
| Mirror mode | Partial | `mirror_rel_path`/`rewrite_links` helpers + unit; full mirror crawl download not wired in GUI |
| Tray, close-to-tray, start-minimized, single hidden autostart (`--autostart`) | Partial | tray-icon created; autostart per-OS done; single-instance mutex not implemented (second launch opens second window) — documented limit |
| Protocol `idmcraft://` | Done | platform register + arg parsing |
| `--smoke-test` | Done | main + CI step |
| Installers Win/macOS/Linux | Done (CI artifacts) | workflow + packaging scripts; NSIS/dmg/AppImage when tools present, zip/tar/deb fallbacks always |

Known limits: Safari extension (needs Xcode signing) — not attempted, does not block. GNOME tray needs AppIndicator extension on some distros. Unsigned macOS builds need right-click → Open. Mirror GUI wiring and single-instance are the two documented partials.
