// idmcraft Chromium MV3 background. Native host: com.idmcraft.host
const HOST = "com.idmcraft.host";
let nativePort = null;
function nativeSend(msg) {
  return new Promise((resolve) => {
    try {
      if (!nativePort) {
        nativePort = chrome.runtime.connectNative(HOST);
        nativePort.onDisconnect.addListener(() => { nativePort = null; });
        nativePort.onMessage.addListener((m) => resolve(m));
      }
      nativePort.postMessage(msg);
      // Fallback resolve if host replies via onMessage already registered once.
      setTimeout(() => resolve({ type: "ok", queued: true }), 300);
    } catch (e) {
      // Fallback: one-shot sendNativeMessage
      chrome.runtime.sendNativeMessage(HOST, msg, (resp) => resolve(resp || { type: "ok" }));
    }
  });
}
async function sendDownload(url, tab) {
  let cookies = "";
  try {
    const cs = await chrome.cookies.getAll({ url });
    cookies = cs.map(c => `${c.name}=${c.value}`).join("; ");
  } catch (e) {}
  const msg = { type: "download", url, referrer: tab ? tab.url : "", referer: tab ? tab.url : "", cookies, filename: filenameFromUrl(url) };
  return nativeSend(msg);
}
function filenameFromUrl(u) {
  try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : ""; } catch (e) { return ""; }
}
const MEDIA_EXT = /\.(mp4|webm|mkv|mp3|mp4|zip|rar|7z|gz|pdf|exe|msi|dmg|deb|iso|tar|mpd|m3u8)(\?|#|$)/i;
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: "idmcraft-link", title: "Download with idmcraft", contexts: ["link"] });
  chrome.contextMenus.create({ id: "idmcraft-media", title: "Download media with idmcraft", contexts: ["video", "audio", "image"] });
  chrome.contextMenus.create({ id: "idmcraft-page", title: "Download all links with idmcraft", contexts: ["page"] });
});
chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "idmcraft-link" && info.linkUrl) sendDownload(info.linkUrl, tab);
  else if (info.menuItemId === "idmcraft-media" && info.srcUrl) sendDownload(info.srcUrl, tab);
  else if (info.menuItemId === "idmcraft-page" && tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "idmcraft-grab-all" });
});
// Intercept browser downloads and redirect to idmcraft
chrome.downloads.onCreated.addListener(async (item) => {
  if (!item.url || item.url.startsWith("blob:") || item.url.startsWith("data:")) return;
  // ponytail: chrome.downloads in MV3 requires user gesture in some builds; context menu + panel are primary paths.
  try {
    const stored = await chrome.storage.local.get(["intercept"]);
    if (stored.intercept === false) return;
    await chrome.downloads.cancel(item.id).catch(() => {});
    await sendDownload(item.finalUrl || item.url, null);
  } catch (e) {}
});
chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (msg && msg.type === "idmcraft-download") { sendDownload(msg.url, sender.tab).then(sendResponse); return true; }
  if (msg && msg.type === "idmcraft-video-panel") { sendDownload(msg.url, sender.tab).then(sendResponse); return true; }
});
chrome.action.onClicked.addListener((tab) => { if (tab && tab.id) chrome.tabs.sendMessage(tab.id, { type: "idmcraft-scan" }); });
