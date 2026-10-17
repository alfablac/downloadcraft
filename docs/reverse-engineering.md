# Reverse engineering log — IDM 6.43 (clean-room, observable behavior only)

## RE round 3 — registry record schema + Configuration tabs (2026-10-16)

Registry evidence (`HKCU\Software\DownloadManager`, value NAMES/types only, no values read):

- Per-download records = numeric subkeys (177 present; key = download ID; `maxID` counter key). Fields include: `LocalFileName`, `LocalPath`, `LogFileName`, `Host`, `Path`, `FileName`, `User`, `EncPassword` (binary), `UA`, `Referer`, `Port`, `categoryID`, `cFlags`, `owWPage` (parent web page), `Url0` (primary URL; suffix-0 implies mirror list `Url1…`), `U0_u`/`U0_EncP` (per-URL credentials), `CstmHdrs` (custom headers), `AccH`/`OrgAccEncH` (accept headers), `FRFileSize`/`FileSize` (binary u64), `LastModified`, `dateAdded`, `lastTryDate`, `Status` (DWORD), `queueID` (DWORD), `TPswitch`, `bRetAfFR`... Clone maps: DownloadTask {url + (new) mirrors, username/password per mirror, headers, referrer, category, queue, status, size, dates} — see model.rs additions.
- `ListSettings` binary blobs per column (`FileName, DateAdded, Queue, Size, Status, Timeleft, TransferRate, LastTry, Description, SaveTo, Referer, Parent_wp, Order639`) = persisted column order/width — confirms UIA column set exactly.
- `FoldersTree\{Compressed,Documents,Music,Programs,Video}` = {`ID`, `mask` (extension mask), `pathW`, `rememberLastPath`, `forSiteOnly`, `sites`} — category definition shape.
- `Scheduler` = {`isLimitEnabled`, `m_MBytes`, `m_hours`, `showLimitExceededWarning`} — quota scheduler (stop after X MB per Y hours) separate from time schedules. Clone: quota fields on Schedule (planned).
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

Source machine: Windows, `C:\Program Files (x86)\Internet Download Manager`. No proprietary code/binaries copied. Extension `manifest.json` files read from `.crx`/`.xpi` containers; behavior inferred from manifests, file layout, registry keys, `DwnlData` segment layout, and public UI surface. No decompilation of IDMan.exe logic beyond file/registry observation.

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

Clone extension design (own code): host name `com.idmcraft.host`, inbox-file drop (`%config%/idmcraft/inbox/msg-*.txt` key=value: url/filename/referrer/cookies), message `{type:"download",url,referrer,cookies,filename}`, `{type:"ping"}→{type:"pong"}`. One codebase per engine family (Chromium MV3, Firefox MV2).

**RE round 2 (2026-10-16):** IDM binary/runtime evidence deepened: extension JS opcode protocol (persistent native port, ping ticker, `x-idm-*` header injection mirroring IDMan, `idmreg:`/`idmdwnl:` schemes), LNG UI string tables (menus/columns/dialogs), PE imports (WinINet/WinSock in IDMan.exe; VMPrs = bundled dash parser), IDMan internal schemes (`mpdtoidm://`, `hlstoidm://`, `idmhlstomp4://`), UIA main-window dump (menus Tasks/File/Downloads/View/Help/Registration; category tree All/Compressed/Documents/Music/Programs/Video/Unfinished/Finished/Grabber projects/Queues; columns File Name/Q/Size/Status/Time left/Transfer rate/Last Try Date/Description/Date Added/Save To/Referer/Parent web page), DwnlData logs (8 parts typical per multi-part download). Clone GUI rebuilt to this surface with modern dark design system (design-system/idmcraft/MASTER.md). IDM live-DASH panel behavior remains unverified (3 documented attempts); clone keeps standards-based DASH.

**Release infra note:** tag run `38992400781` failed at Release step on all OS (softprops/action-gh-release). Workflow now uploads via `gh release upload --clobber` (creates release if missing).

## DwnlData segment model (observed)

Per-download folder `<sanitized-host+path>_<numeric id>` containing sequential part files with no extension (`<name>`, `<name>1`…), plus `log_<id>.log` with per-connection trace (DNS, TLS handshake, HTTP status, headers). Observed parts: up to 9+ parts, sizes independently grown. Clone: `.idmcraft-<id>/part{i}` + `state.json` (url/etag/total/segments) — same model, own format.

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
| 15 | Resume after kill | `DwnlData` parts persist across sessions (observed leftover part folders) | Manifest-free: `.idmcraft-media-<id>` workdir + HTTP `state.json` resume for file downloads; MPD snapshot `tracks.json` written | Implemented (HTTP resume e2e) |
| 16 | Temp segment naming | `<name>`, `<name>1`… extensionless + `log_<id>.log` | `.idmcraft-<id>/part{i}` | Own naming (clean-room) |
| 17 | Assembly: fMP4 init+media concat vs remux | No ffmpeg.exe in install → internal `idmvconv.dll` likely. Clone: concat per track; cross-track via ffmpeg sidecar on PATH/beside exe (ADR-002) | Concat + optional ffmpeg mux | Implemented |
| 18 | DRM ContentProtection | IDM cannot play DRM content (public limitation) | Detect `ContentProtection`, refuse before segment fetch, `Protected` status | Implemented + e2e |
| 19 | Reference comparison (ffprobe) | CI generates ffmpeg testsrc fixture, downloads via engine, ffprobe would verify streams when muxer present. Local fake-segment tests assert byte concat only | CI dash test uses real ffmpeg-generated media when available | Partial (concat asserted) |

## 1b. HLS findings

Master: BANDWIDTH/RESOLUTION/CODECS/AUDIO/SUBTITLES groups parsed; variant = highest bandwidth or `x-variant-index`. Media: `EXT-X-MAP` init, `EXT-X-BYTERANGE` (incl. MAP range), `EXT-X-DISCONTINUITY` tolerated (concat order preserved), live = no `EXT-X-ENDLIST` → poll at `targetduration/2`, dedupe, stop at ENDLIST/cancel. TS/fMP4: byte concat (TS concat is playable; fMP4 init+segments concat is standard). `EXT-X-KEY` (non-NONE) → `Protected` refusal. No DRM circumvention.

## Feature inventory from IDM UI/docs (observable)

Main window table (file/size/status/speed), categories (Compressed/Video/Audio/Documents/Programs/General), queues + Scheduler (`Scheduler\q_*.dt,s_*.dt`), Site Grabber (`Grabber\Projects\*.igp`, GrabberData), Options (connections=8 default, proxy, clipboard monitor, startup `/onboot`, tray), download panel in browser, "Download all links". All mapped in `docs/parity.md`.

## ADR-001: GUI framework
egui/eframe (glow) chosen over Tauri. Tauri would need Node + WebView2/WKWebView/WebKitGTK packaging complexity and violates "fewest deps" here; egui gives single Rust binary on Win/macOS/Linux, native tray via `tray-icon`. Trade-off: less HTML/CSS polish. Recorded default per spec §2 ("GUI: egui" listed first). UI design tokens from ui-ux-pro-max search (dark slate `#0F172A`, primary `#1E3A5F`, accent `#059669`) approximated in egui default dark theme; full token theming skipped — ponytail: add when styling pass requested.

## ADR-002: Muxing
Bundled/internal muxer in IDM not reproducible clean-room. Clone: pure concat for single-track fMP4/TS; multi-track uses ffmpeg if on PATH or beside exe (user-installed, never downloaded at runtime; license = user's ffmpeg). Fallback keeps first track + records error. Upgrade path: bundle LGPL ffmpeg per-OS in installer with license file.

## ADR-003: Tray/autostart/protocol per OS
Single `platform.rs`: Windows HKCU Run + registry native-host + protocol; macOS LaunchAgent plist + `~/Library/Application Support/.../NativeMessagingHosts` + Info.plist URL scheme; Linux XDG autostart `.desktop` + `~/.config/.../NativeMessagingHosts` + `x-scheme-handler` desktop entry. Paths unit-tested with mocked home.
