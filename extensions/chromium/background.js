// downloadcraft Chromium MV3 background. Native host: com.downloadcraft.host
const HOST = "com.downloadcraft.host";
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
const candidates = new Map(); // url -> {url, type, tabId, time}
const SNIFF_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl"]);
function pruneCandidates(now = Date.now()) {
  for (const [url, c] of candidates) if (now - c.time > CANDIDATE_TTL_MS) candidates.delete(url);
  while (candidates.size > CANDIDATE_MAX) candidates.delete(candidates.keys().next().value);
}
if (chrome.webRequest && chrome.webRequest.onHeadersReceived) {
  chrome.webRequest.onHeadersReceived.addListener((details) => {
    const header = (details.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-type");
    if (!header || !header.value) return;
    const type = header.value.split(";")[0].trim().toLowerCase();
    if (!SNIFF_TYPES.has(type) && !type.startsWith("video/")) return;
    candidates.set(details.url, { url: details.url, type, tabId: details.tabId, time: Date.now() });
    pruneCandidates();
  }, { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "media", "object", "xmlhttprequest", "other"] }, ["responseHeaders"]);
}
function basename(name) {
  return name ? String(name).split(/[\\/]/).pop() || "" : "";
}
async function sendDownload(url, tab, extra = {}) {
  let cookies = "";
  try {
    const cs = await chrome.cookies.getAll({ url });
    cookies = cs.map(c => `${c.name}=${c.value}`).join("; ");
  } catch (e) {}
  const referrer = extra.referrer != null ? extra.referrer : (tab ? tab.url : "");
  const msg = { type: "download", url, referrer, referer: referrer, cookies, filename: extra.filename ? basename(extra.filename) : filenameFromUrl(url) };
  if (extra.mime) msg.mime = extra.mime;
  return nativeSend(msg);
}
function filenameFromUrl(u) {
  try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; }
}
const MEDIA_EXT = /\.(mp4|webm|mkv|mp3|mp4|zip|rar|7z|gz|pdf|exe|msi|dmg|deb|iso|tar|mpd|m3u8)(\?|#|$)/i;
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: "downloadcraft-link", title: "Download with downloadcraft", contexts: ["link"] });
  chrome.contextMenus.create({ id: "downloadcraft-media", title: "Download media with downloadcraft", contexts: ["video", "audio", "image"] });
  chrome.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with downloadcraft", contexts: ["page"] });
});
chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) sendDownload(info.linkUrl, tab);
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) sendDownload(info.srcUrl, tab);
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" });
});
// Ask-first intercept: stash pending, confirm in originating tab, then
// send to downloadcraft or resume browser-native download.
const pending = new Map(); // download id -> {id, url, filename, mime, referrer}
const bypass = new Map(); // url -> timestamp, one-shot for resumed native downloads
const BYPASS_TTL_MS = 30 * 1000;
function takeBypass(url) {
  const t = bypass.get(url);
  if (t == null) return false;
  bypass.delete(url);
  return Date.now() - t < BYPASS_TTL_MS;
}
// Intercept browser downloads and ask before redirecting to downloadcraft
chrome.downloads.onCreated.addListener(async (item) => {
  if (!item.url || item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  const url = item.finalUrl || item.url;
  if (takeBypass(url) || takeBypass(item.url)) return;
  // ponytail: chrome.downloads in MV3 requires user gesture in some builds; context menu + panel are primary paths.
  try {
    const stored = await chrome.storage.local.get(["intercept", "askBeforeDownload"]);
    if (stored.intercept === false) return;
    await chrome.downloads.cancel(item.id).catch(() => {});
    const entry = { id: item.id, url, filename: basename(item.filename), mime: item.mime || "", referrer: item.referrer || "" };
    const extra = { referrer: entry.referrer, filename: entry.filename, mime: entry.mime };
    if (stored.askBeforeDownload === false) { await sendDownload(entry.url, null, extra); return; }
    pending.set(item.id, entry);
    let tabId = typeof item.tabId === "number" && item.tabId >= 0 ? item.tabId : null;
    if (tabId == null) {
      try { const tabs = await chrome.tabs.query({ active: true, currentWindow: true }); tabId = tabs && tabs[0] ? tabs[0].id : null; } catch (e) {}
    }
    let delivered = false;
    if (tabId != null) {
      try { await chrome.tabs.sendMessage(tabId, { type: "downloadcraft-confirm", id: entry.id, url: entry.url, filename: entry.filename, mime: entry.mime }); delivered = true; } catch (e) {}
    }
    if (!delivered) { pending.delete(item.id); await sendDownload(entry.url, null, extra); }
  } catch (e) {}
});
chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (msg && msg.type === "downloadcraft-confirm-result") {
    const entry = pending.get(msg.id);
    if (entry) {
      pending.delete(msg.id);
      if (msg.accept) {
        sendDownload(entry.url, null, { referrer: entry.referrer, filename: entry.filename, mime: entry.mime }).catch(() => {});
      } else {
        // Resume browser-native. Bypass keyed by URL so onCreated skips re-intercept.
        bypass.set(entry.url, Date.now());
        try { const p = chrome.downloads.download({ url: entry.url, filename: entry.filename || undefined }); if (p && p.catch) p.catch(() => {}); } catch (e) {}
      }
    }
    return;
  }
  if (msg && msg.type === "getCandidates") {
    pruneCandidates();
    const tabId = sender.tab && sender.tab.id;
    const list = [...candidates.values()].filter((c) => tabId == null || c.tabId === tabId || c.tabId === -1);
    sendResponse({ type: "candidates", candidates: list });
    return false;
  }
  if (msg && msg.type === "downloadcraft-download") { sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
  if (msg && msg.type === "downloadcraft-video-panel") { sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer }).then(sendResponse, (e) => sendResponse({ type: "error", error: String((e && e.message) || e) })); return true; }
});
chrome.action.onClicked.addListener((tab) => { if (tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }); });
