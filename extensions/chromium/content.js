// webcraft content script: IDM-style video panel + link detection.
// Network sniffing lives in background (webRequest). This script anchors a
// panel on the largest visible media element and lists sniffed streams,
// including MSE/blob: players whose media URLs only exist as requests.
(function () {
  const api = typeof browser !== "undefined" ? browser : chrome;
  const wired = new WeakSet();
  // Stable per-element id (IDM model): WeakMap video -> "wc1", "wc2"...
  const elIds = new WeakMap();
  let elSeq = 0;
  function elementId(video) { let id = elIds.get(video); if (!id) { id = "wc" + (++elSeq); elIds.set(video, id); } return id; }
  function anchorFor(video) {
    // Captured at firstPlay (scan/playing handlers set fields). srcKey =
    // non-blob currentSrc at anchor time; survives later src swaps.
    if (!video._wcAnchor) {
      const srcs = directUrls(video);
      video._wcAnchor = { since: video._wcFirstPlay || video._wcSeen || 0, postId: idFromUrl(permalinkFor(video)) || idFromUrl(location.href), srcKey: srcs[0] || "" };
    }
    return video._wcAnchor;
  }
  function elementFor(video) {
    const a = anchorFor(video);
    return { id: elementId(video), srcs: directUrls(video), srcKey: a.srcKey, postId: a.postId, streamId: streamIdFor(video), since: a.since, now: Date.now() };
  }
  function filenameFromUrl(u) {
    try { const p = new URL(u).pathname; const last = p.split("/").pop(); return last ? decodeURIComponent(last) : u; } catch (e) { return u.split("/").pop() || u; }
  }
  function candidateLabel(c) { return `${filenameFromUrl(c.url)} (${c.type || "media"})`; }
  function fetchCandidates(cb, video) {
    let done = false;
    const finish = (list) => { if (!done) { done = true; cb(list || []); } };
    setTimeout(() => finish([]), 900);
    try {
      const msg = { type: "getCandidates" };
      if (video) msg.element = elementFor(video);
      const maybe = api.runtime.sendMessage(msg, (r) => finish(r && r.candidates));
      if (maybe && maybe.then) maybe.then((r) => finish(r && r.candidates), () => finish([]));
    } catch (e) { finish([]); }
  }
  function pageUrl() { return location.href; }
  function labelFor(el, url) {
    const q = el.getAttribute && (el.getAttribute("data-quality") || el.getAttribute("label") || el.getAttribute("title"));
    return q ? `${q} — ${url.split("/").pop()}` : url.split("/").pop();
  }
  function ensurePanel() {
    let panel = document.getElementById("downloadcraft-panel");
    if (panel) return panel;
    panel = document.createElement("div");
    panel.id = "downloadcraft-panel";
    panel.style.cssText = "position:absolute;z-index:2147483647;background:#222222;color:#E8E8E8;font-family:\"Inter\",system-ui,-apple-system,\"Segoe UI\",sans-serif;font-size:11px;border:1px solid #3D3D3D;border-radius:6px;padding:8px;max-width:480px;min-width:260px;box-sizing:border-box;";
    panel.innerHTML = '<div style="display:flex;justify-content:flex-end;align-items:center;margin-bottom:4px;"><button class="dc-close" style="background:#2A2A2A;color:#E8E8E8;border:1px solid #3D3D3D;border-radius:6px;padding:0 8px;height:24px;cursor:pointer;font:inherit;">×</button></div><div class="downloadcraft-items"></div>';
    panel.querySelector(".dc-close").addEventListener("click", () => panel.remove());
    document.documentElement.appendChild(panel);
    return panel;
  }
  function fmtSize(n) { if (!n) return ""; if (n >= 1048576) return (n / 1048576).toFixed(1) + " MB"; if (n >= 1024) return (n / 1024).toFixed(1) + " KB"; return n + " B"; }
  // Ask box removed: captured downloads go straight to webcraft modal.
  function ensureConfirm(msg) { /* legacy no-op */ }
  function pageItem(video) {
    const pu = permalinkFor(video || null);
    return { url: pu, pageUrl: pu, intent: "page", label: "▶ This video — webcraft (choose quality / audio / subs)", filename: filenameFromUrl(pu), type: "page" };
  }
  function showPanelFixed(items, note) { showPanel(document.body, items, note); }
  function showPanel(anchor, items, note) {
    // Empty: note-only panel, no fake clickable item (clicking page URL as
    // stream was wrong). Non-clickable note text only.
    if (!items.length && !note) note = "No streams detected for this video";
    const panel = ensurePanel();
    const box = panel.querySelector(".downloadcraft-items");
    box.innerHTML = "";
    if (note) { const hd = document.createElement("div"); hd.textContent = note; hd.style.cssText = "padding:2px 6px 4px;color:#7FB3AA;font-weight:600;"; box.appendChild(hd); }
    items.forEach((it, i) => {
      const b = document.createElement("button");
      b.textContent = it.label;
      if (i === 0) { b.setAttribute("aria-selected", "true"); b.dataset.selected = "1"; }
      b.style.cssText = "display:block;width:100%;text-align:left;margin:2px 0;cursor:pointer;background:#2A2A2A;color:#E8E8E8;border:1px solid #3D3D3D;border-radius:6px;padding:4px 8px;min-height:24px;font:inherit;box-sizing:border-box;" + (i === 0 ? "background:#2A3C38;border-color:#7FB3AA;" : "");
      b.addEventListener("mouseenter", () => { b.style.background = b.dataset.selected ? "#2A3C38" : "#333333"; b.style.borderColor = b.dataset.selected ? "#7FB3AA" : "#555555"; });
      b.addEventListener("mouseleave", () => { b.style.background = b.dataset.selected ? "#2A3C38" : "#2A2A2A"; b.style.borderColor = b.dataset.selected ? "#7FB3AA" : "#3D3D3D"; });
      b.addEventListener("click", () => { api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: it.url, intent: it.intent || "stream", pageUrl: it.pageUrl || pageUrl(), pageTitle: document.title, filename: it.filename || filenameFromUrl(it.url), mime: it.mime || it.type || "" }); panel.remove(); });
      box.appendChild(b);
    });
    // Anchor under video's download button when VIDEO anchor; else anchor rect.
    const btnAnchor = (() => { const v = btnFor.get(anchor); return v && v !== anchor ? v : null; })();
    const br = (btnAnchor || anchor).getBoundingClientRect();
    let left = window.scrollX + br.left;
    let top = window.scrollY + br.bottom + 4;
    if (anchor === document.body) { left = window.scrollX + 8; top = window.scrollY + 8; }
    const maxLeft = window.scrollX + document.documentElement.clientWidth - panel.offsetWidth - 4;
    panel.style.left = (Math.max(window.scrollX + 4, Math.min(left, maxLeft))) + "px";
    panel.style.top = top + "px";
  }
  
  // Page-declared media invisible to <video> scan: og:/twitter: meta,
  // JSON-LD VideoObject, embed/object URLs. Blob:/data: excluded.
  function pageMediaUrls() {
    const out = [];
    const push = (v) => {
      if (!v || typeof v !== "string") return;
      try { const u = new URL(v.trim().replace(/\\\//g, "/"), location.href).href; if (/^https?:$/.test(new URL(u).protocol) && !/\.(json|js|mjs|css|html?|map)($|[?#])/i.test(u)) out.push(u); } catch (e) {}
    };
    document.querySelectorAll('meta[property="og:video"],meta[property="og:video:url"],meta[property="og:video:secure_url"],meta[name="twitter:player:stream"]').forEach((m) => push(m.getAttribute("content")));
    document.querySelectorAll('script[type="application/ld+json"]').forEach((s) => {
      try {
        const walk = (n) => { if (!n || typeof n !== "object") return; if (Array.isArray(n)) { n.forEach(walk); return; } const t = n["@type"]; if (t === "VideoObject" || (Array.isArray(t) && t.includes("VideoObject"))) push(n.contentUrl); Object.values(n).forEach(walk); };
        walk(JSON.parse(s.textContent || "null"));
      } catch (e) {}
    });
    // No iframe/embed/object: every frame runs own content script; those srcs
    // are player pages/ads/captcha, not downloadable media.
    return [...new Set(out)];
  }
  function directUrls(video) {
    const urls = [];
    if (video.currentSrc) urls.push(video.currentSrc);
    if (video.src) urls.push(video.src);
    video.querySelectorAll("source").forEach((s) => { if (s.src) urls.push(s.src); });
    return [...new Set(urls)].filter((u) => u && !u.startsWith("blob:") && !u.startsWith("data:"));
  }
  function idFromUrl(u) {
    const m = String(u || "").match(/\/status\/(\d+)|\/comments\/([a-z0-9]+)|[?&]v=([A-Za-z0-9_-]{6,})|youtu\.be\/([A-Za-z0-9_-]{6,})|\/shorts\/([A-Za-z0-9_-]{6,})|\/(?:reel|reels|p|tv)\/([A-Za-z0-9_-]{5,})|\/stories\/[^/]+\/(\d+)/i);
    return m ? (m[1] || m[2] || m[3] || m[4] || m[5] || m[6] || m[7]) : "";
  }
  function streamIdFromUrl(u) {
    const m = String(u || "").match(/amplify_video\/(\d+)|ext_tw_video\/(\d+)|v\.redd\.it\/([a-z0-9]+)/i);
    return m ? (m[1] || m[2] || m[3]) : "";
  }
  function isInstagram(u) {
    try { const h = u ? new URL(u, location.href).hostname : location.hostname; return /(^|\.)instagram\.com$/i.test(h); } catch (e) { return false; }
  }
  const CARD_SEL = "article,.thing,[data-permalink],[data-testid='tweet'],shreddit-post,ytd-rich-item-renderer,ytd-video-renderer,ytd-compact-video-renderer";
  function cardScope(video) {
    let el = video;
    for (let i = 0; i < 4 && el; i++) {
      if (el.closest) { const s = el.closest(CARD_SEL); if (s) return s; }
      const root = el.getRootNode && el.getRootNode();
      el = root && root.host ? root.host : (el.parentElement || null);
      if (el && el.closest) { const s = el.closest(CARD_SEL); if (s) return s; }
    }
    return null;
  }
  function streamIdFor(video) {
    try {
      const scope = video ? cardScope(video) : null;
      if (!scope) return "";
      const fromAttr = streamIdFromUrl((scope.getAttribute && scope.getAttribute("data-url")) || "");
      if (fromAttr) return fromAttr;
      const a = scope.querySelector && scope.querySelector('a[href*="v.redd.it"]');
      return a ? streamIdFromUrl(a.href || "") : "";
    } catch (e) { return ""; }
  }
  function cleanPermalink(u) {
    try {
      const x = new URL(u, location.href);
      x.hash = "";
      let m = x.pathname.match(/\/status\/(\d+)/);
      if (m) return x.origin + x.pathname.slice(0, x.pathname.indexOf(m[0]) + m[0].length);
      m = x.pathname.match(/\/comments\/([a-z0-9]+)/i);
      if (m) return x.origin + x.pathname.slice(0, x.pathname.indexOf(m[0]) + m[0].length);
      if (x.pathname === "/watch") return x.origin + "/watch?v=" + (x.searchParams.get("v") || "");
      [...x.searchParams.keys()].forEach((k) => x.searchParams.delete(k));
      return x.toString();
    } catch (e) { return String(u || location.href); }
  }
  function isVideoPageUrl(u) {
    // Instagram: host-scoped (path alone missed /p/, /reels/, /stories/, /tv/).
    if (isInstagram(u)) return /\/(reel|reels|p|tv)\/[A-Za-z0-9_-]+|\/stories\//.test(new URL(u, location.href).pathname || "");
    return /\/status\/\d+|\/comments\/[a-z0-9]+|\/watch\?v=|\/shorts\/|youtu\.be\/|\/videos\/|\/video\/|\.globo\.com\/.*\.ghtml|g1\.globo\.com/i.test(String(u || ""));
  }
  function permalinkFor(video) {
    try {
      const scope = video ? cardScope(video) : null;
      if (scope) {
        const dp = scope.getAttribute && scope.getAttribute("data-permalink");
        if (dp) return cleanPermalink(new URL(dp, location.origin).href);
        // Instagram: card <a href="/p|/reel|/reels|/tv/SHORTCODE/"> is the post permalink.
        if (isInstagram()) { const ia = scope.querySelector('a[href*="/p/"],a[href*="/reel/"],a[href*="/reels/"],a[href*="/tv/"]'); if (ia && cardScope(ia) === scope) return cleanPermalink(ia.href); }
        const links = [...scope.querySelectorAll('a[href*="/status/"],a[href*="/comments/"],a[href*="/watch?v="],a[href*="/shorts/"],a[href*="youtu.be/"]')];
        const own = links.filter((a) => cardScope(a) === scope);
        const timed = own.find((a) => a.querySelector("time"));
        if (timed) return cleanPermalink(timed.href);
        if (own.length) return cleanPermalink(own[0].href);
      }
    } catch (e) {}
    return cleanPermalink(location.href);
  }
  function boundedIdHit(url, pid) {
    if (!pid) return false;
    const esc = pid.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    return new RegExp("[/=?&._-]" + esc + "([/?&#._-]|$)").test(url);
  }
  function scopedCandidates(video, candidates) {
    const pid = idFromUrl(permalinkFor(video)) || idFromUrl(location.href);
    const streamId = streamIdFor(video);
    const since = video._wcFirstPlay || video._wcSeen || 0;
    const reddit = /(^|\.)reddit\.com$/.test(location.hostname);
    // Instagram single-post: sniffed CDN mp4s carry no shortcode; timing tier
    // (bg) already matched them. Accept video/* from IG CDNs for this video.
    const igSingle = isInstagram() && isVideoPageUrl(permalinkFor(video) || location.href);
    return candidates.filter((c) => {
      if (!c || !c.url || c.url.startsWith("blob:")) return false;
      const sid = streamIdFromUrl(c.url);
      const xSid = sid && !/v\.redd\.it/.test(c.url) ? sid : "";
      if (xSid && pid) return xSid === pid;
      if (streamId && boundedIdHit(c.url, streamId)) return true;
      if (pid && boundedIdHit(c.url, pid)) return true;
      if (xSid && !pid) return false;
      if (reddit && (pid || streamId)) return false;
      if (igSingle && /cdninstagram\.com|fbcdn\.net/i.test(c.url) && /video|mp4|mpegurl|dash/i.test(c.type || c.url)) return true;
      if (since && c.time) return c.time >= since - 2000 && c.time <= since + 45000;
      return false;
    });
  }
  // Effective rect: IG inserts blob/MSE <video> at 0x0 (layout pending) or
  // CSS-sizes only the wrapper. Fall back to nearest sized ancestor.
  function videoRect(video) {
    const r = video.getBoundingClientRect();
    if (r.width >= 120 && r.height >= 68) return r;
    let el = video.parentElement;
    for (let i = 0; i < 4 && el; i++) {
      const pr = el.getBoundingClientRect();
      if (pr.width >= 120 && pr.height >= 68) return pr;
      el = el.parentElement;
    }
    return r;
  }
  function pickStream(video) {
    // Rescan at click time: manifests/page media declared after load.
    scanManifests();
    const pid = idFromUrl(permalinkFor(video));
    // Page-global manifests/meta leak other posts streams: keep only this post.
    // Instagram: src/MSE is blob:, shortcode cannot bound-hit cdninstagram/fbcdn
    // URLs. On single-post page, page-declared CDN media belongs to that post.
    const scopedGlobal = /(^|\.)reddit\.com$/.test(location.hostname) ? [] : [...manifestUrls, ...pageMediaUrls()].filter((u) => pid && (boundedIdHit(u, pid) || (isInstagram() && isVideoPageUrl(location.href) && /cdninstagram\.com|fbcdn\.net/i.test(u))));
    const direct = [...new Set([...directUrls(video), ...scopedGlobal])];
    fetchCandidates(((candidates) => {
      // Bg element scoping primary; scopedCandidates second-pass safety.
      // If client rules empty bg src/id hits, trust bg (same tab+frame).
      const clientScoped = scopedCandidates(video, candidates);
      const scoped = clientScoped.length ? clientScoped : candidates;
      const mans = [], medias = [];
      scoped.forEach((c) => { if (c && c.url) (/mpd|m3u8|dash|mpegurl/i.test(c.url) || /mpegurl|dash/i.test(c.type || "") ? mans : medias).push(c); });
      const all = [...direct.map((u) => ({ url: u })), ...mans, ...medias];
      const items0 = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)(all);
      const note = items0.length ? "Streams for this video:" : "";
      const mapped = items0.map((c) => {
        const isMan = /mpd|m3u8|dash|mpegurl|DASHPlaylist|HLSPlaylist/i.test(c.url) || /mpegurl|dash/i.test(c.type || "");
        return { url: c.url, intent: isMan ? "manifest" : "stream", pageUrl: permalinkFor(video), label: (isMan ? "Playlist (yt-dlp/native) · " : "") + (c.label || candidateLabel(c)), filename: filenameFromUrl(c.url), type: c.type };
      });
      if (isVideoPageUrl(permalinkFor(video))) mapped.unshift(pageItem(video));
      showPanel(video, mapped, note);
    }), video);
  }
  const btnFor = new WeakMap();
  function positionBtn(video) {
    const btn = btnFor.get(video);
    if (!btn) return;
    const r = videoRect(video);
    if (r.width < 120 || r.height < 68) { btn.style.display = "none"; return; }
    btn.style.display = "";
    if (btn.dataset.dragged) return;
    btn.style.left = (r.left - 10) + "px";
    btn.style.top = (r.top - 10) + "px";
  }
  function attachBtn(video) {
    let btn = btnFor.get(video);
    // React re-renders can drop our button while video stays connected:
    // re-anchor instead of trusting registry.
    if (btn && !btn.isConnected) { (video.parentElement || document.body || document.documentElement).appendChild(btn); positionBtn(video); return btn; }
    if (btn) { positionBtn(video); return btn; }
    btn = document.createElement("button");
    btn.title = "Download video";
    btn.setAttribute("aria-label", "Download video");
    btn.setAttribute("data-downloadcraft-btn", "1");
    btn.innerHTML = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" style="display:block;margin:7px auto;pointer-events:none"><path d="M12 3v11"/><path d="m7 10 5 5 5-5"/><path d="M4 20h16"/></svg>';
    btn.style.cssText = "position:fixed;z-index:2147483646;width:32px;height:32px;border-radius:50%;background:#4E7A74;color:#fff;border:1px solid #7FB3AA;padding:0;cursor:grab;text-align:center;opacity:0;pointer-events:none;display:none;touch-action:none;user-select:none;-webkit-user-select:none;font-family:\"Inter\",system-ui,sans-serif;";
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
    // Discreet elsewhere: invisible until pointer over video or button itself;
    // hidden state must not swallow clicks meant for video controls.
    // Instagram: always semi-visible — hover-only proved undiscoverable there
    // (overlays + touch); still clickable at rest on IG.
    const showBtn = () => { btn.style.opacity = ".9"; btn.style.pointerEvents = "auto"; };
    const hideBtn = () => { if (isInstagram(location.href)) { btn.style.opacity = ".55"; btn.style.pointerEvents = "auto"; } else { btn.style.opacity = "0"; btn.style.pointerEvents = "none"; } };
    // Hover on player wrapper: overlays cover <video> (X/YouTube/Instagram).
    // Delegated mouseover: overlay siblings (IG action rail/gradient) never
    // fire mouseenter on wrapper/video, so check containment per event.
    // Coordinate hover (overlay-proof): IG gradient/action-rail layers are
    // siblings covering video, containment checks never fire for them.
    // Hit-test pointer coords vs effective rect, ignore event target.
    const onHover = (e) => { const r = videoRect(video); const inside = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom; if (e.target === btn || inside) { positionBtn(video); showBtn(); } else hideBtn(); };
    document.addEventListener("mouseover", onHover, true);
    document.addEventListener("mousemove", onHover, true);
    // Discoverability: brief reveal on playback start (pointer may sit still
    // over video, no mousemove fires). Touch/no-hover: also flash on attach.
    let flashTimer = null;
    const flashBtn = () => { positionBtn(video); showBtn(); if (flashTimer) clearTimeout(flashTimer); flashTimer = setTimeout(hideBtn, 2500); };
    video.addEventListener("playing", flashBtn);
    if (window.matchMedia && matchMedia("(hover: none)").matches) { flashBtn(); video.addEventListener("loadedmetadata", flashBtn); video.addEventListener("play", flashBtn); }
    btn.addEventListener("mouseenter", showBtn);
    btn.addEventListener("mouseleave", hideBtn);
    btn.addEventListener("focus", showBtn);
    btn.addEventListener("blur", hideBtn);

    btnFor.set(video, btn);
    btnForEntries.push([video, btn]);
    if (!video._wcSeen) video._wcSeen = Date.now();
    // Fullscreen renders only the fullscreen element subtree: a body-level
    // button vanishes there (IG viewer). Anchor inside video parent when
    // possible (fixed positioning unaffected by ancestor), body fallback.
    (video.parentElement || document.body || document.documentElement).appendChild(btn);
    positionBtn(video);
    hideBtn();
    return btn;
  }
  function scanVideo(video) {
    if (wired.has(video)) { attachBtn(video); return; }
    wired.add(video);
    // Button on poster/metadata too: paused X/Reddit videos never fire
    // `playing`, and loop+muted+autoplay IS the X feed video shape.
    const isBig = () => { const r = videoRect(video); return r.width >= 120 && r.height >= 68; };
    const show = () => { if (isBig()) attachBtn(video); };
    if (isBig() || video.poster || video.readyState >= 1 || video.currentSrc || !video.paused) attachBtn(video);
    video.addEventListener("loadstart", () => { video._wcFirstPlay = 0; video._wcSeen = Date.now(); video._wcAnchor = null; });
    video.addEventListener("playing", () => { if (!video._wcFirstPlay) { video._wcFirstPlay = Date.now(); anchorFor(video); } show(); });
    video.addEventListener("loadedmetadata", show);
    video.addEventListener("loadeddata", show);
    video.addEventListener("canplay", show);
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
  const btnForEntries = [];
  function allVideos(root) {
    const out = [];
    const walk = (r) => {
      r.querySelectorAll("video").forEach((v) => out.push(v));
      r.querySelectorAll("*").forEach((el) => { if (el.shadowRoot) walk(el.shadowRoot); });
    };
    walk(root || document);
    return out;
  }
  function scan() { allVideos().forEach(scanVideo); } // video only: audio via context menu // scanManifests only at click: full innerHTML regex per observer scan = biggest page cost
  // Debounced: full-page rescan on every DOM mutation is too expensive.
  let pending = null;
  const mo = new MutationObserver(() => { if (pending) return; pending = setTimeout(() => { pending = null; scan(); for (let i = btnForEntries.length - 1; i >= 0; i--) { const [v, b] = btnForEntries[i]; if (!v.isConnected) { b.remove(); btnForEntries.splice(i, 1); } } document.querySelectorAll("video, audio").forEach(positionBtn); }, 400); });
  window.addEventListener("resize", () => document.querySelectorAll("video, audio").forEach(positionBtn), { passive: true });
  // Capture: IG reels scroll inner snap containers, no window scroll fires.
  window.addEventListener("scroll", () => document.querySelectorAll("video, audio").forEach(positionBtn), { passive: true, capture: true });
  document.addEventListener("fullscreenchange", () => document.querySelectorAll("video, audio").forEach(positionBtn));
  mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["src", "poster"] });
  scan();
  api.runtime.onMessage.addListener((msg) => {
    // Sniffed candidates never touch UI: panel opens only on user click.
    if (msg.type === "downloadcraft-confirm" && self === top) ensureConfirm(msg);
    if (msg.type === "downloadcraft-scan") {
      let anchor = null, best = 0;
      allVideos().forEach((v) => {
        const r = v.getBoundingClientRect();
        const a = r.width * r.height;
        if (r.width >= 120 && r.height >= 68 && a > best) { best = a; anchor = v; }
      });
      if (anchor) { attachBtn(anchor); pickStream(anchor); return; }
      else {
        // No media element: page row always offered (yt-dlp), fixed top-right.
        scanManifests();
        fetchCandidates((candidates) => {
          const byUrl = new Map();
          [...manifestUrls, ...pageMediaUrls()].forEach((u) => byUrl.set(u, { url: u, label: filenameFromUrl(u), filename: filenameFromUrl(u), type: "" }));
          candidates.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type }); });
          const items = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)([...byUrl.values()]);
          const mapped = items.map((c) => { const isMan = /mpd|m3u8|dash|mpegurl|DASHPlaylist|HLSPlaylist/i.test(c.url) || /mpegurl|dash/i.test(c.type || ""); return { url: c.url, intent: isMan ? "manifest" : "stream", pageUrl: pageUrl(), label: (isMan ? "Playlist (yt-dlp/native) · " : "") + (c.label || candidateLabel(c)), filename: filenameFromUrl(c.url), type: c.type }; });
          if (isVideoPageUrl(location.href)) mapped.unshift(pageItem(null));
          showPanelFixed(mapped, items.length ? "" : "No video found on page.");
        });
      }
      return;
    }
    if (msg.type === "downloadcraft-grab-all" && self === top) {
      // SharePoint/OneDrive: file rows virtualized, no file <a href>. Hand
      // page to Site Grabber with tab cookies; Rust lists folder via REST.
      if (/\.sharepoint\.com$/i.test(location.hostname)) { api.runtime.sendMessage({ type: "downloadcraft-grab-site" }); return; }
      // IDM "Download all links": <a href> with downloadable extension, deduped.
      // Extension may sit in path (file.mp4/view) or query (download.aspx?SourceUrl=x.mp4).
      const seenUrls = new Set();
      const send = (u) => { if (u && !seenUrls.has(u)) { seenUrls.add(u); api.runtime.sendMessage({ type: "downloadcraft-download", url: u, pageTitle: document.title }); } };
      document.querySelectorAll("a[href]").forEach((a) => {
        const u = a.href;
        if (!u || seenUrls.has(u)) return;
        if (/\.(zip|rar|7z|gz|bz2|xz|pdf|exe|msi|dmg|deb|apk|iso|tar|tgz|mp4|mp3|mkv|webm|flac|wav|mov|avi|epub|docx?|xlsx?|pptx?|csv|txt|png|jpe?g|gif|webp|svg)(\?|#|\/|$)/i.test(u)) { send(u); return; }
        try {
          const x = new URL(u);
          // SharePoint viewer links: id=/file=/SourceUrl= carries real file path.
          const inner = x.searchParams.get("SourceUrl") || x.searchParams.get("id") || x.searchParams.get("file") || "";
          const webBase = x.origin + (x.pathname.split("/_layouts/")[0] || "");
          if (/\.[a-z0-9]{2,8}$/i.test(inner) && !/\.(aspx|html?)(\?|#|$)/i.test(inner)) send(webBase + "/_layouts/15/download.aspx?SourceUrl=" + encodeURIComponent(inner.startsWith("/") ? inner : "/" + inner));
        } catch (e) {}
      });
    }
  });
})();
