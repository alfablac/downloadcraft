// webcraft content script (Chromium MV3): video button + panel.
// IDM-observed behavior, clean-room: button appears on first playable/sized
// frame (readyState>=2 or a poster), anchored top-right of the media frame,
// even when src is blob:/MSE (Reddit, YouTube...). Stream URLs come from the
// background webRequest sniffer; clicking the button (and only then) queries
// candidates and downloads the picked stream DIRECTLY (no second list that
// can dead-end empty -> "button does nothing").
(function () {
  const api = typeof browser !== "undefined" ? browser : chrome;
  const wired = new WeakSet();
  const btnFor = new WeakMap();

  function filenameFromUrl(u) {
    try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : u; } catch (e) { return (u || "").split("/").pop() || u; }
  }
  function candidateLabel(c) { return `${filenameFromUrl(c.url)} (${c.type || "media"})`; }
  function fetchCandidates(cb) {
    let done = false;
    const finish = (list) => { if (!done) { done = true; cb(list || []); } };
    try {
      const maybe = api.runtime.sendMessage({ type: "getCandidates" }, (r) => finish(r && r.candidates));
      if (maybe && maybe.then) maybe.then((r) => finish(r && r.candidates), () => finish([]));
    } catch (e) { finish([]); }
    setTimeout(() => finish([]), 900);
  }
  function labelFor(el, url) {
    const q = el.getAttribute && (el.getAttribute("data-quality") || el.getAttribute("label") || el.getAttribute("title"));
    return q ? `${q} - ${url.split("/").pop()}` : url.split("/").pop();
  }

  // ---------- direct download (button -> pick -> native host) ----------
  function sendStream(it) {
    try {
      const p = api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: it.url, filename: it.filename || filenameFromUrl(it.url), mime: it.mime || it.type || "" });
      if (p && p.catch) p.catch(() => {});
    } catch (e) {}
  }

  // ---------- panel (stream picker + ask-first confirm) ----------
  function ensurePanel() {
    let panel = document.getElementById("downloadcraft-panel");
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "downloadcraft-panel";
    panel.style.cssText = "position:absolute;z-index:2147483647;background:#111;color:#fff;font:12px sans-serif;border-radius:6px;padding:6px;box-shadow:0 2px 10px rgba(0,0,0,.5);max-width:420px;min-width:260px;";
    panel.innerHTML = '<div style="display:flex;justify-content:flex-end;align-items:center;margin-bottom:4px;"><button class="dc-close" style="cursor:pointer;background:#2a2a2a;color:#eee;border:1px solid #555;padding:0 6px;">×</button></div><div class="downloadcraft-items"></div>';
    panel.querySelector(".dc-close").addEventListener("click", () => panel.remove());
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
    ok.textContent = "Download";
    ok.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;";
    ok.addEventListener("click", () => reply(true));
    const nat = document.createElement("button");
    nat.textContent = "Browser download";
    nat.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;";
    nat.addEventListener("click", () => reply(false));
    close.addEventListener("click", () => reply(false));
    box.appendChild(close); box.appendChild(name); box.appendChild(urlEl); box.appendChild(ok); box.appendChild(nat);
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
    return [...new Set(urls)].filter((u) => u && !u.startsWith("blob:") && !u.startsWith("data:"));
  }

  function pickStream(video) {
    // Rescan at click time: manifests/page media declared after load.
    scanManifests();
    const direct = [...new Set([...directUrls(video), ...manifestUrls, ...pageMediaUrls()])];
    fetchCandidates((candidates) => {
      const mans = [], medias = [];
      candidates.forEach((c) => { if (c && c.url) (/mpd|m3u8|dash|mpegurl/i.test(c.type || c.url) ? mans : medias).push(c); });
      const all = [...direct.map((u) => ({ url: u })), ...mans, ...medias];
      const seenU = new Set();
      const items = [];
      all.forEach((c) => { if (!seenU.has(c.url)) { seenU.add(c.url); items.push(c); } });
      if (!items.length) {
        // Nothing sniffed yet: stream may exist but player has not fetched
        // past the manifest gate. Tell user instead of silent dead click.
        showPanel(video, [], "No stream captured yet. Press play on the video, then click again.");
        return;
      }
      if (items.length === 1) { sendStream({ url: items[0].url, filename: filenameFromUrl(items[0].url), type: items[0].type, mime: items[0].type }); return; }
      showPanel(video, items.map((c) => ({ url: c.url, label: c.label || candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type })));
    });
  }

  function showPanel(anchor, items, note) {
    const panel = ensurePanel();
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    if (note) {
      const d = document.createElement("div");
      d.textContent = note;
      d.style.cssText = "color:#bbb;padding:4px 2px;word-break:break-word;";
      box.appendChild(d);
    }
    items.forEach((it) => {
      const b = document.createElement("button");
      b.textContent = it.label;
      b.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;background:#2a2a2a;color:#eee;border:1px solid #555;padding:4px 6px;";
      b.addEventListener("click", () => { sendStream(it); panel.remove(); });
      box.appendChild(b);
    });
    const r = anchor.getBoundingClientRect();
    panel.style.left = (window.scrollX + Math.max(8, r.left)) + "px";
    panel.style.top = (window.scrollY + r.bottom + 4) + "px";
  }

  // ---------- button lifecycle ----------
  function positionBtn(video) {
    const btn = btnFor.get(video);
    if (!btn) return;
    const r = video.getBoundingClientRect();
    if (r.width < 40 || r.height < 30) { btn.style.display = "none"; return; }
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
    btn.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); if (btn.dataset.suppressClick) { delete btn.dataset.suppressClick; return; } pickStream(video); });

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
    const show = () => { if (isBigEnough(video)) attachBtn(video); };
    if (isBigEnough(video)) attachBtn(video);
    ["loadedmetadata", "canplay", "playing", "loadeddata"].forEach((ev) => video.addEventListener(ev, show));
  }
  function isBigEnough(video) {
    const r = video.getBoundingClientRect();
    return r.width >= 120 && r.height >= 68;
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
  let pending = null;
  const mo = new MutationObserver(() => {
    if (pending) return;
    pending = setTimeout(() => {
      pending = null; scan();
      document.querySelectorAll("video, audio").forEach(positionBtn);
    }, 400);
  });
  mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["src", "style", "class"] });
  window.addEventListener("resize", () => document.querySelectorAll("video, audio").forEach(positionBtn), { passive: true });
  scan();

  api.runtime.onMessage.addListener((msg) => {
    if (!msg) return;
    if (msg.type === "downloadcraft-confirm") { showConfirm(msg); return; }
    if (msg.type === "downloadcraft-navigate") { scan(); return; }
    if (msg.type === "downloadcraft-scan") {
      const anchor = document.querySelector("video, audio");
      if (anchor) { attachBtn(anchor); pickStream(anchor); return; }
      // No media element (embed/meta-only page): panel vs page media +
      // sniffed candidates, anchored to body.
      scanManifests();
      fetchCandidates((candidates) => {
        const byUrl = new Map();
        [...manifestUrls, ...pageMediaUrls()].forEach((u) => byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u), type: "" }));
        candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: `${filenameFromUrl(c.url)} (${c.type || "media"})`, filename: filenameFromUrl(c.url), type: c.type }); });
        const items = [...byUrl.values()];
        if (items.length) showPanel(document.body, items);
      });
      return;
    }
    if (msg.type === "downloadcraft-grab-all") {
      document.querySelectorAll("a[href]").forEach((a) => {
        const u = a.href;
        if (/\.(zip|rar|7z|gz|pdf|exe|msi|mp4|mp3|mkv|iso|tar)(\?|#|$)/i.test(u)) api.runtime.sendMessage({ type: "downloadcraft-download", url: u });
      });
    }
  });
})();
