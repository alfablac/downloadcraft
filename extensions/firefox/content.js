// downloadcraft content script: IDM-style video panel + link detection.
// Network sniffing lives in background (webRequest). This script anchors a
// panel on the largest visible media element and lists sniffed streams,
// including MSE/blob: players whose media URLs only exist as requests.
(function () {
  const api = typeof browser !== "undefined" ? browser : chrome;
  const wired = new WeakSet();
  function filenameFromUrl(u) {
    try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : u; } catch (e) { return u.split("/").pop() || u; }
  }
  function candidateLabel(c) { return `${filenameFromUrl(c.url)} (${c.type || "media"})`; }
  function fetchCandidates(cb) {
    let done = false;
    const finish = (list) => { if (!done) { done = true; cb(list || []); } };
    try {
      const maybe = api.runtime.sendMessage({ type: "getCandidates" }, (r) => finish(r && r.candidates));
      if (maybe && maybe.then) maybe.then((r) => finish(r && r.candidates), () => finish([]));
    } catch (e) { finish([]); }
  }
  function labelFor(el, url) {
    const q = el.getAttribute && (el.getAttribute("data-quality") || el.getAttribute("label") || el.getAttribute("title"));
    return q ? `${q} — ${url.split("/").pop()}` : url.split("/").pop();
  }
  function ensurePanel() {
    let panel = document.getElementById("downloadcraft-panel");
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "downloadcraft-panel";
    panel.style.cssText = "position:absolute;z-index:2147483647;background:#161616;color:#eee;font:12px sans-serif;border:1px solid #555;padding:6px;box-shadow:0 2px 10px rgba(0,0,0,.5);max-width:480px;min-width:260px;";
    panel.innerHTML = '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:4px;"><b>Download with downloadcraft</b><button class="dc-close" style="cursor:pointer;background:#2a2a2a;color:#eee;border:1px solid #555;padding:0 6px;">×</button></div><div class="downloadcraft-items"></div>';
    panel.querySelector(".dc-close").addEventListener("click", () => panel.remove());
    document.documentElement.appendChild(panel);
    return panel;
  }
  function showPanel(anchor, items) {
    if (!items.length) return;
    const panel = ensurePanel();
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    items.forEach((it) => {
      const b = document.createElement("button");
      b.textContent = it.label;
      b.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;background:#2a2a2a;color:#eee;border:1px solid #555;padding:4px 6px;";
      b.addEventListener("click", () => { api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: it.url, filename: it.filename || filenameFromUrl(it.url), mime: it.mime || it.type || "" }); panel.remove(); });
      box.appendChild(b);
    });
    const r = anchor.getBoundingClientRect();
    panel.style.left = (window.scrollX + r.left) + "px";
    panel.style.top = (window.scrollY + r.bottom + 4) + "px";
  }
  function mediaAnchor() {
    let best = null, bestArea = 0;
    document.querySelectorAll("video, audio").forEach((v) => {
      const r = v.getBoundingClientRect();
      const area = r.width * r.height;
      if (r.width > 40 && area > bestArea) { best = v; bestArea = area; }
    });
    return best;
  }
  function directUrls(video) {
    const urls = [];
    if (video.currentSrc) urls.push(video.currentSrc);
    if (video.src) urls.push(video.src);
    video.querySelectorAll("source").forEach((s) => { if (s.src) urls.push(s.src); });
    return [...new Set(urls)].filter((u) => u && !u.startsWith("blob:"));
  }
  function showForVideo(video) {
    const direct = directUrls(video);
    fetchCandidates((candidates) => {
      const byUrl = new Map();
      direct.forEach((u) => byUrl.set(u, { url: u, label: labelFor(video, u), filename: filenameFromUrl(u) }));
      manifestUrls.forEach((u) => { if (!byUrl.has(u)) byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u) }); });
      candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type, mime: c.type }); });
      showPanel(video, [...byUrl.values()]);
    });
  }
  function scanVideo(video) {
    if (wired.has(video)) return; wired.add(video);
    const btn = document.createElement("button");
    btn.textContent = "Download with downloadcraft";
    btn.style.cssText = "position:absolute;z-index:2147483646;background:#161616;color:#eee;border:1px solid #555;padding:4px 8px;cursor:pointer;font:12px sans-serif;";
    btn.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); showForVideo(video); });
    const parent = video.parentElement || document.body;
    if (getComputedStyle(parent).position === "static") parent.style.position = "relative";
    parent.appendChild(btn);
    // MSE/blob: player: no direct URL; still useful once requests are sniffed.
    const tryAuto = () => { if (!directUrls(video).length) return; };
    video.addEventListener("loadedmetadata", tryAuto, { once: true });
  }
  let manifestUrls = [];
  function scanManifests() {
    const html = document.documentElement.innerHTML;
    const re = /https?:\/\/[^\s"'<>]+\.(?:mpd|m3u8)(?:\?[^\s"'<>]*)?/gi;
    manifestUrls = [...new Set(html.match(re) || [])];
  }
  function scan() { document.querySelectorAll("video, audio").forEach(scanVideo); scanManifests(); }
  // Debounced: full-page rescan on every DOM mutation is too expensive.
  let pending = null;
  const mo = new MutationObserver(() => { if (pending) return; pending = setTimeout(() => { pending = null; scan(); }, 400); });
  mo.observe(document.documentElement, { childList: true, subtree: true });
  scan();
  api.runtime.onMessage.addListener((msg) => {
    if (msg.type === "downloadcraft-candidates") {
      // New stream sniffed for this tab: refresh open panel if present.
      const panel = document.getElementById("downloadcraft-panel");
      const anchor = mediaAnchor();
      if (panel && anchor) showForVideo(anchor);
    }
    if (msg.type === "downloadcraft-scan") {
      const anchor = mediaAnchor() || document.body;
      fetchCandidates((candidates) => {
        const byUrl = new Map();
        [...document.querySelectorAll("video, audio")].forEach((v) => directUrls(v).forEach((u) => byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u) })));
        manifestUrls.forEach((u) => { if (!byUrl.has(u)) byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u) }); });
        candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type, mime: c.type }); });
        showPanel(anchor, [...byUrl.values()]);
      });
    }
    if (msg.type === "downloadcraft-grab-all") {
      // IDM "Download all links": <a href> with downloadable extension, deduped.
      const seenUrls = new Set();
      document.querySelectorAll("a[href]").forEach((a) => {
        const u = a.href;
        if (!u || seenUrls.has(u)) return;
        if (/\.(zip|rar|7z|gz|bz2|xz|pdf|exe|msi|dmg|deb|apk|iso|tar|tgz|mp4|mp3|mkv|webm|flac|wav|mov|avi|epub)(\?|#|$)/i.test(u)) { seenUrls.add(u); api.runtime.sendMessage({ type: "downloadcraft-download", url: u }); }
      });
    }
  });
})();
