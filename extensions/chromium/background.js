// webcraft Chromium MV3 background. Native host: com.webcraft.host
try { importScripts("rank.js"); } catch (e) {}
const HOST = "com.webcraft.host";
// Native messaging: single in-flight queue. Host originally replied without
// an id, so sends are serialized and the first reply settles the in-flight
// send. New host echoes `id`; a mismatched id is ignored. 4s timeout rejects.
let nativePort = null;
let nativeNextId = 1;
let nativeInFlight = null;
const nativeQueue = [];
function nativeSend(msg) {
  return new Promise((resolve, reject) => {
    nativeQueue.push({ msg: { ...msg, id: nativeNextId++ }, resolve, reject, timer: null });
    pumpNative();
  });
}
function settleNative(item, err, resp) {
  if (nativeInFlight !== item) return;
  clearTimeout(item.timer);
  nativeInFlight = null;
  if (err) item.reject(err); else item.resolve(resp);
  pumpNative();
}
function pumpNative() {
  if (nativeInFlight || !nativeQueue.length) return;
  const item = nativeQueue.shift();
  nativeInFlight = item;
  item.timer = setTimeout(() => settleNative(item, new Error("native host timeout")), 4000);
  try {
    if (!nativePort) {
      nativePort = chrome.runtime.connectNative(HOST);
      nativePort.onMessage.addListener((m) => {
        const cur = nativeInFlight;
        if (!cur) return;
        if (m && m.id != null && m.id !== cur.msg.id) return;
        settleNative(cur, null, m);
      });
      nativePort.onDisconnect.addListener(() => {
        nativePort = null;
        const cur = nativeInFlight;
        nativeInFlight = null;
        if (cur) { clearTimeout(cur.timer); cur.reject(new Error("native host disconnected")); }
        while (nativeQueue.length) nativeQueue.shift().reject(new Error("native host disconnected"));
      });
    }
    nativePort.postMessage(item.msg);
  } catch (e) {
    // Fallback: one-shot sendNativeMessage
    try {
      chrome.runtime.sendNativeMessage(HOST, item.msg, (resp) => {
        if (chrome.runtime.lastError) settleNative(item, new Error(chrome.runtime.lastError.message));
        else settleNative(item, null, resp || { type: "ok" });
      });
    } catch (e2) { settleNative(item, e2); }
  }
}
// --- Media sniffing (observing webRequest only; no webRequestBlocking) ---
const CANDIDATE_TTL_MS = 5 * 60 * 1000;
const CANDIDATE_MAX = 50;
const candidates = new Map(); // url -> {url, type, tabId, frameId, redirectFrom, time, size}
// IDM model: every in-flight request keyed by requestId. Transient (not
// persisted); on completion/error record dies, candidate (URL index) stays.
// storage.session persists derived candidates only, so SW restart loses
// requestId correlation but keeps toolbar-scan list.
const requestRecords = new Map(); // requestId -> {url, redirectFrom, hops, tabId, frameId, time, type, size, status}
const RECORD_TTL_MS = 10 * 60 * 1000;
const RECORD_MAX = 500;
const MEDIA_PATH = /\.(mpd|m3u8|mp4|webm|ts|m4s)($|[?#])/i;
const MEDIA_TYPES = new Set(["media", "xmlhttprequest", "other", "object"]);
function isMediaIsh(details) {
  // Record only media-ish requests; full <all_urls> recording bloats TTL map
  // with page/API traffic element matcher never needs.
  if (details.type && MEDIA_TYPES.has(details.type)) return true;
  try { return MEDIA_PATH.test(new URL(details.url || "").pathname); } catch (e) { return MEDIA_PATH.test(String(details.url || "")); }
}
function sweepRecords(now = Date.now()) {
  for (const [k, r] of requestRecords) if (r.time && now - r.time > RECORD_TTL_MS) requestRecords.delete(k);
  while (requestRecords.size > RECORD_MAX) requestRecords.delete(requestRecords.keys().next().value); // ponytail: FIFO cap
}
function recordRequest(details) {
  try {
    if (!details || !details.requestId || !(details.tabId >= 0)) return;
    if (!isMediaIsh(details)) return;
    sweepRecords();
    const prev = requestRecords.get(details.requestId);
    if (prev) {
      // Redirect: same requestId re-fires with new url. hops keeps every hop
      // (origin first, cap 8); redirectFrom stays first origin for back-compat.
      if (details.url && details.url !== prev.url) {
        if (!prev.redirectFrom) prev.redirectFrom = prev.url;
        if (!Array.isArray(prev.hops)) prev.hops = prev.redirectFrom ? [prev.redirectFrom, prev.url] : [prev.url];
        if (!prev.hops.includes(details.url) && prev.hops.length < 8) prev.hops.push(details.url);
        prev.url = details.url;
      }
    } else {
      requestRecords.set(details.requestId, { url: details.url || "", redirectFrom: "", hops: details.url ? [details.url] : [], tabId: details.tabId, frameId: details.frameId != null ? details.frameId : null, time: details.timeStamp || Date.now(), type: "", size: 0, status: 0 });
    }
  } catch (e) {}
}
// MV3 service worker dies idle: persist candidates in storage.session
// (per tab) or played-then-clicked-later finds empty list.
function persistCandidates() {
  try {
    const o = {};
    for (const [u, c] of candidates) o[u] = c;
    chrome.storage.session.set({ wcCandidates: o });
  } catch (e) {}
}
let candidatesLoaded = false;
async function loadCandidates() {
  if (candidatesLoaded) return;
  candidatesLoaded = true;
  try {
    const s = await chrome.storage.session.get(["wcCandidates"]);
    // ponytail: entries lacking numeric status predate status tracking (incl. pre-206 filter); drop, TTL re-sniff rediscovers live ones.
    if (s && s.wcCandidates) for (const [u, c] of Object.entries(s.wcCandidates)) if (c && typeof c.status === "number") candidates.set(u, c);
    pruneCandidates();
  } catch (e) {}
}
const SNIFF_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl", "application/x-mpegurl", "video/vnd.mpeg.dash.mpd", "video/mpegurl", "video/x-mpegurl", "application/mpegurl"]);
function isSniff(details, type) {
  const url = details.url || "";
  if (/\.(json|js|mjs|css|html?|map)($|[?#])/i.test(url)) return false; // page assets, whatever MIME claims
  let path = "";
  try { path = new URL(url).pathname.toLowerCase(); } catch (e) {}
  // Extension fallback: DASH/HLS/full mp4 served without usable Content-Type.
  if (!type || type === "application/octet-stream") {
    return path.endsWith(".mpd") || path.endsWith(".m3u8") || path.endsWith(".mp4");
  }
  if (SNIFF_TYPES.has(type)) return true;
  if (type.startsWith("video/") || type.startsWith("audio/")) {
    // Segments (seg-1.ts, chunk_00001.m4s, -0002.ts) pollute panel; only
    // manifests/full media belong there.
    try { const seg = new URL(url).pathname.split("/").pop() || ""; if (/^(seg|segment|chunk|frag)[-_]?\d|\.m4s$|[-_]\d+\.(ts|m4s)$/i.test(seg)) return false; } catch (e) {}
    return true;
  }
  return path.endsWith(".mpd") || path.endsWith(".m3u8");
}
function pruneCandidates(now = Date.now()) {
  let changed = false;
  for (const [url, c] of candidates) if (now - c.time > CANDIDATE_TTL_MS) { candidates.delete(url); changed = true; }
  while (candidates.size > CANDIDATE_MAX) { candidates.delete(candidates.keys().next().value); changed = true; }
  if (changed) persistCandidates();
}
if (chrome.webRequest) {
  try {
    const F = { urls: ["<all_urls>"] };
    const kill = (d) => { try { requestRecords.delete(d.requestId); } catch (e) {} };
    if (chrome.webRequest.onBeforeRequest) chrome.webRequest.onBeforeRequest.addListener(recordRequest, F);
    if (chrome.webRequest.onBeforeSendHeaders) chrome.webRequest.onBeforeSendHeaders.addListener(recordRequest, F);
    if (chrome.webRequest.onCompleted) chrome.webRequest.onCompleted.addListener(kill, F);
    if (chrome.webRequest.onErrorOccurred) chrome.webRequest.onErrorOccurred.addListener(kill, F);
  } catch (e) {}
}
if (chrome.webRequest && chrome.webRequest.onHeadersReceived) {
  chrome.webRequest.onHeadersReceived.addListener(async (details) => {
    recordRequest(details);
    const rec2 = requestRecords.get(details.requestId);
    if (rec2) { rec2.status = details.statusCode || 0; const lh = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-length"); if (lh) rec2.size = parseInt(lh.value, 10) || 0; const ch = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-type"); if (ch && ch.value) rec2.type = ch.value.split(";")[0].trim().toLowerCase(); }
    await loadCandidates();
    if (details.statusCode && ![200, 206].includes(details.statusCode)) return; // 404/redirect bodies poison panel
    if (!(details.tabId >= 0)) return; // non-tab leaks into every tab list
    const header = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-type");
    const type = header && header.value ? header.value.split(";")[0].trim().toLowerCase() : "";
    if (!isSniff(details, type)) return;
    const lenH = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-length");
    const rec = requestRecords.get(details.requestId);
    candidates.set(details.url, { url: details.url, type, status: details.statusCode || 0, tabId: details.tabId, frameId: details.frameId != null ? details.frameId : null, redirectFrom: (rec && rec.redirectFrom) || "", hops: (rec && rec.hops) || [], time: (rec && rec.time) || Date.now(), size: lenH ? parseInt(lenH.value, 10) || 0 : 0 });
    persistCandidates();
    pruneCandidates();
  }, { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "media", "object", "xmlhttprequest", "other"] }, ["responseHeaders"]);
}
function basename(name) {
  return name ? String(name).split(/[\\/]/).pop() || "" : "";
}
async function sendDownload(url, tab, extra = {}) {
  let cookies = "";
  let cookiesJson = "";
  try {
    // Cookies match page host (X/YouTube/Reddit session), not CDN host.
    // Page intent: pageUrl promotes to yt-dlp target app-side. Stream intent:
    // cookieUrl scopes cookies only, no promotion field sent.
    const cs = await chrome.cookies.getAll({ url: extra.pageUrl || extra.cookieUrl || url });
    cookies = cs.map(c => `${c.name}=${c.value}`).join("; ");
    // Structured form for yt-dlp cookie jar (httpOnly/domain/expiry lost in string form).
    cookiesJson = JSON.stringify(cs.map(c => ({ name: c.name, value: c.value, domain: c.domain, path: c.path, expirationDate: c.expirationDate, secure: c.secure, httpOnly: c.httpOnly, hostOnly: c.hostOnly })));
  } catch (e) {}
  // Referrer = video page (permalink), not feed tab URL (x.com/home).
  const referrer = extra.referrer != null ? extra.referrer : (extra.pageUrl || extra.cookieUrl || (tab ? tab.url : ""));
  const msg = { type: "download", url, referrer, referer: referrer, cookies, cookiesJson, filename: extra.filename ? basename(extra.filename) : filenameFromUrl(url) };
  if (extra.pageUrl) msg.pageUrl = extra.pageUrl;
  if (extra.intent) msg.intent = extra.intent;
  if (extra.mime) msg.mime = extra.mime;
  // Captured (intercepted) downloads pass host-side capture filter (extension
  // allowlist + min size from capture-filter.json). Explicit sends (context
  // menu, video panel, link grab) omit `captured` and always go: user intent.
  if (extra.captured) msg.captured = true;
  if (extra.size > 0) msg.size = extra.size;
  return nativeSend(msg);
}
function filenameFromUrl(u) {
  try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; }
}
const MEDIA_EXT = /\.(mp4|webm|mkv|mp3|mp4|zip|rar|7z|gz|pdf|exe|msi|dmg|deb|iso|tar|mpd|m3u8)(\?|#|$)/i;
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: "downloadcraft-link", title: "Download with webcraft", contexts: ["link"] });
  chrome.contextMenus.create({ id: "downloadcraft-media", title: "Download media with webcraft", contexts: ["video", "audio", "image"] });
  chrome.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with webcraft", contexts: ["page"] });
});
chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) sendDownload(info.linkUrl, tab);
  // blob:/data: srcUrl dies in native host; open panel (page yt-dlp row works).
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl && /^(blob|data):/.test(info.srcUrl)) { if (tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }); }
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) sendDownload(info.srcUrl, tab);
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" });
});
// Captured downloads go DIRECTLY to webcraft (its modal is the config box,
// like IDM). No browser ask box. Filter = webcraft Settings, decided by
// native host (captureCheck) BEFORE cancel: filtered -> browser keeps file.
const bypass = new Map(); // url -> timestamp, one-shot for resumed native downloads
const BYPASS_TTL_MS = 30 * 1000;
function takeBypass(url) {
  const t = bypass.get(url);
  if (t == null) return false;
  bypass.delete(url);
  return Date.now() - t < BYPASS_TTL_MS;
}
function resumeBrowser(url, filename) {
  bypass.set(url, Date.now());
  try { const p = chrome.downloads.download({ url, filename: filename || undefined }); if (p && p.catch) p.catch(() => {}); } catch (e) {}
}
// Never intercept page-asset downloads (json/js/css) even with download attr.
function isAssetDownload(item) {
  const m = String((item && item.mime) || "").toLowerCase();
  if (/json|javascript|ecmascript|css/.test(m)) return true;
  const f = String((item && item.filename) || (item && item.url) || "").split(/[?#]/)[0];
  return /\.(json|js|mjs|css)$/i.test(f);
}
async function captureCheck(entry) {
  try {
    const r = await nativeSend({ type: "captureCheck", url: entry.url, filename: entry.filename, mime: entry.mime, size: entry.size });
    return !!(r && r.allow === true); // fail-closed: garbage/down -> browser keeps file
  } catch (e) { return false; }
}
chrome.downloads.onCreated.addListener(async (item) => {
  if (isAssetDownload(item)) return;
  if (!item.url || item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  const url = item.finalUrl || item.url;
  if (takeBypass(url) || takeBypass(item.url)) return;
  try {
    const entry = { id: item.id, url, filename: basename(item.filename), mime: item.mime || "", referrer: item.referrer || "", size: item.totalBytes > 0 ? item.totalBytes : 0 };
    // Host filter decides (webcraft Settings). Deny/unreachable: do not
    // cancel — browser download proceeds untouched, no resume needed.
    if (!(await captureCheck(entry))) return;
    await chrome.downloads.cancel(item.id).catch(() => {});
    try {
      const resp = await sendDownload(entry.url, null, { referrer: entry.referrer, filename: entry.filename, mime: entry.mime, size: entry.size, captured: true });
      // Race filter change / host write error — never lose file.
      if (!resp || resp.skipped || resp.type === "error") resumeBrowser(entry.url, entry.filename);
    } catch (e) { resumeBrowser(entry.url, entry.filename); }
  } catch (e) {}
});
chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  // Legacy stale content scripts may still answer removed ask box: ignore.
  if (msg && msg.type === "downloadcraft-confirm-result") return;
  if (msg && msg.type === "getCandidates") {
    loadCandidates().then(() => {
      pruneCandidates();
      sweepRecords();
      const tabId = sender.tab && sender.tab.id;
      let pool;
      if (msg.element) {
        // Element-scoped (IDM): same tab+frame only. Prefer live records;
        // candidates carry same fields (frameId/redirectFrom/hops) and survive
        // SW/record cleanup, so match over union deduped by url.
        const byUrl = new Map();
        // status 206 = partial byte slice, not standalone file: never panel.
        [...requestRecords.values(), ...candidates.values()].forEach((r) => { if (r && r.url && r.status !== 206) byUrl.set(r.url, r); });
        const senderFrame = sender.frameId != null ? sender.frameId : null;
        const el = { tabId: sender.tab ? sender.tab.id : tabId, frameId: senderFrame, srcs: msg.element.srcs || [], srcKey: msg.element.srcKey || "", postId: msg.element.postId || "", since: msg.element.since || 0, now: Date.now() };
        pool = typeof matchRecordsForElement === "function" ? matchRecordsForElement([...byUrl.values()], el, Date.now()) : [];
        // Element-frame match empty: retry frame-agnostic (frameId=null passes
        // tolerant filter) before returning empty.
        if (!pool.length && senderFrame != null && typeof matchRecordsForElement === "function") pool = matchRecordsForElement([...byUrl.values()], { ...el, frameId: null }, Date.now());
      } else {
        pool = [...candidates.values()].filter((c) => c.status !== 206 && (tabId == null || c.tabId === tabId));
      }
      const list = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)(pool);
      sendResponse({ type: "candidates", candidates: list });
    });
    return true;
  }
  if (msg && msg.type === "downloadcraft-download") { sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
  // intent=stream: picked manifest/file is target; pageUrl must NOT promote
  // to yt-dlp page (else manifest choice lost). Only cookies scope passes.
  if (msg && msg.type === "downloadcraft-video-panel") { sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer || msg.pageUrl, pageUrl: msg.intent === "page" ? msg.pageUrl : undefined, cookieUrl: msg.pageUrl, intent: msg.intent }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
});
chrome.action.onClicked.addListener((tab) => { if (tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }).catch(() => {}); });
// SPA pushState nav fires url change without status=loading: drop stale streams then too.
if (chrome.tabs && chrome.tabs.onUpdated) chrome.tabs.onUpdated.addListener((tabId, info) => { if ((info.status === "loading" || info.url) && info.url) { let n = false; for (const [u, c] of [...candidates]) if (c.tabId === tabId) { candidates.delete(u); n = true; } for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k); if (n) persistCandidates(); } });
if (chrome.tabs && chrome.tabs.onRemoved) chrome.tabs.onRemoved.addListener((tabId) => { for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k); for (const [u, c] of [...candidates]) if (c.tabId === tabId) candidates.delete(u); persistCandidates(); });
