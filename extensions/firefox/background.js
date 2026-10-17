// downloadcraft Firefox background. Native host: com.downloadcraft.host
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
// --- Media sniffing (observing webRequest only; no blocking) ---
const CANDIDATE_TTL_MS = 5 * 60 * 1000;
const CANDIDATE_MAX = 50;
const candidates = new Map(); // url -> {url, type, tabId, time}
const SNIFF_TYPES = new Set(["application/dash+xml", "application/x-mpegurl", "application/vnd.apple.mpegurl"]);
function pruneCandidates(now = Date.now()) {
  for (const [url, c] of candidates) if (now - c.time > CANDIDATE_TTL_MS) candidates.delete(url);
  while (candidates.size > CANDIDATE_MAX) candidates.delete(candidates.keys().next().value);
}
if (browser.webRequest && browser.webRequest.onHeadersReceived) {
  browser.webRequest.onHeadersReceived.addListener((details) => {
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
  try { const cs = await browser.cookies.getAll({ url }); cookies = cs.map(c => `${c.name}=${c.value}`).join("; "); } catch (e) {}
  const referrer = extra.referrer != null ? extra.referrer : (tab ? tab.url : "");
  const msg = { type: "download", url, referrer, referer: referrer, cookies, filename: extra.filename ? basename(extra.filename) : filenameFromUrl(url) };
  if (extra.mime) msg.mime = extra.mime;
  return nativeSend(msg);
}
function filenameFromUrl(u) { try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; } }
browser.runtime.onInstalled.addListener(() => {
  browser.contextMenus.create({ id: "downloadcraft-link", title: "Download with downloadcraft", contexts: ["link"] });
  browser.contextMenus.create({ id: "downloadcraft-media", title: "Download media with downloadcraft", contexts: ["video", "audio", "image"] });
  browser.contextMenus.create({ id: "downloadcraft-page", title: "Download all links with downloadcraft", contexts: ["page"] });
});
browser.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "downloadcraft-link" && info.linkUrl) sendDownload(info.linkUrl, tab);
  else if (info.menuItemId === "downloadcraft-media" && info.srcUrl) sendDownload(info.srcUrl, tab);
  else if (info.menuItemId === "downloadcraft-page" && tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-grab-all" });
});
browser.downloads.onCreated.addListener(async (item) => {
  if (!item.url || item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  try {
    const stored = await browser.storage.local.get(["intercept"]);
    if (stored.intercept === false) return;
    await browser.downloads.cancel(item.id).catch(() => {});
    await sendDownload(item.finalUrl || item.url, null, { referrer: item.referrer || "", filename: basename(item.filename), mime: item.mime || "" });
  } catch (e) {}
});
browser.runtime.onMessage.addListener((msg, sender) => {
  if (msg && msg.type === "getCandidates") {
    pruneCandidates();
    const tabId = sender.tab && sender.tab.id;
    const list = [...candidates.values()].filter((c) => tabId == null || c.tabId === tabId || c.tabId === -1);
    return Promise.resolve({ type: "candidates", candidates: list });
  }
  if (msg && (msg.type === "downloadcraft-download" || msg.type === "downloadcraft-video-panel")) return sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer });
});
browser.browserAction.onClicked.addListener((tab) => { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "downloadcraft-scan" }); });
