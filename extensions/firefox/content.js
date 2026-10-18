// webcraft content script: IDM-style video panel + link detection.
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
    panel.innerHTML = '<div style="display:flex;justify-content:flex-end;align-items:center;margin-bottom:4px;"><button class="dc-close" style="cursor:pointer;background:#2a2a2a;color:#eee;border:1px solid #555;padding:0 6px;">×</button></div><div class="downloadcraft-items"></div>';
    panel.querySelector(".dc-close").addEventListener("click", () => { if (panel.dataset.confirmId) { api.runtime.sendMessage({ type: "downloadcraft-confirm-result", id: Number(panel.dataset.confirmId), action: "browser" }); } panel.remove(); });
    document.documentElement.appendChild(panel);
    return panel;
  }
  function fmtSize(n) { if (!n) return ""; if (n >= 1048576) return (n / 1048576).toFixed(1) + " MB"; if (n >= 1024) return (n / 1024).toFixed(1) + " KB"; return n + " B"; }
  function ensureConfirm(msg) {
    let panel = ensurePanel();
    panel.id = "downloadcraft-panel";
    panel.dataset.confirmId = String(msg.id);
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    const close = panel.querySelector(".dc-close");
    const finish = (action) => { clearTimeout(timer); if (panel.dataset.confirmId) delete panel.dataset.confirmId; api.runtime.sendMessage({ type: "downloadcraft-confirm-result", id: msg.id, action }); panel.remove(); };
    const name = document.createElement("div");
    name.textContent = msg.filename || filenameFromUrl(msg.url);
    name.style.cssText = "font-weight:bold;word-break:break-all;margin:2px 0 4px;";
    const meta = document.createElement("div");
    meta.textContent = [fmtSize(msg.size), msg.mime].filter(Boolean).join("  ");
    meta.style.cssText = "color:#bbb;word-break:break-all;";
    const link = document.createElement("div");
    link.textContent = msg.url.length > 120 ? msg.url.slice(0, 117) + "..." : msg.url;
    link.style.cssText = "color:#8ab4f8;word-break:break-all;margin:2px 0 6px;";
    const row = document.createElement("div");
    row.style.cssText = "display:flex;gap:6px;";
    const mk = (text, action, primary) => {
      const b = document.createElement("button");
      b.textContent = text;
      b.style.cssText = "flex:1;text-align:center;margin:2px 0;padding:6px;cursor:pointer;border:1px solid #555;color:#eee;background:" + (primary ? "#1d65d8" : "#2a2a2a") + ";";
      b.addEventListener("click", () => finish(action));
      return b;
    };
    row.appendChild(mk("Download", "downloadcraft", true));
    row.appendChild(mk("Browser download", "browser", false));
    box.appendChild(name); box.appendChild(meta); box.appendChild(link); box.appendChild(row);
    // No answer (panel orphaned?) -> default browser download: file not lost.
    const timer = setTimeout(() => finish("browser"), 120000);
    panel.style.left = (window.scrollX + Math.max(8, (window.innerWidth - 480) / 2)) + "px";
    panel.style.top = (window.scrollY + 16) + "px";
  }
  function showPanel(anchor, items) {
    if (!items.length) return;
    const panel = ensurePanel();
    if (panel.dataset.confirmId) { api.runtime.sendMessage({ type: "downloadcraft-confirm-result", id: Number(panel.dataset.confirmId), action: "browser" }); delete panel.dataset.confirmId; }
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    items.forEach((it, i) => {
      const b = document.createElement("button");
      b.textContent = it.label;
      if (i === 0) { b.setAttribute("aria-selected", "true"); b.dataset.selected = "1"; }
      b.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;background:#2a2a2a;color:#eee;border:1px solid #555;padding:4px 6px;" + (i === 0 ? "outline:2px solid #8ab4f8;" : "");
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
  // Page-declared media invisible to <video> scan: og:/twitter: meta,
  // JSON-LD VideoObject, embed/object URLs. Blob:/data: excluded.
  function pageMediaUrls() {
    const out = [];
    const push = (v) => {
      if (!v || typeof v !== "string") return;
      try { const u = new URL(v.trim().replace(/\\\//g, "/"), location.href).href; if (/^https?:$/.test(new URL(u).protocol)) out.push(u); } catch (e) {}
    };
    document.querySelectorAll('meta[property="og:video"],meta[property="og:video:url"],meta[property="og:video:secure_url"],meta[name="twitter:player:stream"]').forEach((m) => push(m.getAttribute("content")));
    document.querySelectorAll('script[type="application/ld+json"]').forEach((s) => {
      try {
        const walk = (n) => { if (!n || typeof n !== "object") return; if (Array.isArray(n)) { n.forEach(walk); return; } push(n.contentUrl); push(n.embedUrl); Object.values(n).forEach(walk); };
        walk(JSON.parse(s.textContent || "null"));
      } catch (e) {}
    });
    document.querySelectorAll("iframe[src], embed[src], object[data]").forEach((e) => push(e.getAttribute("src") || e.getAttribute("data")));
    return [...new Set(out)];
  }
  function directUrls(video) {
    const urls = [];
    if (video.currentSrc) urls.push(video.currentSrc);
    if (video.src) urls.push(video.src);
    video.querySelectorAll("source").forEach((s) => { if (s.src) urls.push(s.src); });
    return [...new Set(urls)].filter((u) => u && !u.startsWith("blob:"));
  }
  function showForVideo(video) {
    // Rescan at click time: manifests/page media declared after load.
    scanManifests();
    const direct = [...new Set([...directUrls(video), ...pageMediaUrls()])];
    fetchCandidates((candidates) => {
      if (!direct.length && !candidates.length && !manifestUrls.length) {
        const panel = ensurePanel();
        const box = panel.querySelector(".downloadcraft-items");
        box.innerHTML = '<div style="color:#bbb;padding:4px 2px;">No stream captured yet. Press play on the video, then click again.</div>';
        const r = video.getBoundingClientRect();
        panel.style.left = (window.scrollX + Math.max(8, r.left)) + "px";
        panel.style.top = (window.scrollY + r.bottom + 4) + "px";
        return;
      }
      const byUrl = new Map();
      direct.forEach((u) => byUrl.set(u, { url: u, label: labelFor(video, u), filename: filenameFromUrl(u) }));
      manifestUrls.forEach((u) => { if (!byUrl.has(u)) byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u) }); });
      candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type, mime: c.type }); });
      showPanel(video, (typeof rankCandidates === "function" ? rankCandidates : (x) => x)([...byUrl.values()]));
    });
  }
  const btnFor = new WeakMap();
  function positionBtn(video) {
    const btn = btnFor.get(video);
    if (!btn) return;
    const r = video.getBoundingClientRect();
    if (r.width < 120 || r.height < 68) { btn.style.display = "none"; return; }
    btn.style.display = "";
    if (btn.dataset.dragged) return;
    btn.style.left = (window.scrollX + r.right - btn.offsetWidth - 8) + "px";
    btn.style.top = (window.scrollY + r.top + 8) + "px";
  }
  function attachBtn(video) {
    let btn = btnFor.get(video);
    if (btn) { positionBtn(video); return btn; }
    btn = document.createElement("button");
    btn.title = "Download video";
    btn.setAttribute("aria-label", "Download video");
    btn.setAttribute("data-downloadcraft-btn", "1");
    btn.innerHTML = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" style="display:block;margin:5px auto;pointer-events:none"><path d="M12 3v11"/><path d="m7 10 5 5 5-5"/><path d="M4 20h16"/></svg>';
    btn.style.cssText = "position:absolute;z-index:2147483646;width:24px;height:24px;border-radius:50%;background:rgba(17,17,17,.45);color:#fff;border:1px solid rgba(255,255,255,.15);padding:0;cursor:grab;text-align:center;opacity:0;pointer-events:none;display:none;touch-action:none;user-select:none;-webkit-user-select:none;";
    btn.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); if (btn.dataset.suppressClick) { delete btn.dataset.suppressClick; return; } showForVideo(video); });

    // Draggable: press-move >4px drags (persisted offset per video via dataset),
    // plain click still downloads. Grab cursor affords drag.
    let drag = null;
    btn.addEventListener("pointerdown", (e) => {
      drag = { x: e.clientX, y: e.clientY, moved: false };
      btn.setPointerCapture && btn.setPointerCapture(e.pointerId);
      btn.style.cursor = "grabbing";
    });
    btn.addEventListener("pointermove", (e) => {
      if (!drag) return;
      const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
      if (Math.abs(dx) + Math.abs(dy) > 4) drag.moved = true;
      if (drag.moved) {
        btn.style.left = (parseFloat(btn.style.left || "0") + dx) + "px";
        btn.style.top = (parseFloat(btn.style.top || "0") + dy) + "px";
        drag.x = e.clientX; drag.y = e.clientY;
        btn.dataset.dragged = "1";
      }
    });
    btn.addEventListener("pointerup", (e) => {
      const wasDrag = drag && drag.moved;
      drag = null;
      btn.style.cursor = "grab"; if (wasDrag) { btn.dataset.suppressClick = "1"; setTimeout(() => delete btn.dataset.suppressClick, 50); }
      if (wasDrag) { e.preventDefault(); e.stopPropagation(); }
    });
    // Discreet: invisible until pointer over video or button itself;
    // hidden state must not swallow clicks meant for video controls.
    const showBtn = () => { btn.style.opacity = ".9"; btn.style.pointerEvents = "auto"; };
    const hideBtn = () => { btn.style.opacity = "0"; btn.style.pointerEvents = "none"; };
    video.addEventListener("mouseenter", showBtn);
    video.addEventListener("mouseleave", hideBtn);
    btn.addEventListener("mouseenter", showBtn);
    btn.addEventListener("mouseleave", hideBtn);
    btn.addEventListener("focus", showBtn);
    btn.addEventListener("blur", hideBtn);

    btnFor.set(video, btn);
    (document.body || document.documentElement).appendChild(btn);
    positionBtn(video);
    return btn;
  }
  function scanVideo(video) {
    if (wired.has(video)) { attachBtn(video); return; }
    wired.add(video);
    // Blob/MSE players (Reddit etc): button must exist before any direct URL
    // is knowable; streams resolve from sniffed requests at click time.
    const show = () => attachBtn(video);
    if (video.getBoundingClientRect().width >= 120) attachBtn(video);
    ["loadedmetadata", "canplay", "playing", "loadeddata"].forEach((ev) => video.addEventListener(ev, show));
  }
  let manifestUrls = [];
  function scanManifests() {
    // Escaped (https:\/\/, &amp;) and relative manifest refs in inline
    // player config were missed by absolute-URL-only regex.
    const html = document.documentElement.innerHTML.replace(/\\\//g, "/").replace(/&amp;/g, "&");
    const abs = html.match(/https?:\/\/[^\s"'<>\\]+\.(?:mpd|m3u8)(?:\?[^\s"'<>\\]*)?/gi) || [];
    const rel = html.match(/["'(=]\s*([^\s"'<>\\]+\.(?:mpd|m3u8)(?:\?[^\s"'<>\\]*)?)/gi) || [];
    const out = [...abs];
    rel.forEach((m) => { try { out.push(new URL(m.replace(/^["'(=]\s*/, ""), location.href).href); } catch (e) {} });
    manifestUrls = [...new Set(out)];
  }
  function scan() { document.querySelectorAll("video, audio").forEach(scanVideo); scanManifests(); }
  // Debounced: full-page rescan on every DOM mutation is too expensive.
  let pending = null;
  const mo = new MutationObserver(() => { if (pending) return; pending = setTimeout(() => { pending = null; scan(); document.querySelectorAll("video, audio").forEach(positionBtn); }, 400); });
  window.addEventListener("resize", () => document.querySelectorAll("video, audio").forEach(positionBtn), { passive: true });
  mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["src", "poster", "style", "class"] });
  scan();
  api.runtime.onMessage.addListener((msg) => {
    if (msg.type === "downloadcraft-candidates") {
      // New stream sniffed for this tab: refresh open panel if present.
      const panel = document.getElementById("downloadcraft-panel");
      const anchor = mediaAnchor();
      if (panel && anchor) showForVideo(anchor);
    }
    if (msg.type === "downloadcraft-confirm" && self === top) ensureConfirm(msg);
    if (msg.type === "downloadcraft-scan") {
      const anchor = mediaAnchor() || document.body;
      fetchCandidates((candidates) => {
        const byUrl = new Map();
        [...document.querySelectorAll("video, audio")].forEach((v) => directUrls(v).forEach((u) => byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u) })));
        manifestUrls.forEach((u) => { if (!byUrl.has(u)) byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u) }); });
        candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type, mime: c.type }); });
        showPanel(anchor, (typeof rankCandidates === "function" ? rankCandidates : (x) => x)([...byUrl.values()]));
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
