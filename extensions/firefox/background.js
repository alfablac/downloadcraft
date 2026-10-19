// webcraft Firefox background. Native host: com.webcraft.host
// IDM-parity capture (from shipped IDM Firefox ext evidence): blocking
// webRequest.onHeadersReceived decides takeover by status, Content-Disposition,
// Content-Type (MIME->ext map), URL extension; media requests feed video panel.
const HOST = "com.webcraft.host";
// Native messaging: single in-flight queue (see Chromium background.js).
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
      nativePort = browser.runtime.connectNative(HOST);
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
    browser.runtime.sendNativeMessage(HOST, item.msg).then(
      (resp) => settleNative(item, null, resp || { type: "ok" }),
      (err) => settleNative(item, err),
    );
  }
}
function notify(title, message) {
  try { browser.notifications.create({ type: "basic", title, message: String(message || "").slice(0, 240) }); } catch (e) {}
}
async function safeSend(url, tab, extra = {}) {
  try { return await sendDownload(url, tab, extra); }
  catch (e) { if (!extra.captured) notify("webcraft: download not sent", "Native host unreachable. Start webcraft desktop app once, then retry. (" + (e && e.message || e) + ")"); throw e; }
}
// Ask-first capture removed: captures go straight to webcraft modal.
// ponytail: URL-only bypass for 60s; a same-URL re-click inside window goes
// to browser. Ceiling: cannot key by download id, id unknown at webRequest time.
const bypassUrls = new Map(); // url -> time
function markBypass(url) {
  const now = Date.now();
  bypassUrls.set(url, now);
  for (const [u, t] of bypassUrls) if (now - t > 60000) bypassUrls.delete(u);
}
function isBypassed(url) {
  const t = bypassUrls.get(url);
  if (!t) return false;
  if (Date.now() - t > 60000) { bypassUrls.delete(url); return false; }
  return true;
}
async function browserDownload(p) {
  markBypass(p.url);
  const opts = { url: p.url };
  if (p.filename) opts.filename = basename(p.filename);
  try { await browser.downloads.download(opts); }
  catch (e) { if (!opts.filename) throw e; markBypass(p.url); await browser.downloads.download({ url: p.url }); }
}

// --- Capture tables (IDM: Extensions registry list + MIME->ext map subset) ---

const PANEL_EXTS = new Set("aac avi f4v flv m4a m4v mkv mov mp3 mp4 mpeg mpg ogg ogv rm ts wav webm wma wmv 3gp".split(" "));
const STREAM_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl", "application/x-mpegurl", "video/vnd.mpeg.dash.mpd", "video/mpegurl", "video/x-mpegurl", "application/mpegurl"]);
const CANDIDATE_TTL_MS = 5 * 60 * 1000;
const CANDIDATE_MAX = 80;
const candidates = new Map(); // url -> {url, type, tabId, frameId, redirectFrom, time, size}
// IDM model: every in-flight request keyed by requestId (transient);
// on completion/error record dies, candidate (URL index) stays.
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
function pruneCandidates(now = Date.now()) {
  for (const [url, c] of candidates) if (now - c.time > CANDIDATE_TTL_MS) candidates.delete(url);
  while (candidates.size > CANDIDATE_MAX) candidates.delete(candidates.keys().next().value);
}
function headerOf(headers, name) { const h = (headers || []).find((x) => x.name.toLowerCase() === name); return h ? h.value || "" : ""; }
function extOfUrl(u) { try { const seg = new URL(u).pathname.split("/").pop() || ""; const i = seg.lastIndexOf("."); return i > 0 ? seg.slice(i + 1).toLowerCase() : ""; } catch (e) { return ""; } }
function filenameFromDisp(disp, url) {
  const m = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(disp || "");
  if (m) { try { return decodeURIComponent(m[1]); } catch (e) { return m[1]; } }
  return filenameFromUrl(url);
}
function filenameFromUrl(u) { try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; } }
function basename(name) { return name ? String(name).split(/[\\/]/).pop() || "" : ""; }
// IDM takeover rules: status ok, attachment, binary/media type, or known file extension.
function captureDecision(details) {
  if (isBypassed(details.url)) return null;
  if (![200, 206].includes(details.statusCode)) return null;
  if (/^(seg|segment|chunk|frag)[-_]?\d|\.m4s$|[-_]\d+\.(ts|m4s)$/i.test(filenameFromUrl(details.url))) return null; // stream segments: panel lists manifest, not fragments
  const type = headerOf(details.responseHeaders, "content-type").split(";")[0].trim().toLowerCase();
  const disp = headerOf(details.responseHeaders, "content-disposition").toLowerCase();
  const ext = extOfUrl(details.url);
  const fname = filenameFromDisp(disp, details.url);
  const isStream = STREAM_TYPES.has(type) || ext === "m3u8" || ext === "mpd";
  const isMedia = type.startsWith("video/") || type.startsWith("audio/") || PANEL_EXTS.has(ext);
  if (isStream || isMedia) return { url: details.url, type: type || (ext === "mpd" ? "application/dash+xml" : "application/x-mpegurl"), tabId: details.tabId, kind: "panel" };
  const isAttach = disp.startsWith("attachment");
  // NB: nearly all application/* is JSON/JS API traffic. Requiring real file
  // ext (no fname fallback) stops drive-by capture of site XHR.
  const isBinary = type === "application/octet-stream" || type === "application/x-download"
    || (type.startsWith("application/") && !/json|javascript|ecmascript|xhtml/.test(type));
  // Attachment is user intent, but json/js/css payloads are page assets, not downloads.
  var fnameExt = (String(fname || "").split(".").pop() || "").toLowerCase();
  if (/^(json|js|mjs|css|html?)$/.test(fnameExt) || /^(json|js|mjs|css|html?)$/.test(ext)) return null;
  // Dumb pipe: any attachment/binary is candidate; host captureCheck
  // (webcraft Settings) is sole policy decider. Local table removed.
  if (isAttach || isBinary) {
    if (type === "text/html" && !isAttach) return null;
    return { url: details.url, type, tabId: details.tabId, kind: "download", filename: fname, mime: type };
  }
  return null;
}
if (browser.webRequest) {
  try {
    const F = { urls: ["<all_urls>"] };
    const kill = (d) => { try { requestRecords.delete(d.requestId); } catch (e) {} };
    if (browser.webRequest.onBeforeRequest) browser.webRequest.onBeforeRequest.addListener(recordRequest, F);
    if (browser.webRequest.onBeforeSendHeaders) browser.webRequest.onBeforeSendHeaders.addListener(recordRequest, F);
    if (browser.webRequest.onCompleted) browser.webRequest.onCompleted.addListener(kill, F);
    if (browser.webRequest.onErrorOccurred) browser.webRequest.onErrorOccurred.addListener(kill, F);
  } catch (e) {}
}
if (browser.webRequest && browser.webRequest.onHeadersReceived) {
  browser.webRequest.onHeadersReceived.addListener(async (details) => {
    recordRequest(details);
    const rec = requestRecords.get(details.requestId);
    if (rec) { rec.status = details.statusCode || 0; rec.size = parseInt(headerOf(details.responseHeaders, "content-length"), 10) || rec.size || 0; const ct = headerOf(details.responseHeaders, "content-type").split(";")[0].trim().toLowerCase(); if (ct) rec.type = ct; }
    const d = captureDecision(details);
    if (!d) return {};
    // XHR/other: observe-only. Cancelling them kills site API calls.
    if (d.kind === "download" && (details.type === "xmlhttprequest" || details.type === "other")) return {};
    if (d.kind === "panel") {
      candidates.set(d.url, { url: d.url, type: d.type, status: details.statusCode || 0, tabId: d.tabId, frameId: details.frameId != null ? details.frameId : null, redirectFrom: rec ? rec.redirectFrom : "", hops: rec ? (rec.hops || []) : [], time: rec ? rec.time : Date.now(), size: parseInt(headerOf(details.responseHeaders, "content-length"), 10) || 0 });
      pruneCandidates();
      return {};
    }
    // Direct to webcraft modal (no browser ask box). Host decides filter
    // (webcraft Settings) BEFORE cancel: deny -> browser keeps file.
    try {
      const size = parseInt(headerOf(details.responseHeaders, "content-length"), 10) || 0;
      let allow = false;
      try { const r = await nativeSend({ type: "captureCheck", url: d.url, filename: d.filename, mime: d.mime, size }); allow = !!(r && r.allow === true); } catch (e) { allow = false; }
      if (!allow) return {}; // filtered/host down: browser downloads
      const ref = details.originUrl || details.documentUrl || "";
      const tab = details.tabId >= 0 ? await browser.tabs.get(details.tabId).catch(() => null) : null;
      markBypass(d.url); // BEFORE send: onCreated landing mid-send must skip
      const resp = await safeSend(d.url, tab, { filename: d.filename, mime: d.mime, referrer: ref, size, captured: true });
      // Not yet cancelled (return value decides): skipped/error -> browser keeps file.
      if (!resp || resp.skipped || resp.type === "error") return {};
      return { cancel: true };
    } catch (e) { return {}; } // native failed: let browser download proceed
  }, { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "media", "xmlhttprequest", "other"] }, ["blocking", "responseHeaders"]);
}
async function sendDownload(url, tab, extra = {}) {
  let cookies = "";
  let cookiesJson = "";
  // Cookies for page host, not sniffed CDN host (see Chromium).
  try { const cs = await browser.cookies.getAll({ url: extra.pageUrl || extra.cookieUrl || url }); cookies = cs.map(c => `${c.name}=${c.value}`).join("; "); cookiesJson = JSON.stringify(cs.map(c => ({ name: c.name, value: c.value, domain: c.domain, path: c.path, expirationDate: c.expirationDate, secure: c.secure, httpOnly: c.httpOnly, hostOnly: c.hostOnly }))); } catch (e) {}
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
// Menus: create on startup too (event-page restarts lose onInstalled-only menus).
function createMenus() {
  try {
    browser.contextMenus.create({ id: "downloadcraft-link", title: "Download with webcraft", contexts: ["link"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-media", title: "Download media with webcraft", contexts: ["video", "audio", "image"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with webcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-grab", title: "Grab site with webcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
  } catch (e) {}
}
browser.runtime.onInstalled.addListener(createMenus);
createMenus();
browser.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) safeSend(info.linkUrl, tab).catch(() => {});
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl && /^(blob|data):/.test(info.srcUrl)) { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }).catch(() => {}); }
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) safeSend(info.srcUrl, tab).catch(() => {});
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" });
  else if (info.menuItemId === "downloadcraft-grab" && tab && tab.url) safeSend("webcraft://grab?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url }).catch(() => {});
});
// Never intercept page-asset downloads (json/js/css) even with download attr.
function isAssetDownload(item) {
  const m = String((item && item.mime) || "").toLowerCase();
  if (/json|javascript|ecmascript|css/.test(m)) return true;
  const f = String((item && item.filename) || (item && item.url) || "").split(/[?#]/)[0];
  return /\.(json|js|mjs|css)$/i.test(f);
}
browser.downloads.onCreated.addListener(async (item) => {
  if (isAssetDownload(item)) return;
  if (!item.url || isBypassed(item.url) || item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  try {
    const url = item.finalUrl || item.url;
    const entry = { url, filename: basename(item.filename), mime: item.mime || "", size: item.totalBytes > 0 ? item.totalBytes : 0, referrer: item.referrer || "" };
    // Host filter (webcraft Settings) decides before cancel; deny = untouched.
    let allow = false;
    try { const r = await nativeSend({ type: "captureCheck", url: entry.url, filename: entry.filename, mime: entry.mime, size: entry.size }); allow = !!(r && r.allow === true); } catch (e) {}
    if (!allow) return;
    await browser.downloads.cancel(item.id).catch(() => {});
    try {
      const resp = await safeSend(url, null, { referrer: entry.referrer, filename: entry.filename, mime: entry.mime, size: entry.size, captured: true });
      if (!resp || resp.skipped || resp.type === "error") browserDownload(entry).catch(() => {}); // raced filter/host error: resume native
    } catch (e) { browserDownload(entry).catch(() => {}); } // host died mid-send: file must not vanish
  } catch (e) {}
});
browser.runtime.onMessage.addListener((msg, sender) => {
  // Legacy stale content scripts may still answer removed ask box: ignore.
  if (msg && msg.type === "downloadcraft-confirm-result") return Promise.resolve({ ok: true });
  if (msg && msg.type === "getCandidates") {
    pruneCandidates();
    sweepRecords();
    const tabId = sender.tab && sender.tab.id;
    let pool;
    if (msg.element) {
      // Element-scoped (IDM): same tab+frame only, never other-frame/tab.
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
      // No frame filter: player frame != sniffer frame on embeds drops all.
      pool = [...candidates.values()].filter((c) => c.status !== 206 && (tabId == null || c.tabId === tabId));
    }
    const list = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)(pool);
    return Promise.resolve({ type: "candidates", candidates: list });
  }
  if (msg && (msg.type === "downloadcraft-download" || msg.type === "downloadcraft-video-panel")) return safeSend(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer || msg.pageUrl, pageUrl: msg.intent === "page" ? msg.pageUrl : undefined, cookieUrl: msg.pageUrl, intent: msg.intent });
});

// Toolbar click: scan tab for video / page download (was no-op).
if (browser.browserAction && browser.browserAction.onClicked) browser.browserAction.onClicked.addListener((tab) => { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }).catch(() => {}); });
// SPA pushState nav: url change without status=loading also drops stale streams.
if (browser.tabs && browser.tabs.onUpdated) browser.tabs.onUpdated.addListener((tabId, info) => { if ((info.status === "loading" || info.url) && info.url) { for (const [u, c] of [...candidates]) if (c.tabId === tabId) candidates.delete(u); for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k); } });
if (browser.tabs && browser.tabs.onRemoved) browser.tabs.onRemoved.addListener((tabId) => { for (const [k, r] of [...requestRecords]) if (r.tabId === tabId) requestRecords.delete(k); for (const [u, c] of [...candidates]) if (c.tabId === tabId) candidates.delete(u); });
