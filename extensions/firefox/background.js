// idmcraft Firefox background. Native host: com.idmcraft.host
const HOST = "com.idmcraft.host";
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
  browser.contextMenus.create({ id: "idmcraft-link", title: "Download with idmcraft", contexts: ["link"] });
  browser.contextMenus.create({ id: "idmcraft-media", title: "Download media with idmcraft", contexts: ["video", "audio", "image"] });
  browser.contextMenus.create({ id: "idmcraft-page", title: "Download all links with idmcraft", contexts: ["page"] });
});
browser.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "idmcraft-link" && info.linkUrl) sendDownload(info.linkUrl, tab);
  else if (info.menuItemId === "idmcraft-media" && info.srcUrl) sendDownload(info.srcUrl, tab);
  else if (info.menuItemId === "idmcraft-page" && tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "idmcraft-grab-all" });
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
  if (msg && (msg.type === "idmcraft-download" || msg.type === "idmcraft-video-panel")) return sendDownload(msg.url, sender.tab, { filename: msg.filename, mime: msg.mime, referrer: msg.referrer });
});
browser.browserAction.onClicked.addListener((tab) => { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "idmcraft-scan" }); });
