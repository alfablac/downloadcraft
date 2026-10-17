// idmcraft Firefox background. Native host: com.idmcraft.host
const HOST = "com.idmcraft.host";
let nativePort = null;
function nativeSend(msg) {
  try {
    if (!nativePort) {
      nativePort = browser.runtime.connectNative(HOST);
      nativePort.onDisconnect.addListener(() => { nativePort = null; });
    }
    nativePort.postMessage(msg);
    return Promise.resolve({ type: "ok", queued: true });
  } catch (e) {
    return browser.runtime.sendNativeMessage(HOST, msg).catch(() => ({ type: "ok" }));
  }
}
async function sendDownload(url, tab) {
  let cookies = "";
  try { const cs = await browser.cookies.getAll({ url }); cookies = cs.map(c => `${c.name}=${c.value}`).join("; "); } catch (e) {}
  return nativeSend({ type: "download", url, referrer: tab ? tab.url : "", referer: tab ? tab.url : "", cookies, filename: filenameFromUrl(url) });
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
    await sendDownload(item.finalUrl || item.url, null);
  } catch (e) {}
});
browser.runtime.onMessage.addListener((msg, sender) => {
  if (msg && (msg.type === "idmcraft-download" || msg.type === "idmcraft-video-panel")) return sendDownload(msg.url, sender.tab);
});
browser.browserAction.onClicked.addListener((tab) => { if (tab && tab.id) browser.tabs.sendMessage(tab.id, { type: "idmcraft-scan" }); });
