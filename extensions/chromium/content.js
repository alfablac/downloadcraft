// downloadcraft content script: video panel + link detection. Observed IDM behavior reimplemented clean-room.
(function () {
  const seen = new Set();
  const api = typeof browser !== "undefined" ? browser : chrome;
  function filenameFromUrl(u) {
    try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : u; } catch (e) { return u.split("/").pop() || u; }
  }
  function candidateLabel(c) {
    return `${filenameFromUrl(c.url)} (${c.type})`;
  }
  function fetchCandidates(cb) {
    let done = false;
    const finish = (list) => { if (!done) { done = true; cb(list || []); } };
    try {
      // Chrome MV3: callback form. Firefox-style promise also handled.
      const maybe = api.runtime.sendMessage({ type: "getCandidates" }, (r) => finish(r && r.candidates));
      if (maybe && maybe.then) maybe.then((r) => finish(r && r.candidates), () => finish([]));
    } catch (e) { finish([]); }
  }
  function labelFor(el, url) {
    // Quality labels: use data attributes / nearby text if present.
    const q = el.getAttribute && (el.getAttribute("data-quality") || el.getAttribute("label") || el.getAttribute("title"));
    return q ? `${q} — ${url.split("/").pop()}` : url.split("/").pop();
  }
  function ensurePanel() {
    let panel = document.getElementById("downloadcraft-panel");
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "downloadcraft-panel";
    panel.style.cssText = "position:absolute;z-index:2147483647;background:#111;color:#fff;font:12px sans-serif;border-radius:6px;padding:6px;box-shadow:0 2px 10px rgba(0,0,0,.5);max-width:420px;";
    panel.innerHTML = '<div style="font-weight:700;margin-bottom:4px;">Download with downloadcraft</div><div class="downloadcraft-items"></div>';
    document.documentElement.appendChild(panel);
    return panel;
  }
  function showConfirm(msg) {
    const panel = ensurePanel();
    panel.style.left = Math.max(8, window.scrollX + Math.round((window.innerWidth - 420) / 2)) + "px";
    panel.style.top = (window.scrollY + 16) + "px";
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    const close = document.createElement("button");
    close.textContent = "✕";
    close.title = "Browser download";
    close.style.cssText = "float:right;cursor:pointer;";
    const name = document.createElement("div");
    name.textContent = msg.filename || filenameFromUrl(msg.url || "");
    name.style.cssText = "font-weight:700;word-break:break-all;margin-right:24px;";
    const urlEl = document.createElement("div");
    urlEl.textContent = msg.url || "";
    urlEl.style.cssText = "color:#bbb;word-break:break-all;margin:4px 0;";
    const reply = (accept) => { panel.remove(); try { api.runtime.sendMessage({ type: "downloadcraft-confirm-result", id: msg.id, accept }); } catch (e) {} };
    const ok = document.createElement("button");
    ok.textContent = "Download with downloadcraft";
    ok.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;";
    ok.addEventListener("click", () => reply(true));
    const native = document.createElement("button");
    native.textContent = "Browser download";
    native.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;";
    native.addEventListener("click", () => reply(false));
    close.addEventListener("click", () => reply(false));
    box.appendChild(close); box.appendChild(name); box.appendChild(urlEl); box.appendChild(ok); box.appendChild(native);
  }
  function showPanel(anchor, items) {
    const panel = ensurePanel();
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    items.forEach((it) => {
      const b = document.createElement("button");
      b.textContent = it.label;
      b.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;";
      b.addEventListener("click", () => { api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: it.url, filename: it.filename || filenameFromUrl(it.url), mime: it.mime || it.type || "" }); panel.remove(); });
      box.appendChild(b);
    });
    const r = anchor.getBoundingClientRect();
    panel.style.left = (window.scrollX + r.left) + "px";
    panel.style.top = (window.scrollY + r.bottom + 4) + "px";
  }
  function scanVideo(video) {
    if (seen.has(video)) return; seen.add(video);
    const urls = [];
    if (video.currentSrc) urls.push(video.currentSrc);
    if (video.src) urls.push(video.src);
    video.querySelectorAll("source").forEach(s => { if (s.src) urls.push(s.src); });
    // DASH/HLS often set via MSE; page source scan below catches .mpd/.m3u8 strings.
    const uniq = [...new Set(urls)].filter(u => u && !u.startsWith("blob:"));
    if (!uniq.length) return;
    const btn = document.createElement("button");
    btn.textContent = "Download with downloadcraft";
    btn.style.cssText = "position:absolute;z-index:2147483646;background:#1e3a5f;color:#fff;border:0;border-radius:4px;padding:4px 8px;cursor:pointer;font:12px sans-serif;";
    btn.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); showPanel(btn, uniq.map(u => ({ url: u, label: labelFor(video, u) }))); });
    const parent = video.parentElement || document.body;
    if (getComputedStyle(parent).position === "static") parent.style.position = "relative";
    parent.appendChild(btn);
  }
  function scan() {
    document.querySelectorAll("video, audio").forEach(scanVideo);
    // Page-source scan for manifest URLs (IDM video panel does equivalent network/page sniffing observation).
    const html = document.documentElement.innerHTML;
    const re = /https?:\/\/[^\s"'<>]+\.(?:mpd|m3u8)(?:\?[^\s"'<>]*)?/gi;
    const found = [...new Set(html.match(re) || [])];
    // Expose via panel on first video-less page only through extension action scan message.
    return found;
  }
  let manifestUrls = [];
  const mo = new MutationObserver(() => { manifestUrls = scan() || manifestUrls; });
  mo.observe(document.documentElement, { childList: true, subtree: true });
  manifestUrls = scan();
  api.runtime.onMessage.addListener((msg) => {
    if (msg.type === "downloadcraft-confirm") { showConfirm(msg); return; }
    if (msg.type === "downloadcraft-scan") {
      const base = [...document.querySelectorAll("video, audio")].flatMap(v => [v.currentSrc, v.src].filter(Boolean)).concat(manifestUrls);
      fetchCandidates((candidates) => {
        const byUrl = new Map();
        [...new Set(base)].filter(u => u && !u.startsWith("blob:")).forEach(u => byUrl.set(u, { url: u, label: u.split("/").pop(), filename: filenameFromUrl(u) }));
        candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type, mime: c.type }); });
        const uniq = [...byUrl.values()];
        if (uniq.length) {
          const anchor = document.querySelector("video, audio") || document.body;
          // Use a temporary anchor at top-left if no media element.
          showPanel(anchor, uniq);
        }
      });
    }
    if (msg.type === "downloadcraft-grab-all") {
      document.querySelectorAll("a[href]").forEach(a => {
        const u = a.href;
        if (/\.(zip|rar|7z|gz|pdf|exe|msi|mp4|mp3|mkv|iso|tar)(\?|#|$)/i.test(u)) api.runtime.sendMessage({ type: "downloadcraft-download", url: u });
      });
    }
  });
})();
