// downloadcraft Firefox background. Native host: com.downloadcraft.host
// IDM-parity capture (from shipped IDM Firefox ext evidence): blocking
// webRequest.onHeadersReceived decides takeover by status, Content-Disposition,
// Content-Type (MIME->ext map), URL extension; media requests feed video panel.
const HOST = "com.downloadcraft.host";
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
  catch (e) { notify("downloadcraft: download not sent", "Native host unreachable. Start downloadcraft desktop app once, then retry. (" + (e && e.message || e) + ")"); throw e; }
}
// --- Ask-first capture: stash, prompt tab, act on answer. Fallback: safeSend. ---
const pendingCaptures = new Map(); // id -> {id,url,filename,mime,size,referrer,tabId,time}
let pendingNextId = 1;
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
// Returns true when panel shown (or same URL already awaiting an answer).
async function promptCapture(capture) {
  if (!(capture.tabId >= 0) || !capture.url) return false;
  const now = Date.now();
  for (const p of pendingCaptures.values()) if (p.url === capture.url && now - p.time < 60000) return true;
  const id = pendingNextId++;
  pendingCaptures.set(id, { ...capture, id, time: now });
  try {
    await browser.tabs.sendMessage(capture.tabId, { type: "downloadcraft-confirm", id, url: capture.url, filename: capture.filename || "", mime: capture.mime || "", size: capture.size || 0 }, { frameId: 0 });
    return true;
  } catch (e) { pendingCaptures.delete(id); return false; }
}
// --- Capture tables (IDM: Extensions registry list + MIME->ext map subset) ---
const FILE_EXTS = new Set("3gp 7z aac ace avi bin bz2 cab dmg deb doc docx epub exe flac gz iso jar mkv mov mp3 mp4 mpeg mpg msi ogg pdf ppt pptx psd rar rpm tar tgz wav webm wma wmv xls xlsx xz zip".split(" "));
const PANEL_EXTS = new Set("aac avi f4v flv m4a m4v mkv mov mp3 mp4 mpeg mpg ogg ogv rm ts wav webm wma wmv 3gp".split(" "));
const STREAM_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl", "application/x-mpegurl"]);
const CANDIDATE_TTL_MS = 5 * 60 * 1000;
const CANDIDATE_MAX = 80;
const candidates = new Map(); // url -> {url, type, tabId, time}
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
  if (/^Seg\d|^ts\d/i.test(filenameFromUrl(details.url))) return null; // stream segments: panel lists manifest, not fragments
  const type = headerOf(details.responseHeaders, "content-type").split(";")[0].trim().toLowerCase();
  const disp = headerOf(details.responseHeaders, "content-disposition").toLowerCase();
  const ext = extOfUrl(details.url);
  const fname = filenameFromDisp(disp, details.url);
  const isStream = STREAM_TYPES.has(type) || ext === "m3u8" || ext === "mpd";
  const isMedia = type.startsWith("video/") || type.startsWith("audio/") || PANEL_EXTS.has(ext);
  if (isStream || isMedia) return { url: details.url, type: type || (ext === "mpd" ? "application/dash+xml" : "application/x-mpegurl"), tabId: details.tabId, kind: "panel" };
  const isAttach = disp.startsWith("attachment");
  const isBinary = type === "application/octet-stream" || type === "application/x-download" || type.startsWith("application/") && type !== "application/xhtml+xml";
  if (isAttach || (isBinary && (FILE_EXTS.has(ext) || fname)) || FILE_EXTS.has(ext) && type !== "text/html") {
    if (type === "text/html" && !isAttach) return null;
    return { url: details.url, type, tabId: details.tabId, kind: "download", filename: fname, mime: type };
  }
  return null;
}
const interceptOn = async () => { try { const s = await browser.storage.local.get(["intercept"]); return s.intercept !== false; } catch (e) { return true; } };
const askFirstOn = async () => { try { const s = await browser.storage.local.get(["askBeforeDownload"]); return s.askBeforeDownload !== false; } catch (e) { return true; } };
if (browser.webRequest && browser.webRequest.onHeadersReceived) {
  browser.webRequest.onHeadersReceived.addListener(async (details) => {
    const d = captureDecision(details);
    if (!d) return {};
    if (d.kind === "panel") {
      candidates.set(d.url, { url: d.url, type: d.type, tabId: d.tabId, time: Date.now() });
      pruneCandidates();
      if (details.tabId >= 0) { try { browser.tabs.sendMessage(details.tabId, { type: "downloadcraft-candidates" }); } catch (e) {} }
      return {};
    }
    // File takeover: only main/sub frame navigations + explicit other downloads.
    if (!(await interceptOn())) return {};
    // Ask-first: cancel browser download, prompt tab, act on answer.
    try {
      const ref = details.originUrl || details.documentUrl || "";
      const cap = { url: d.url, filename: d.filename, mime: d.mime, size: parseInt(headerOf(details.responseHeaders, "content-length"), 10) || 0, referrer: ref, tabId: details.tabId };
      const shown = (await askFirstOn()) && await promptCapture(cap);
      if (!shown) { const tab = details.tabId >= 0 ? await browser.tabs.get(details.tabId).catch(() => null) : null; await safeSend(d.url, tab, { filename: d.filename, mime: d.mime, referrer: ref }); }
      return { cancel: true };
    } catch (e) { return {}; } // native failed: let browser download proceed
  }, { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "other"] }, ["blocking", "responseHeaders"]);
}
async function sendDownload(url, tab, extra = {}) {
  let cookies = "";
  try { const cs = await browser.cookies.getAll({ url }); cookies = cs.map(c => `${c.name}=${c.value}`).join("; "); } catch (e) {}
  const referrer = extra.referrer != null ? extra.referrer : (tab ? tab.url : "");
  const msg = { type: "download", url, referrer, referer: referrer, cookies, filename: extra.filename ? basename(extra.filename) : filenameFromUrl(url) };
  if (extra.mime) msg.mime = extra.mime;
  return nativeSend(msg);
}
// Menus: create on startup too (event-page restarts lose onInstalled-only menus).
function createMenus() {
  try {
    browser.contextMenus.create({ id: "downloadcraft-link", title: "Download with downloadcraft", contexts: ["link"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-media", title: "Download media with downloadcraft", contexts: ["video", "audio", "image"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with downloadcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
    browser.contextMenus.create({ id: "downloadcraft-grab", title: "Grab site with downloadcraft", contexts: ["page"] }, () => void browser.runtime.lastError);
  } catch (e) {}
}
browser.runtime.onInstalled.addListener(createMenus);
createMenus();
browser.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) safeSend(info.linkUrl, tab).catch(() => {});
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) safeSend(info.srcUrl, tab).catch(() => {});
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" });
  else if (info.menuItemId === "downloadcraft-grab" && tab && tab.url) safeSend("downloadcraft://grab?url=" + encodeURIComponent(tab.url), tab, { referrer: tab.url }).catch(() => {});
});
browser.downloads.onCreated.addListener(async (item) => {
  if (!item.url || isBypassed(item.url) || item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  try {
    if (!(await interceptOn())) return;
    await browser.downloads.cancel(item.id).catch(() => {});
    // Ask-first; downloads API carries no tabId, so prompt stays fallback-only here.
    const url = item.finalUrl || item.url;
    const cap = { url, filename: basename(item.filename), mime: item.mime || "", size: item.fileSize > 0 ? item.fileSize : 0, referrer: item.referrer || "", tabId: -1 };
    if (!((await askFirstOn()) && await promptCapture(cap))) await safeSend(url, null, { referrer: cap.referrer, filename: cap.filename, mime: cap.mime });
  } catch (e) {}
});
browser.runtime.onMessage.addListener((msg, sender) => {
  if (msg && msg.type === "downloadcraft-confirm-result") {
    const p = pendingCaptures.get(msg.id);
    if (p) {
      pendingCaptures.delete(msg.id);
      (async () => {
        if (msg.action === "downloadcraft") {
          const tab = p.tabId >= 0 ? await browser.tabs.get(p.tabId).catch(() => null) : null;
          try { await safeSend(p.url, tab, { filename: p.filename, mime: p.mime, referrer: p.referrer }); }
          catch (e) { try { await browserDownload(p); } catch (e2) {} } // host died: file must not vanish
        } else if (msg.action === "browser") {
          try { await browserDownload(p); } catch (e) { notify("downloadcraft: browser download failed", (e && e.message) || String(e)); }
        }
      })();
    }
    return Promise.resolve({ ok: true });
  }
  if (msg && msg.type === "getCandidates") {
    pruneCandidates();
    const tabId = sender.tab && sender.tab.id;
    const list = [...candidates.values()].filter((c) => tabId == null || c.tabId === tabId || c.tabId === -1);
    return Promise.resolve({ type: "candidates", candidates: list });
  }
  if (msg && (msg.type === "downloadcraft-download" || msg.type === "downloadcraft-video-panel")) return safeSend(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer });
});
// Ask-first leftovers: tab closed mid-prompt must not lose file (no candidates map here).
if (browser.tabs && browser.tabs.onRemoved) browser.tabs.onRemoved.addListener((tid) => { for (const [pid, p] of [...pendingCaptures]) if (p.tabId === tid) { pendingCaptures.delete(pid); browserDownload(p).catch(() => {}); } });
browser.browserAction.onClicked.addListener((tab) => { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }); });
