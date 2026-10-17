// idmcraft content script: video panel + link detection. Observed IDM behavior reimplemented clean-room.
(function () {
  const seen = new Set();
  function labelFor(el, url) {
    // Quality labels: use data attributes / nearby text if present.
    const q = el.getAttribute && (el.getAttribute("data-quality") || el.getAttribute("label") || el.getAttribute("title"));
    return q ? `${q} — ${url.split("/").pop()}` : url.split("/").pop();
  }
  function ensurePanel() {
    let panel = document.getElementById("idmcraft-panel");
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "idmcraft-panel";
    panel.style.cssText = "position:absolute;z-index:2147483647;background:#111;color:#fff;font:12px sans-serif;border-radius:6px;padding:6px;box-shadow:0 2px 10px rgba(0,0,0,.5);max-width:420px;";
    panel.innerHTML = '<div style="font-weight:700;margin-bottom:4px;">Download with idmcraft</div><div class="idmcraft-items"></div>';
    document.documentElement.appendChild(panel);
    return panel;
  }
  function showPanel(anchor, items) {
    const panel = ensurePanel();
    const box = panel.querySelector(".idmcraft-items");
    box.innerHTML = "";
    items.forEach((it) => {
      const b = document.createElement("button");
      b.textContent = it.label;
      b.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;";
      b.addEventListener("click", () => { chrome.runtime.sendMessage({ type: "idmcraft-video-panel", url: it.url }); panel.remove(); });
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
    btn.textContent = "Download with idmcraft";
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
  chrome.runtime.onMessage.addListener((msg) => {
    if (msg.type === "idmcraft-scan") {
      const items = [...document.querySelectorAll("video, audio")].flatMap(v => [v.currentSrc, v.src].filter(Boolean)).concat(manifestUrls);
      const uniq = [...new Set(items)].filter(u => !u.startsWith("blob:"));
      if (uniq.length) {
        const anchor = document.querySelector("video, audio") || document.body;
        // Use a temporary anchor at top-left if no media element.
        showPanel(anchor, uniq.map(u => ({ url: u, label: u.split("/").pop() })));
      }
    }
    if (msg.type === "idmcraft-grab-all") {
      document.querySelectorAll("a[href]").forEach(a => {
        const u = a.href;
        if (/\.(zip|rar|7z|gz|pdf|exe|msi|mp4|mp3|mkv|iso|tar)(\?|#|$)/i.test(u)) chrome.runtime.sendMessage({ type: "idmcraft-download", url: u });
      });
    }
  });
})();
