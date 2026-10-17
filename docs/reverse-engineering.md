# Reverse engineering log — IDM 6.43 (clean-room, observable behavior only)

## RE round 3 — registry record schema + Configuration tabs (2026-10-16)

Registry evidence (`HKCU\Software\DownloadManager`, value NAMES/types only, no values read):

- Per-download records = numeric subkeys (177 present; key = download ID; `maxID` counter key). Fields include: `LocalFileName`, `LocalPath`, `LogFileName`, `Host`, `Path`, `FileName`, `User`, `EncPassword` (binary), `UA`, `Referer`, `Port`, `categoryID`, `cFlags`, `owWPage` (parent web page), `Url0` (primary URL; suffix-0 implies mirror list `Url1…`), `U0_u`/`U0_EncP` (per-URL credentials), `CstmHdrs` (custom headers), `AccH`/`OrgAccEncH` (accept headers), `FRFileSize`/`FileSize` (binary u64), `LastModified`, `dateAdded`, `lastTryDate`, `Status` (DWORD), `queueID` (DWORD), `TPswitch`, `bRetAfFR`... Clone maps: DownloadTask {url + (new) mirrors, username/password per mirror, headers, referrer, category, queue, status, size, dates} — see model.rs additions.
- `ListSettings` binary blobs per column (`FileName, DateAdded, Queue, Size, Status, Timeleft, TransferRate, LastTry, Description, SaveTo, Referer, Parent_wp, Order639`) = persisted column order/width — confirms UIA column set exactly.
- `FoldersTree\{Compressed,Documents,Music,Programs,Video}` = {`ID`, `mask` (extension mask), `pathW`, `rememberLastPath`, `forSiteOnly`, `sites`} — category definition shape.
- `Scheduler` = {`isLimitEnabled`, `m_MBytes`, `m_hours`, `showLimitExceededWarning`} — quota scheduler (stop after X MB per Y hours) separate from time schedules. Clone: quota fields on Schedule (done — `quota_mb`/`quota_hours`, gate in scheduler).
- `DwnlPanel` = DWORD per extension (FLV MP3 MP4 M4V F4V M4A MPG MPEG AVI WMV WMA WAV ASF RM OGG OGV MOV 3GP QT WEBM TS MKV AAC VTT TTML TTML2 DFXP SRT) + `PanelView`, `DonlCaptWP` — video download panel shown per file type; subtitles types included. Clone: panel per type incl. subtitles.
- `Queue` = single REG_SZ blob (serialized queues). `GrabberSts` = {bDontShowCnfNameDlg, maxProjectID}.

Runtime evidence (pywinauto win32, IDM 6.43.15.3): **Configuration dialog has 9 tab pages** (tab texts custom-drawn; identified by page content + LNG sections):

1. **General** — Launch on startup; clipboard URL auto-start; IE-based browser monitoring; capture-from browser list + Add browser; force/prevent-download keys; context-menu items editor; download panels customizer; advanced integration status.
2. **File types** — automatically-start file types list (+Default); excluded sites; excluded addresses (+Edit list).
3. **Save To / Categories** — category combo (General…), extension-mask edit, per-category default dir + Browse, change-folder-on-last-selected, set file date from server, temporary directory.
4. **Downloads** — customize progress dialog; show start-download dialog vs add-only-to-queue; show complete dialog; start immediately while showing File Info; queue-selection panels on “Download Later”/batch close; ignore file modification time on resume; duplicate-link action combo; User-Agent for manual downloads; virus-check settings.
5. **Connection** — per-site “Max connections number” exceptions list (New/Delete/Edit); download limits quota (X MBytes every Y hours + warning toggle); default max conn. number = 8; connections limits; TLS 1.3 option.
6. **Proxy** — get system / none / system / PAC script / manual; address+port+user+password; per-protocol use (http/https/ftp); Advanced/SOCKS; bypass list; FTP PASV.
7. **Site logins (Passwords)** — per-server credential list.
8. **Dial-Up / VPN** — Windows dial-up/VPN connection, user/password, redial attempts/interval.
9. **Sounds** — per-event sound files (list + Browse/Play).

Clone Configuration UI restructured to property-sheet sections mirroring this map (General/File types/Save to/Downloads/Connection/Proxy/Browser/About); Dial-Up/VPN + Sounds listed As-Not-Cloned with reason (obsolete / OS-default).

## RE round 4 — Add URL / Scheduler / Grabber dialogs (pywinauto, live)

- **Add new download** (`Enter new address to download`): `Address` field + `Use &authorization` checkbox + `Login`/`Password` + OK/Cancel. Then Download File Info (Save As) follows. Clone Add dialog now mirrors: URL, Save to (settings dir), Referrer/Cookies, Mirrors, Description, Basic auth login/password.
- **Scheduler dialog**: queue list + New queue/Delete/Start now/Stop; per-queue: One-time downloading | Periodic synchronization; Start download at (time picker); Start again every (h/min); weekday checkboxes Sun–Sat; Once at (date); Stop download at; Number retries for each file (default 10); Open file when done (path+`...`); Hang up modem when done; Exit IDM when done; Turn off computer when done (+Shut down action combo: Shut down/Hibernate/…); Force processes to terminate; Start download on IDM startup; Download limits quota (2000 MBytes every 5 hours + warning toggle — matches Scheduler registry quota). Clone: Schedule {start/stop, retries=open?, on-complete actions subset} + new quota_mb/quota_hours fields.
- **Site Grabber wizard Step 1**: Explore whole site | N link levels within base site + M levels on other sites; Ignore popup windows; Don't explore parent directories; Explore all sites within main domain; Process JavaScript (risk note); only/exclude paths-domains lists (`;`-separated); Grabber Project Name (`project_4` auto); Start page/address; Project template combo (Custom settings); Use authorization Login/Password + Advanced manual login / don't-open-logout-page; file filter combos (All Files *.* + Add Filter, Don't use exclude filter); Search files on site only; Hide duplicate files in different locations; Start downloading all matched files at once; Download if file size Not less than / Not more than (KB). Statistics dialog: Web pages processed / Explored / Matched / Downloaded counts (Simple/Advanced). Clone grabber covers: depth, levels within base site + levels on other sites (external_depth, counted per registrable domain), scope, don't-explore-parent-dirs (outside-dir pages fetched once as possible file endpoints, links never expanded), only/exclude paths-domains (`;`-separated, GUI + project fields), extension/MIME include/exclude/size filters, content-type discovery for extensionless endpoints, static script/plain-text absolute URL + CSS `url(...)` discovery, bounded parallel HEAD probes (8), dedupe, project save/load. Executed-JS processing + login-form walk not cloned (documented).

Source machine: Windows, `C:\Program Files (x86)\Internet Download Manager`. No proprietary code/binaries copied. Extension `manifest.json` files read from `.crx`/`.xpi` containers; behavior inferred from manifests, file layout, registry keys, `DwnlData` segment layout, and public UI surface. No decompilation of IDMan.exe logic beyond file/registry observation.

## RE round 5 — live fixtures (2026-10-17, loopback only)

Method: local fixtures under `%TEMP%\opencode\idm-fixtures\` (`server.py` ThreadingHTTPServer, Range-capable, request log `log.txt`; `drive.py` pywinauto win32). IDMan 6.43.15.3 driven via documented CLI `IDMan.exe /d <url>` (same backend as Add URL) + Start Download button in Download File Info; each fixture ~20 s observation; Downloads/DwnlData/registry probed names+counts only, values redacted/not read. Baseline control `file.bin` (3 MB): multiple Range GETs, saved `Downloads\file.bin` 3145728 B, "Download complete 3,00 MB" dialog — drive method valid.

| Fixture | Observation | Inference | Unknown |
|---|---|---|---|
| `dash/vod.mpd` (static, control) via `/d` | 1 GET `/dash/vod.mpd`, 0 segment GETs; saved `Downloads\vod.mpd` 546 B as plain file; Download complete dialog | Manual add does NOT parse MPD; treats manifest as ordinary file | Panel/browser-capture path |
| `dash/live.mpd` (dynamic, `minimumUpdatePeriod=PT2S`) via `/d` | 1 GET in 20 s (total live GET count incl. curl smoke = 2), 0 segment GETs, no repeat poll; saved 576 B plain file | Manual add does NOT poll dynamic MPD, does NOT assemble segments. Clone poll+assemble model not contradicted via this entry path, but not confirmed either | Whether browser download-panel path (`mpdtoidm://` internal message, round 2) polls/assembles live, selects live-edge, or refuses |
| `mpdtoidm://…` / `hlstoidm://…` as CLI arg; schemes in registry | Main window opened only; 0 GETs; schemes absent from HKCU/HKCR Classes (names checked) | Schemes = internal extension→native-host messages, not OS protocol handlers; not reachable via CLI | Exact native-host payload format (not probed; would need browser+extension session) |
| `hls/aes/playlist.m3u8` (`EXT-X-KEY METHOD=AES-128`, local key) via `/d` | 1 GET playlist only; 0 GET key/segments; saved 180 B plain file | Manual add does NOT parse HLS, KEY never fetched/decrypted via this path | Panel path (`hlstoidm://`) AES-128 handling |
| `hls/sample/playlist.m3u8` (`METHOD=SAMPLE-AES`) via `/d` | 1 GET playlist only; 0 GET fake key/segments; saved 184 B plain file; no error dialog | Manual add does NOT evaluate EXT-X-KEY at all via this path | Panel path SAMPLE-AES refusal/acceptance |
| Registry + post-run state | `HKCU\Software\DownloadManager` 74 value names, none MPD/HLS/DASH-specific (only `PanelExceptionServers` matched); fixture tasks appeared in main list as plain filenames (`live.mpd`, `vod.mpd`, `playlist.m3u8`) and were deleted after | No persisted MPD/HLS config evidence | — |

Parity recommendation (A) live/dynamic DASH: **keep clone behavior** (poll `minimumUpdatePeriod`, dedupe, assemble) — IDM manual-add treats MPD as plain file, panel path Unverified; do not claim IDM parity. (B) HLS EXT-X-KEY: **keep clone behavior** (decrypt AES-128, refuse SAMPLE-AES → `Protected`) — same reasoning; IDM AES/SAMPLE-AES handling Unverified, not observed. Anything panel-path = **Unverified — do not guess IDM internals.**

## RE round 7 - IDM video panel mechanics (2026-10-17, REA agent)

Tags: [REA Evidence ...] = REA analyze_javascript_application records; [strings] = plain shell string scans (not REA Evidence). IDMan.exe 6.43.15.3 sha256 661e78ea..., native not decompiled (no Ghidra/Hopper).

- Content detection [REA ev_1ea020b0]: content script on *://*/* all_frames document_start enumerates video,audio,object,embed; MutationObserver (attributes style + parent childList), ResizeObserver, IntersectionObserver(0.2); 200ms debounce re-report; size gate width<15 or height<10 rejected; anchor/poster pairing retries 10x500ms.
- Page-context sniffing [REA ev_1ea020b0]: document.js injected into page wraps fetch + XMLHttpRequest.open, tags matches (.pfm-/.pxr- ids), posts to content script; also parses inline <script> player configs. This is how blob/MSE players (no src) still surface URLs.
- Network sniffing [REA ev_1ea020b0]: webRequest onBeforeRequest/onBeforeSendHeaders/onHeadersReceived/onResponseStarted; classifier scores extension/Content-Type (M3U8|F4M|MPD stream, F4F fragments, M4S DASH segment); status 200/206/304; verdicts consulted with native host per request.
- Site modes [REA ev_1ea020b0]: modes A=1..5 largely pushed from IDMan at runtime (init opcode 11), not hardcoded; shipped defaults: Facebook selectors, Vimeo id regex, YouTube inline-script extraction; hostname site classifier fed by native table (site 1 = youtube). Unknown hosts -> site 0 generic.
- Flow [REA ev_1ea020b0]: content port ops [21] page, [41] media register, [23] update, [24] panel meta; background -> native type 23 element, type 9 rect (native draws/moves panel), type 14 link download. Transport: WebSocket ws://127.0.0.1:1001 fallback connectNative com.tonec.idm, custom framed records. Clone keeps own JSON native messaging (wire format internal to IDM, not cloned).
- Native [strings]: scheme cluster yta*/mpda*/mpd*/hls*/dhs*toidm:// plus googlevideo/videoplayback; CM3UParser + CMp4DashReader classes (no CMDP parser string - bounded negative); DwnlPanel registry Software\DownloadManager\DwnlPanel minsize/defminsizeset, "Do not show Download this video button if media file size is below ... kilobytes" (native-side gate); protected-stream refusal string. No ffmpeg string.
- Reddit [strings]: zero hits reddit/v.redd.it/redd.it (ROT13 erqqvg too) in all 4 extension containers + IDMan.exe ASCII/UTF-16. v.redd.it rides generic paths only; runtime-pushed native site rule cannot be excluded but nothing Reddit-specific ships statically.
- Clone actions round 7: button no longer gated on direct URLs (blob/MSE), discreet 30px draggable glyph, click resolves sniffed streams (empty -> guidance note); sniffers accept video/*, audio/*, .mpd/.m3u8 path regardless Content-Type; Firefox webRequest adds media type. Page-context fetch/XHR wrapping (document.js equivalent) NOT cloned - follow-up gap if background sniffing alone misses a player.

## RE round 6 — REA on shipped extension containers + IDMan.exe (2026-10-17)

Tool: `rea-agents@6.5.0` CLI (MCP registration for opencode `aligned` per `doctor`, skill v42; REA MCP tools absent from this session — client restart required; CLI fallback used, per skill). Attempt 1: default npx cache corrupt (`ERR_MODULE_NOT_FOUND AbortablePromise-*.js`). Attempt 2: fresh `npm_config_cache` → success.

- **REA Evidence** `ev_1ea020b0b313244e7d11eaaa0de66a589030f990e3438a6c18a62b46f4a0dabd` (`analyze_javascript_application` on extracted `IDMGCExt.crx`): 27 sources inventoried/parsed; graph confirms `connectNative("com.tonec.idm")`, 6 `onBeforeRequest` sites. Limitations: static AST only, dynamic expressions unresolved, no runtime execution. Full record: `%TEMP%\opencode\idm-rea\evidence-chrome.json` (38 MB, not committed).
- **CRX v3 containers** (header = 12 + header_len@8): `IDMGCExt.crx` MV3 `6.43.1`, `minimum_chrome_version 109`, service worker, permissions incl `scripting,declarativeNetRequest,downloads*`; `IDMEdgeExt.crx` MV2 `6.43.1` persistent background; `idmmzcc3.xpi` MV2 `6.43.1` gecko `mozilla_cc3@internetdownloadmanager.com`, signed META-INF present; `IDMOpExt.nex` MV2 `6.41.8`. All: content script `all_frames:true, run_at:document_start`, matches http/https/ftp/file.
- **Native protocol shape (observed literals only, no code copied):** persistent port, numeric opcode message arrays (`[18,...]` connect, `[15,...]`/`[16,...]` relay, `[21,url,referrer...]` page report, `[41,...]` media-element report, `[45,...]` script report), `?d=` → internal `idmreg:` handoff, `x-idm-*` request-header inject/strip (`x-idm-status`, `x-idm-request-id` in Edge/FF builds), 1.5 s reconnect timer. Clone keeps own JSON `{type,url,referrer,cookies,filename}` — wire format is IDM-internal, not cloned.
- **MIME→extension map (observed):** `video/vnd.mpeg.dash.mpd→MPD`, `application/vnd.apple.mpegurl`/`video/mpegurl`/`application/x-mpegurl→M3U8`, `text/plain→…VTT|SRT|M3U8|MPD…`. Clone applied: `probe_manifest` now accepts `dash`/`mpd` + `mpegurl`/`m3u` substrings; extension sniff sets already covered both families.
- **Installed-state wiring (observed):** Chrome profile `Default/Extensions/ngpampappnmepgilojfohadhhmbhlaek/6.43.1_0` (manifest carries store `key`, differs from CRX-extracted hash — signed store copy), Edge `llbjbkhnmlidjebalopleeepgdfgcpec` MV2 `6.43.1`, Firefox profile XPI present. Registry `com.tonec.idm` → Chrome/Edge `IDMMsgHost.json`, Mozilla `IDMMsgHostMoz.json`; Brave host key **missing** on this machine (IDM registered Chrome/Edge/Mozilla only here).
- **IDMan.exe 6.43.15.3**: `Status: NotSigned` (Get-AuthenticodeSignature) — unsigned build on this machine; no inference drawn beyond that.
- **Clone actions from round 6:** extension content scripts aligned `document_start`+`all_frames` (both engines); DASH subtitle AdaptationSets → sidecar file (parity table row 13 was Partial); media segment cache = kill-resume source; drag-and-drop URL/`.url` drop onto main window.

## Install inventory (observed)

| Item | Evidence |
|---|---|
| Version | `HKCU\Software\DownloadManager`, `LstCheck`, extension manifests `6.43.1` (Chrome/Edge/Firefox), Opera `6.41.8` |
| Main exe | `IDMan.exe` (6.1 MB), `IDMGrHlp.exe`, `IEMonitor.exe`, `IDMMsgHost.exe` (39 KB, native host), `IDMIntegrator64.exe`, `MediumILStart.exe`, `idmBroker.exe` |
| Browser DLLs | `downlWithIDM(64).dll`, `idmbrbtn(64).dll`, `idmcchandler2/7(_64).dll`, `idmmzcc7(_64).dll`, `IDMIECC(64).dll`, `IDMGetAll(64).dll`, `IDMShellExt(64).dll` |
| Network drivers | `idmtdi32/64.sys` + `.cat/.inf`, `idmwfp*.sys` (WFP/TDI traffic interception drivers) — clone does **not** use kernel drivers; browser integration via extension + native messaging only |
| Media helpers | `idmvconv.dll`, `IDMVMPrs(64).dll`, `idmvs.dll`. No bundled `ffmpeg.exe` found in install dir. Inference: IDM muxes/converts internally via `idmvconv.dll` or downloads components on demand. Clone decision in ADR-002. |
| Extensions shipped | `IDMGCExt.crx` (Chrome MV3), `IDMEdgeExt.crx` (Edge MV2), `idmmzcc3.xpi` (Firefox MV2), `IDMOpExt.nex` (Opera MV2) |
| Data dir | `%APPDATA%\IDM\`: `DwnlData\<user>\<sanitized-url>_<id>\` segment files (`<name>`, `<name>1`…`<name>9+`, `log_<id>.log`), `Grabber\Projects\*.igp`, `GrabberData\`, `Scheduler\q_1.dt,s_1.dt`, `UrlHistory.txt`, `urlexclist.dat`, `defextmap.dat` |

## Registry (observed, HKCU\Software\DownloadManager)

Key values: `AppDataIDMFolder=%APPDATA%\IDM\`, `TempPath` same, `MaxConnectionsNumber=8`, `LaunchOnStart=1` (+ `HKCU\...\Run\IDMan = IDMan.exe /onboot`), `MonitorUrlClipboard=0`, proxy flags (`UseHttpProxy` etc.), extension exclusion list (`ExceptionServers`), panel exception list, huge default extension list (3GP AAC ACE … ZIP DAT), `ToolbarStyle`, `TrayIcon=1`.

Native messaging: `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.tonec.idm` → `IDMMsgHost.json`; same for Edge. Mozilla → `IDMMsgHostMoz.json`. Both JSONs: `name=com.tonec.idm`, `type=stdio`, Chrome `allowed_origins=[chrome-extension://ngpampappnmepgilojfohadhhmbhlaek/, chrome-extension://llbjbkhnmlidjebalopleeepgdfgcpec/]`, Firefox `allowed_extensions=[mozilla_cc3@internetdownloadmanager.com]`, `path=IDMMsgHost.exe` (relative).

## Extension findings

Chrome MV3 (`IDMGCExt.crx`, v6.43.1): `background.service_worker=background.js`, `host_permissions=<all_urls>`, `web_accessible_resources=[document.js,debug.js,captured.js,captured.html]`, content scripts, `minimum_chrome_version=109`. `background.js` (56 KB, minified): `connectNative("com.tonec.idm")` persistent port, message listener, detects `mpd`/`m3u8` strings. Firefox MV2 (`idmmzcc3.xpi`): permissions `<all_urls>,tabs,cookies,contextMenus,webNavigation,webRequest,webRequestBlocking,management,storage,nativeMessaging,privacy,idle`, gecko id `mozilla_cc3@internetdownloadmanager.com`, content script on http/https/ftp/file at `document_start`. Edge/Opera are MV2 forks of same codebase.

Clone extension design (own code): host name `com.downloadcraft.host`, inbox-file drop (`%config%/downloadcraft/inbox/msg-*.txt` key=value: url/filename/referrer/cookies), message `{type:"download",url,referrer,cookies,filename}`, `{type:"ping"}→{type:"pong"}`. One codebase per engine family (Chromium MV3, Firefox MV2).

**RE round 2 (2026-10-16):** IDM binary/runtime evidence deepened: extension JS opcode protocol (persistent native port, ping ticker, `x-idm-*` header injection mirroring IDMan, `idmreg:`/`idmdwnl:` schemes), LNG UI string tables (menus/columns/dialogs), PE imports (WinINet/WinSock in IDMan.exe; VMPrs = bundled dash parser), IDMan internal schemes (`mpdtoidm://`, `hlstoidm://`, `idmhlstomp4://`), UIA main-window dump (menus Tasks/File/Downloads/View/Help/Registration; category tree All/Compressed/Documents/Music/Programs/Video/Unfinished/Finished/Grabber projects/Queues; columns File Name/Q/Size/Status/Time left/Transfer rate/Last Try Date/Description/Date Added/Save To/Referer/Parent web page), DwnlData logs (8 parts typical per multi-part download). Clone GUI rebuilt to this surface with modern dark design system (design-system/downloadcraft/MASTER.md). IDM live-DASH panel behavior remains unverified (3 documented attempts); clone keeps standards-based DASH.

**Release infra note:** tag run `38992400781` failed at Release step on all OS (softprops/action-gh-release). Workflow now uploads via `gh release upload --clobber` (creates release if missing).

## DwnlData segment model (observed)

Per-download folder `<sanitized-host+path>_<numeric id>` containing sequential part files with no extension (`<name>`, `<name>1`…), plus `log_<id>.log` with per-connection trace (DNS, TLS handshake, HTTP status, headers). Observed parts: up to 9+ parts, sizes independently grown. Clone: `.downloadcraft-<id>/part{i}` + `state.json` (url/etag/total/segments) — same model, own format.

## 1a. DASH experiment log

**Method limitation (documented attempt 1):** Live IDM-side DASH capture was not executed against instrumented browser session in this environment (IDM closed-source GUI automation + driver interception not scriptable here without manual clicks). Table below separates **observed** (manifest/file evidence) from **standards-based inference** (DASH-IF IOP). Clone implements the inference column; where IDM behavior is unverified, entry says so explicitly. Attempt 2: registry/DwnlData search found no MPD-specific keys or cached manifests under `%APPDATA%\IDM` — IDM does not persist MPD snapshots visibly there. Attempt 3: extension `background.js` string scan confirms `mpd`/`m3u8` sniffing in extension, so detection is at least partly network/page sniff in browser.

| # | Experiment | IDM observed behavior | Clone required behavior | Status |
|---|---|---|---|---|
| 1 | Detection by extension | Extension searches `mpd`/`m3u8` in page/network context (background.js contains both literals; content/document scripts injected on all pages) | Extension scans media elements + page source for `.mpd`/`.m3u8`, video panel offers download | Implemented |
| 2 | Detection by Content-Type `application/dash+xml` | Unverified live; extension host_permissions `<all_urls>` + webRequest (Firefox) suggests header sniffing | Clone dispatches on URL `.mpd` and treats fetch in engine; panel scan covers CT-only case via page scan | Inference |
| 3 | MPD request headers | Unverified; Firefox ext has cookies permission → cookies forwarded to host | Forward cookies/referrer/UA headers from task to MPD + segments | Implemented |
| 4 | Video panel quality list | IDM panel (documented IDM UI) lists qualities per stream | Label = `WxH codec N kbps fps lang` via `mpd::rep_label` | Implemented (label fn unit-tested via parse fields) |
| 5 | Default selection | Unverified; IDM typically picks highest quality | Highest bandwidth video + highest audio | Implemented |
| 6 | SegmentTemplate `$Number%05d$`, `$RepresentationID$`, `$Time$`, `$Bandwidth$` | Standard DASH; IDM downloads these streams in practice (public IDM DASH support) | Expand all, zero-pad, tested | Implemented + unit tests |
| 7 | SegmentTimeline (`S@t/d/r`, incl `r=-1`) | Standard | Expand repeats; `-1` bounded by period duration | Implemented |
| 8 | SegmentList / SegmentURL + Initialization | Standard | Resolve, concat init+media | Implemented + e2e test |
| 9 | SegmentBase (single file + Initialization range) | Standard | Single URL segment; index_range recorded | Parse unit-tested |
| 10 | Nested/multiple BaseURL (MPD→Period→AS→Rep), CDN switch | Standard RFC3986 chain | Chain-join sequentially | Unit-tested |
| 11 | Multi-Period sequencing | Standard | Sets concatenated in document order; same-kind tracks merged in order | Unit-tested |
| 12 | Separate audio/video AdaptationSets | IDM produces single playable file (user-visible behavior) | Download both tracks, ffmpeg `-c copy` mux if present, else error+keep first track | Implemented |
| 13 | Subtitle AdaptationSets | Unverified | Downloaded as `text` track; mux attempts include it; sidecar not guaranteed | Partial |
| 14 | Live/dynamic MPD | **Unverified in IDM.** No evidence found in registry/files. Clone polls `minimumUpdatePeriod`, dedupes segments, assembles on stall/cancel. If IDM refuses live, divergence is documented here rather than blocking. | Poll + assemble | Implemented (fixture test) |
| 15 | Resume after kill | `DwnlData` parts persist across sessions (observed leftover part folders) | Manifest-free: `.downloadcraft-media-<id>` workdir + HTTP `state.json` resume for file downloads; MPD snapshot `tracks.json` written | Implemented (HTTP resume e2e) |
| 16 | Temp segment naming | `<name>`, `<name>1`… extensionless + `log_<id>.log` | `.downloadcraft-<id>/part{i}` | Own naming (clean-room) |
| 17 | Assembly: fMP4 init+media concat vs remux | No ffmpeg.exe in install → internal `idmvconv.dll` likely. Clone: concat per track; cross-track via ffmpeg sidecar on PATH/beside exe (ADR-002) | Concat + optional ffmpeg mux | Implemented |
| 18 | DRM ContentProtection | IDM cannot play DRM content (public limitation) | Detect `ContentProtection`, refuse before segment fetch, `Protected` status | Implemented + e2e |
| 19 | Reference comparison (ffprobe) | CI generates ffmpeg testsrc fixture, downloads via engine, ffprobe would verify streams when muxer present. Local fake-segment tests assert byte concat only | CI dash test uses real ffmpeg-generated media when available | Partial (concat asserted) |

## 1b. HLS findings

Master: BANDWIDTH/RESOLUTION/CODECS/AUDIO/SUBTITLES groups parsed; variant = highest bandwidth or `x-variant-index`. Media: `EXT-X-MAP` init, `EXT-X-BYTERANGE` (incl. MAP range), `EXT-X-DISCONTINUITY` tolerated (concat order preserved), live = no `EXT-X-ENDLIST` → poll at `targetduration/2`, dedupe, stop at ENDLIST/cancel. TS/fMP4: byte concat (TS concat is playable; fMP4 init+segments concat is standard). `EXT-X-KEY METHOD=AES-128` (plain key over HTTP, not DRM) → fetched + CBC-decrypted (`tests/hls_aes.rs`); `SAMPLE-AES`/other methods → `Protected` refusal. No DRM circumvention.

## Feature inventory from IDM UI/docs (observable)

Main window table (file/size/status/speed), categories (Compressed/Video/Audio/Documents/Programs/General), queues + Scheduler (`Scheduler\q_*.dt,s_*.dt`), Site Grabber (`Grabber\Projects\*.igp`, GrabberData), Options (connections=8 default, proxy, clipboard monitor, startup `/onboot`, tray), download panel in browser, "Download all links". All mapped in `docs/parity.md`.

## ADR-001: GUI framework
egui/eframe (glow) chosen over Tauri. Tauri would need Node + WebView2/WKWebView/WebKitGTK packaging complexity and violates "fewest deps" here; egui gives single Rust binary on Win/macOS/Linux, native tray via `tray-icon`. Trade-off: less HTML/CSS polish. Recorded default per spec §2 ("GUI: egui" listed first). UI design tokens from ui-ux-pro-max search (dark slate `#0F172A`, primary `#1E3A5F`, accent `#059669`) approximated in egui default dark theme; full token theming skipped — ponytail: add when styling pass requested.

## ADR-002: Muxing
Bundled/internal muxer in IDM not reproducible clean-room. Clone: pure concat for single-track fMP4/TS; multi-track uses ffmpeg if on PATH or beside exe (user-installed, never downloaded at runtime; license = user's ffmpeg). Fallback keeps first track + records error. Upgrade path: bundle LGPL ffmpeg per-OS in installer with license file.

## ADR-003: Tray/autostart/protocol per OS
Single `platform.rs`: Windows HKCU Run + registry native-host + protocol; macOS LaunchAgent plist + `~/Library/Application Support/.../NativeMessagingHosts` + Info.plist URL scheme; Linux XDG autostart `.desktop` + `~/.config/.../NativeMessagingHosts` + `x-scheme-handler` desktop entry. Paths unit-tested with mocked home.
