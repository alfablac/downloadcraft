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
      video._wcAnchor = { since: video._wcFirstPlay || video._wcSeen || 0, postId: idFromUrl(permalinkFor(video)), srcKey: srcs[0] || "" };
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
    if (panel) {
      // Fullscreen renders only its subtree: re-home open panel on FS change.
      const fs = document.fullscreenElement;
      if (fs && !fs.contains(panel)) fs.appendChild(panel);
      else if (!fs && panel.parentElement !== document.documentElement) document.documentElement.appendChild(panel);
      return panel;
    }
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
    if (!pu) return null;
    const kind = typeof videoPageKind === "function" ? videoPageKind(pu) : (isVideoPageUrl(pu) ? "page" : "other");
    // Registry page: always offer. Other: a per-video anchor (wrapping/timed
    // link) already proved this URL is the item; generic yt-dlp resolves it.
    // Feed/other with no per-video anchor: never offer (no x.com/home repeat).
    if (kind !== "page" && !video) return null;
    if (kind === "feed") return null;
    return { url: pu, pageUrl: pu, intent: "page", label: "▶ This video — webcraft (choose quality / audio / subs)", filename: filenameFromUrl(pu), type: "page" };
  }
  // Declarative player evidence with no <video> yet: Globo G1 renders
  // <bs-player videoid="..."> and only mounts <video> after play. Presence
  // proves a video lives here even while no element/media URL exists.
  // Never offer the microdata MP4 (globo vodstreaming): it 400s standalone
  // and yt-dlp's GloboArticle crashes claiming *.globo.com MP4s.
  function hasPlayerEvidence() {
    try {
      return !!document.querySelector("bs-player[videoid],[data-video-id],[data-videoid],amp-video,[data-globo-video]");
    } catch (e) { return false; }
  }
  // Embedded player pages: cross-origin iframe content is invisible to this
  // frame's <video> scan, but the embed src (youtube/youtube-nocookie
  // /embed/, vimeo player, dailymotion /embed/, globoplay /v/) is itself a
  // registry "page" URL. Offer it — canonicalized (nocookie embed -> watch
  // URL) — as intent="page". Deliberately NOT part of pageMediaUrls():
  // embeds are page offers for yt-dlp, never streams.
  function embedPageItems() {
    try {
      const out = [], seen = new Set();
      document.querySelectorAll("iframe[src],embed[src],object[data]").forEach((el) => {
        const raw = el.getAttribute("src") || el.getAttribute("data") || "";
        if (!raw) return;
        let abs = "";
        try { abs = new URL(raw, location.href).href; } catch (e) { return; }
        if (!/^https?:/.test(abs)) return;
        const cleaned = cleanPermalink(abs);
        if (!cleaned || typeof videoPageKind !== "function" || videoPageKind(cleaned) !== "page") return;
        if (sameUrl(cleaned, location.href) || seen.has(cleaned)) return;
        seen.add(cleaned);
        out.push({ url: cleaned, pageUrl: cleaned, intent: "page", label: "▶ Embedded video — webcraft (choose quality / audio / subs)", filename: filenameFromUrl(cleaned), type: "page" });
      });
      return out;
    } catch (e) { return []; }
  }
  // Toolbar on a non-registry page that still shows media evidence: page is
  // the only yt-dlp handle (blob:/MSE src is useless). Feeds excluded.
  // Registry "page" (e.g. globo .ghtml) is itself proof: player evidence
  // only required off-registry.
  function genericPageItem() {
    try {
      const loc = cleanPermalink(location.href);
      if (!loc || isFeedHome(loc)) return null;
      const kind = typeof videoPageKind === "function" ? videoPageKind(loc) : "other";
      if (kind === "feed") return null;
      if (kind !== "page" && !allVideos().length && !pageMediaUrls().length && !hasPlayerEvidence() && !embedPageItems().length) return null;
      return { url: loc, pageUrl: loc, intent: "page", label: "▶ This page — webcraft (choose quality / audio / subs)", filename: filenameFromUrl(loc), type: "page" };
    } catch (e) { return null; }
  }
  function showPanel(anchor, items, note) {
    // Defensive fallback (Shorts report): on a registry/generic video page
    // the page item must be reachable even if per-video stream scoping came
    // up empty (recycled reel element, late permalink, ...). Only the true
    // dead end (feed, or no media evidence at all) keeps the note.
    if (!items.length && !note) {
      const fb = pageItem(null) || genericPageItem() || embedPageItems()[0];
      if (fb) items = [fb];
    }
    // Empty: note-only panel, no fake clickable item, no silent return.
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
    const VIDEO = btnFor.get(anchor);
    let left = 0;
    (() => {
      const br = (VIDEO || anchor).getBoundingClientRect();
      left = window.scrollX + br.left;
      panel.style.left = left + "px";
      panel.style.top = (window.scrollY + br.bottom + 4) + "px";
    })();
    requestAnimationFrame(() => {
      const maxLeft = window.scrollX + document.documentElement.clientWidth - panel.offsetWidth - 4;
      panel.style.left = Math.max(window.scrollX + 4, Math.min(left, maxLeft)) + "px";
    });
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
    // Registry first (Threads/TikTok/Vimeo/Twitch/... ids); inline fallback
    // only when rank.js failed to load.
    if (typeof videoIdOf === "function") { const id = videoIdOf(u); if (id) return id; }
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
  // Meta family (IG/FB/Threads): blob:/MSE players, 0x0 <video>, overlays,
  // CDN urls carry no post id -> host-scoped correlation like Instagram.
  function isMeta(u) {
    try { const h = u ? new URL(u, location.href).hostname : location.hostname; return /(^|\.)(instagram\.com|facebook\.com|threads\.net|threads\.com)$/i.test(h) || /(^|\.)fb\.watch$/i.test(h); } catch (e) { return false; }
  }
  function siteName() { return typeof siteOf === "function" ? siteOf(location.href) : ""; }
  const CARD_SEL = "article,.thing,[data-permalink],[data-testid='tweet'],shreddit-post,ytd-rich-item-renderer,ytd-video-renderer,ytd-compact-video-renderer,[data-testid='post-container']";
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
  // Tracking params stripped everywhere (query may carry video id,
  // e.g. /watch?v= survives only via site-shaped branch below).
  const TRACK_PARAMS = ["fbclid", "gclid", "igshid", "s", "t", "ref", "ref_src", "ref_url", "mc_cid", "mc_eid", "spm", "si", "_hsenc", "_hsmi", "vero_id"];
  // YouTube embed/player URLs (youtube.com/embed/<id>, youtube-nocookie,
  // youtu.be/<id>) canonicalize to the watch page yt-dlp knows best.
  function canonicalVideoPage(u) {
    try {
      const x = new URL(String(u || ""), location.href);
      const h = x.hostname.toLowerCase().replace(/^(www\.|m\.|music\.)/, "");
      if (h === "youtu.be" || h === "youtube.com" || h === "youtube-nocookie.com") {
        const id = (typeof videoIdOf === "function" ? videoIdOf(x.href) : "") || "";
        if (id && (/^\/(embed|v|live|shorts)\//.test(x.pathname) || h === "youtu.be")) return "https://www.youtube.com/watch?v=" + id;
      }
    } catch (e) {}
    return "";
  }
  function cleanPermalink(u) {
    try {
      if (!u) return "";
      const canon = canonicalVideoPage(u);
      if (canon) return canon;
      const x = new URL(u, location.href);
      x.hash = "";
      const stripHost = (re) => { const m = x.pathname.match(re); return m ? x.origin + x.pathname.slice(0, x.pathname.indexOf(m[0]) + m[0].length) : null; };
      // Truncate at video-claiming segment (drops /likes, /replies, titles,
      // ?t= alike) so yt-dlp gets canonical per-video page, per site.
      const site = typeof siteOf === "function" ? siteOf(x.href) : "";
      const cuts = {
        x: [/\/status\/\d+/, /\/i\/videos?\/\d+/, /\/videos?\/\d+/],
        reddit: [/\/comments\/[a-z0-9]+/i],
        threads: [/@[^/]+\/post\/[A-Za-z0-9_-]+/],
        instagram: [/\/(?:reel|reels|p|tv)\/[A-Za-z0-9_-]+/, /\/stories\/[^/]+\/\d+/],
        tiktok: [/@[^/]+\/(?:video|photo)\/\d+/, /\/(?:video|photo)\/\d+/],
        twitch: [/\/videos\/\d+/, /\/[^/]+\/clip\/[A-Za-z0-9_-]+/, /clips\.twitch\.tv\/.+/],
        vimeo: [/\/\d+/],
        dailymotion: [/\/(?:video|embed\/video)\/[a-z0-9]+/],
        bilibili: [/\/video\/[^/]+/],
      }[site];
      if (cuts) for (const re of cuts) { const hit = stripHost(re); if (hit) return hit; }
      let m = x.pathname.match(/\/status\/(\d+)/);
      if (m) return x.origin + x.pathname.slice(0, x.pathname.indexOf(m[0]) + m[0].length);
      m = x.pathname.match(/\/comments\/([a-z0-9]+)/i);
      if (m) return x.origin + x.pathname.slice(0, x.pathname.indexOf(m[0]) + m[0].length);
      if (x.pathname === "/watch") return x.origin + "/watch?v=" + (x.searchParams.get("v") || "");
      [...x.searchParams.keys()].forEach((k) => { if (/^utm_/i.test(k) || TRACK_PARAMS.includes(k.toLowerCase())) x.searchParams.delete(k); });
      return x.toString();
    } catch (e) { return ""; }
  }
  function sameUrl(a, b) {
    try { return !!a && cleanPermalink(a) === cleanPermalink(b); } catch (e) { return false; }
  }
  function isVideoPageUrl(u) {
    // Registry is source of truth (Threads/Twitch clips/Vimeo channels/...);
    // legacy regex only when rank.js failed to load.
    if (typeof videoPageKind === "function") return videoPageKind(u) === "page";
    if (isInstagram(u)) return /\/(reel|reels|p|tv)\/[A-Za-z0-9_-]+|\/stories\//.test(new URL(u, location.href).pathname || "");
    return /\/status\/\d+|\/comments\/[a-z0-9]+|\/watch\?v=|\/shorts\/|youtu\.be\/|\/videos\/|\/video\/|\.globo\.com\/.*\.ghtml|g1\.globo\.com/i.test(String(u || ""));
  }
  // Generic per-video scope: walk up from <video> (shadow-host hops like
  // cardScope) collecting ancestors owning exactly this one video. A
  // container owning >1 video is feed-level, never bounds search. Depth-
  // bounded for big feeds. Ancestors only -- no global coordinate /
  // ancestor-rect fallback (v0.22.2 regression).
  function scopeChain(video, limit) {
    const chain = [];
    const max = limit || 6;
    let el = video;
    for (let i = 0; i < 10 && el && chain.length < max; i++) {
      if (el.closest) { const s = el.closest(CARD_SEL); if (s && !chain.includes(s)) { chain.push(s); if (chain.length >= max) break; } }
      const root = el.getRootNode && el.getRootNode();
      el = root && root.host ? root.host : (el.parentElement || null);
      if (el) {
        let n = 0;
        try { n = el.querySelectorAll ? el.querySelectorAll("video").length : 0; } catch (e) {}
        if (n === 1 && !chain.includes(el)) chain.push(el);
        else if (n > 1) break;
      }
    }
    return chain;
  }
  // "2 hours ago" / "Jan 3, 2024" / "2024-01-03 10:20" / "3h" time-ish text.
  function isTimeText(t) {
    t = String(t || "").trim();
    if (!t) return false;
    if (/\d/.test(t) && /(ago|am\b|pm\b|:|\/|\b(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)\b|\d{4})/i.test(t)) return true;
    return /^\d+\s?(s|m|h|d|w)$/i.test(t);
  }
  // Timestamp/permalink-flavored link inside scope: wraps <time>, has
  // rel~=bookmark, or class/aria-label/title mentions permalink/timestamp;
  // visible text (or time[datetime]) must parse as time/date anyway.
  function timestampLink(scope) {
    try {
      for (const a of [...scope.querySelectorAll("a[href]")]) {
        const time = a.querySelector("time");
        const cls = ((a.className && a.className.baseVal !== undefined) ? a.className.baseVal : a.className) || "";
        const flavored = !!time || /(^|\s)bookmark(\s|$)/i.test(a.getAttribute("rel") || "") || /permalink|timestamp|time/i.test(cls + " " + (a.getAttribute("aria-label") || "") + " " + (a.getAttribute("title") || ""));
        if (!flavored) continue;
        const text = time ? (time.getAttribute("datetime") || time.textContent || "") : (a.textContent || "");
        if (text && (isTimeText(text) || (time && time.getAttribute("datetime")))) return a.href || "";
      }
    } catch (e) {}
    return "";
  }
  // Useable item link: absolute http(s), same origin, not current page,
  // not bare fragment/javascript link. Returns cleaned URL or "".
  function itemHref(raw) {
    try {
      if (!raw) return "";
      const s = String(raw).trim();
      if (!s || s.startsWith("#") || /^javascript:/i.test(s)) return "";
      const u = new URL(s, location.href);
      if (!/^https?:$/.test(u.protocol) || u.origin !== location.origin) return "";
      if (sameUrl(u.href, location.href)) return "";
      return cleanPermalink(u.href);
    } catch (e) { return ""; }
  }
  function isFeedHome(u) {
    // Registry feeds (youtube.com/feed, /explore, channel/grid pages) join
    // bare roots: a feed location is never the item.
    if (typeof videoPageKind === "function" && videoPageKind(u) === "feed") return true;
    try { const p = new URL(u, location.href).pathname.replace(/\/+$/, "").toLowerCase(); return p === "" || p === "/home" || p === "/feed"; } catch (e) { return false; }
  }
  // Generic-first per-video item URL. Chain: 1a wrapping <a>; 1b timestamp/
  // permalink link in nearest single-video scope; 1c scope data-* attrs,
  // then nearest differing same-origin <a> (last path segment with digit,
  // != current page segment preferred); 1d og:url/canonical pointing
  // elsewhere non-root; 1e v0.22.4 site signals (CARD_SEL data-permalink,
  // IG post link, /status/ /comments/ /watch?v= /shorts/ youtu.be, timed
  // links); 1f cleaned location ONLY on single-video pages. Multi-video
  // page, no per-video link: "" (unknown) -- NEVER feed/home URL.
  function permalinkFor(video) {
    try {
      if (!video) {
        // No element context (toolbar scan): location is item only when it
        // is not a root/home page; callers gate with isVideoPageUrl.
        const loc = cleanPermalink(location.href);
        return isFeedHome(loc) ? "" : loc;
      }
      // Registry "page" (Globo .ghtml, YouTube watch/embed, single-post
      // pages): the location IS the item — scope-link hunting (related-
      // article links inside the same <article>) must never win over it.
      // Feeds/other pages keep the per-card chain below.
      {
        const loc = cleanPermalink(location.href);
        if (typeof videoPageKind === "function" && videoPageKind(loc) === "page" && !isFeedHome(loc)) return loc;
      }
      const multi = allVideos().length > 1;
      // 1a. <a> wrapping video itself (closest crosses shadow hosts too).
      try { const a = video.closest && video.closest("a[href]"); if (a) { const h = itemHref(a.getAttribute("href")); if (h) return h; } } catch (e) {}
      const chain = scopeChain(video);
      // 1b. Timestamp/permalink link, nearest scope first.
      for (const scope of chain) { const h = itemHref(timestampLink(scope)); if (h) return h; }
      // 1c. Scope data attrs, then nearest differing link.
      const curSeg = location.pathname.split("/").filter(Boolean).pop() || "";
      for (const scope of chain) {
        if (scope.getAttribute) for (const attr of ["data-permalink", "data-url", "data-href"]) { const h = itemHref(scope.getAttribute(attr)); if (h) return h; }
        try {
          const links = [...scope.querySelectorAll("a[href]")];
          const pick = links.find((a) => { const h = itemHref(a.getAttribute("href")); if (!h) return false; const seg = new URL(h).pathname.split("/").filter(Boolean).pop() || ""; return /\d/.test(seg) && seg !== curSeg; }) || links.find((a) => itemHref(a.getAttribute("href")));
          if (pick) return itemHref(pick.getAttribute("href"));
        } catch (e) {}
      }
      // 1d. og:url / canonical pointing at different non-root item.
      try {
        const meta = document.querySelector('meta[property="og:url"]');
        const canon = document.querySelector('link[rel="canonical"]');
        const cand = (meta && meta.getAttribute("content")) || (canon && canon.getAttribute("href")) || "";
        const h = itemHref(cand);
        if (h && !isFeedHome(h)) return h;
      } catch (e) {}
      // 1e. Site-shaped fallback (v0.22.4 behavior).
      const scope = cardScope(video);
      if (scope) {
        const dp = scope.getAttribute && scope.getAttribute("data-permalink");
        if (dp) return cleanPermalink(new URL(dp, location.origin).href);
        // Instagram: card <a href="/p|/reel|/reels|/tv/SHORTCODE/"> is post permalink.
        if (isInstagram()) { const ia = scope.querySelector('a[href*="/p/"],a[href*="/reel/"],a[href*="/reels/"],a[href*="/tv/"]'); if (ia && cardScope(ia) === scope) return cleanPermalink(ia.href); }
        const links = [...scope.querySelectorAll('a[href*="/status/"],a[href*="/comments/"],a[href*="/watch?v="],a[href*="/shorts/"],a[href*="youtu.be/"]')];
        const own = links.filter((a) => cardScope(a) === scope);
        const timed = own.find((a) => a.querySelector("time"));
        if (timed) return cleanPermalink(timed.href);
        if (own.length) return cleanPermalink(own[0].href);
      }
      // 1f. Single-video page: page itself is item. Registry page is item
      // even in multi-video shells (Tw provider/consent iframes add a 2nd
      // <video> on content pages that 1f would otherwise reject).
      if (!multi || (typeof videoPageKind === "function" && videoPageKind(cleanPermalink(location.href)) === "page")) { const loc = cleanPermalink(location.href); return isFeedHome(loc) ? "" : loc; }
      // 1g. Registry page on multi-video shell fell through (no per-video
      // link): location is still the item (og:video sites above covered in
      // 1d only when meta differs; equal-meta hits itemHref same-page gate).
      if (typeof videoPageKind === "function" && videoPageKind(cleanPermalink(location.href)) === "page") { const loc = cleanPermalink(location.href); if (!isFeedHome(loc)) return loc; }
    } catch (e) {}
    return "";
  }
  function boundedIdHit(url, pid) {
    if (!pid) return false;
    const esc = pid.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    return new RegExp("[/=?&._-]" + esc + "([/?&#._-]|$)").test(url);
  }
  function scopedCandidates(video, candidates) {
    const pid = idFromUrl(permalinkFor(video)) || (allVideos().length <= 1 ? idFromUrl(location.href) : "");
    const streamId = streamIdFor(video);
    const since = video._wcFirstPlay || video._wcSeen || 0;
    const reddit = /(^|\.)reddit\.com$/.test(location.hostname);
    // Single-post registry page: sniffed CDN mp4s carry no page id (Meta
    // CDNs, googlevideo, twimg...); timing tier (bg) matched them. Accept
    // video/manifest candidates from this site's CDN pair for this video.
    const single = isVideoPageUrl(permalinkFor(video) || location.href);
    const pageHost = location.hostname.toLowerCase();
    return candidates.filter((c) => {
      if (!c || !c.url || c.url.startsWith("blob:")) return false;
      const sid = streamIdFromUrl(c.url);
      const xSid = sid && !/v\.redd\.it/.test(c.url) ? sid : "";
      if (xSid && pid) return xSid === pid;
      if (streamId && boundedIdHit(c.url, streamId)) return true;
      if (pid && boundedIdHit(c.url, pid)) return true;
      if (xSid && !pid) return false;
      if (reddit && (pid || streamId)) return false;
      if (single) {
        try { const ch = new URL(c.url).hostname.toLowerCase(); if ((typeof hostMatchesScope === "function" ? hostMatchesScope(pageHost, ch) : /cdninstagram\.com|fbcdn\.net/i.test(c.url)) && /video|mp4|mpegurl|dash/i.test(c.type || c.url)) return true; } catch (e) {}
      }
      if (since && c.time) return c.time >= since - 2000 && c.time <= since + 45000;
      return false;
    });
  }
  // Hover host: nearest sized wrapper (site overlays cover <video> itself).
  function playerRoot(video) {
    try {
      const vr = video.getBoundingClientRect();
      let host = video;
      let el = video.parentElement;
      for (let i = 0; i < 4 && el; i++) {
        const r = el.getBoundingClientRect();
        if (Math.abs(r.width - vr.width) <= 12 && Math.abs(r.height - vr.height) <= 12 && r.width >= 40) { host = el; el = el.parentElement; } else break;
      }
      return host;
    } catch (e) { return video; }
  }
  // Effective rect: Meta inserts blob/MSE <video> at 0x0 (layout pending) or
  // CSS-sizes only the wrapper. Fall back to nearest sized ancestor.
  // Meta-scoped: elsewhere ancestor fallback pinned button to whole card.
  function videoRect(video) {
    const r = video.getBoundingClientRect();
    if (r.width >= 120 && r.height >= 68) return r;
    if (!isMeta()) return r;
    let el = video.parentElement;
    for (let i = 0; i < 4 && el; i++) {
      const pr = el.getBoundingClientRect();
      if (pr.width >= 120 && pr.height >= 68) return pr;
      el = el.parentElement;
    }
    return r;
  }
  function showForVideo(video) {
    // Rescan at click time: manifests/page media declared after load.
    scanManifests();
    const pid = idFromUrl(permalinkFor(video));
    // Meta/YouTube/...: src/MSE is blob:, CDN urls carry no page id. On
    // single-post page, page-declared media from the site's CDN pair belongs
    // to that post.
    const scopedGlobal = /(^|\.)reddit\.com$/.test(location.hostname) ? [] : [...manifestUrls, ...pageMediaUrls()].filter((u) => pid && (() => { try { return boundedIdHit(u, pid) || (isVideoPageUrl(location.href) && (typeof hostMatchesScope === "function" ? hostMatchesScope(location.hostname.toLowerCase(), new URL(u).hostname.toLowerCase()) : /cdninstagram\.com|fbcdn\.net/i.test(u))); } catch (e) { return boundedIdHit(u, pid); } })());
    const direct = [...new Set([...directUrls(video), ...scopedGlobal])];
    fetchCandidates(((candidates) => {
      // Bg element scoping primary; scopedCandidates second-pass safety.
      // If client rules empty bg's src/id hits, trust bg (same tab+frame).
      const clientScoped = scopedCandidates(video, candidates);
      const scoped = clientScoped.length ? clientScoped : candidates;
      const byUrl = new Map();
      direct.forEach((u) => byUrl.set(u, { url: u, label: labelFor(video, u), filename: filenameFromUrl(u) }));
      scoped.forEach((c) => { if (c && c.url && !c.url.startsWith("blob:")) byUrl.set(c.url, { url: c.url, label: candidateLabel(c), filename: filenameFromUrl(c.url), type: c.type, mime: c.type }); });
      const streams = (typeof rankCandidates === "function" ? rankCandidates : (x) => x)([...byUrl.values()]);
      // IDM box for THIS video: page send first (formats in webcraft),
      // then this video's own streams only — never tab-global list.
      const pu = permalinkFor(video);
      const items = [];
      // Registry page first; else media-evidenced generic page (blob:/MSE
      // players on unlisted sites) — same contract as Chromium pickStream.
      const pi = pageItem(video) || genericPageItem(); if (pi) items.push(pi);
      streams.forEach((it) => { const isMan = /mpd|m3u8|dash|mpegurl|DASHPlaylist|HLSPlaylist/i.test(it.url || "") || /mpegurl|dash/i.test(it.type || ""); items.push({ ...it, intent: isMan ? "manifest" : "stream", pageUrl: pu || location.href, label: (isMan ? "Playlist (yt-dlp/native) · " : "") + (it.label || it.url) }); });
      showPanel(video, items);
    }), video);
  }
  // Button appearance presets (shared contract with Chromium twin).
  // Default matches the historic 27px round arrow button; the popup /
  // background owns the stored preset, this script only validates,
  // stores, and re-applies it to live buttons.
  let btnStyle = { icon: "arrow", w: 27, h: 27, radius: "50%" };
  const BTN_ICONS = ["arrow", "arrow-bold", "chevron", "plus", "dl"];
  function normalizeBtnStyle(style) {
    if (!style || typeof style !== "object") return null;
    const icon = BTN_ICONS.includes(style.icon) ? style.icon : "arrow";
    const dim = (v, d) => { const n = Number(v); return Number.isFinite(n) ? Math.min(96, Math.max(16, Math.round(n))) : d; };
    const w = dim(style.w, 27), h = dim(style.h, 27);
    let radius = "50%";
    if (typeof style.radius === "string" && style.radius.trim()) radius = style.radius.trim();
    else if (style.radius != null && style.radius !== "" && Number.isFinite(Number(style.radius))) radius = Number(style.radius) + "px";
    return { icon, w, h, radius };
  }
  function applyBtnStyle(btn) {
    if (!btn) return;
    btn.style.width = btnStyle.w + "px";
    btn.style.height = btnStyle.h + "px";
    btn.style.borderRadius = btnStyle.radius;
    if (btnStyle.icon === "dl") {
      const fs = Math.max(8, Math.round(btnStyle.h * 0.36));
      btn.innerHTML = '<span style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;font-size:' + fs + 'px;font-weight:700;pointer-events:none">DL</span>';
      return;
    }
    const px = Math.round(Math.min(btnStyle.w, btnStyle.h) * 0.48);
    const margin = Math.max(0, (btnStyle.h - px) / 2);
    let sw = "2.4", paths = '<path d="M12 3v11"/><path d="m7 10 5 5 5-5"/><path d="M4 20h16"/>';
    if (btnStyle.icon === "arrow-bold") sw = "3.6";
    else if (btnStyle.icon === "chevron") paths = '<path d="m5 8 7 7 7-7"/><path d="m5 13 7 7 7-7"/>';
    else if (btnStyle.icon === "plus") paths = '<path d="M12 5v14M5 12h14"/>';
    btn.innerHTML = '<svg width="' + px + '" height="' + px + '" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="' + sw + '" stroke-linecap="round" stroke-linejoin="round" style="display:block;margin:' + margin + 'px auto;pointer-events:none">' + paths + '</svg>';
  }
  function applyBtnStyleToAll() {
    try { btnForEntries.forEach(([, b]) => applyBtnStyle(b)); } catch (e) {}
    try { cardBtnEntries.forEach(([, b]) => applyBtnStyle(b)); } catch (e) {}
    try { if (pageBtn) applyBtnStyle(pageBtn); } catch (e) {}
  }
  function setBtnStyle(style) {
    const next = normalizeBtnStyle(style);
    if (!next) return;
    btnStyle = next;
    applyBtnStyleToAll();
  }
  const btnFor = new WeakMap();
  function fsEl() { return document.fullscreenElement || document.webkitFullscreenElement || null; }
  function inFs(video, fs) {
    let el = video;
    while (el) {
      if (fs.contains(el)) return true;
      const root = el.getRootNode && el.getRootNode();
      const next = root && root.host ? root.host : el.parentElement;
      if (!next || next === el) return false;
      el = next;
    }
    return false;
  }
  // Host: body normally (escapes player transform/overflow/stacking and
  // React/Lit re-renders, v0.22.2 regression); fullscreen subtree only
  // while fullscreen contains video. <video> as fullscreen element itself:
  // null (children of <video> never render; button hidden till exit).
  function hostFor(video) {
    const fs = fsEl();
    if (!fs) return document.body || document.documentElement;
    return fs !== video && inFs(video, fs) ? fs : null;
  }
  function syncBtnHost(video) {
    const btn = btnFor.get(video);
    if (!btn) return;
    const host = hostFor(video);
    if (!host) { btn.style.display = "none"; return; }
    if (btn.parentNode !== host) host.appendChild(btn);
    positionBtn(video);
  }
  function positionBtn(video) {
    const btn = btnFor.get(video);
    if (!btn) return;
    if (fsEl() && !hostFor(video)) { btn.style.display = "none"; return; }
    const r = videoRect(video);
    if (r.width < 120 || r.height < 68) { btn.style.display = "none"; return; }
    btn.style.display = "";
    if (btn.dataset.dragged) return;
    const fs = !!fsEl();
    btn.style.left = (fs ? Math.max(2, r.left - 10) : r.left - 10) + "px";
    btn.style.top = (fs ? Math.max(2, r.top - 10) : r.top - 10) + "px";
  }
  function attachBtn(video) {
    let btn = btnFor.get(video);
    // Host migration (body<->fullscreen) + re-anchor if page re-render
    // dropped button while video stayed connected.
    if (btn) { syncBtnHost(video); return btn; }
    btn = document.createElement("button");
    btn.title = "Download video";
    btn.setAttribute("aria-label", "Download video");
    btn.setAttribute("data-downloadcraft-btn", "1");
    btn.style.cssText = "position:fixed;z-index:2147483647;background:rgba(20,26,25,.72);color:rgba(255,255,255,.92);border:1px solid rgba(255,255,255,.28);box-shadow:0 1px 5px rgba(0,0,0,.35);padding:0;cursor:grab;text-align:center;opacity:0;pointer-events:none;display:none;touch-action:none;user-select:none;-webkit-user-select:none;transform:translateZ(0);font-family:\"Inter\",system-ui,sans-serif;";
    applyBtnStyle(btn);
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
    // Unified hover: fresh videoRect() coordinate test on every pointer
    // event (overlay-proof on all sites — overlay siblings never fire
    // containment events). Tracked pointerInside/focus/drag state gates
    // every hide path: the old playing-flash timer hid the button after
    // 2500ms even with the mouse parked over the video, and the non-Meta
    // mouseover-only path could never re-show it (no boundary crossing).
    // Non-Meta rest state is discreet but clickable like Meta's.
    let pointerInside = false, leaveTimer = null, focused = false;
    const showBtn = () => { positionBtn(video); btn.style.opacity = ".96"; btn.style.pointerEvents = "auto"; btn.style.background = "rgba(46,74,69,.94)"; btn.style.borderColor = "rgba(127,179,170,.9)"; };
    const restBtn = () => { btn.style.background = "rgba(20,26,25,.72)"; btn.style.borderColor = "rgba(255,255,255,.28)"; btn.style.opacity = ".42"; btn.style.pointerEvents = "auto"; };
    const hideBtn = restBtn;
    const scheduleHide = () => { if (leaveTimer) clearTimeout(leaveTimer); leaveTimer = setTimeout(() => { if (!pointerInside && !focused && !drag) hideBtn(); }, 350); };
    let hoverRaf = 0, lastEvt = null;
    const applyHover = () => {
      hoverRaf = 0;
      const e = lastEvt; if (!e) return;
      const r = videoRect(video);
      const inside = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
      pointerInside = inside || e.target === btn || document.activeElement === btn;
      if (pointerInside) { if (leaveTimer) { clearTimeout(leaveTimer); leaveTimer = null; } showBtn(); }
      else scheduleHide();
    };
    const onHover = (e) => { lastEvt = e; if (!hoverRaf) hoverRaf = requestAnimationFrame(applyHover); };
    document.addEventListener("mouseover", onHover, true);
    document.addEventListener("mousemove", onHover, true);
    document.addEventListener("mouseleave", () => { pointerInside = false; scheduleHide(); }, true);
    // Play-time element resizes (theatre mode, quality swap): keep anchor.
    if (typeof ResizeObserver !== "undefined") { try { new ResizeObserver(() => { if (btn.style.display !== "none") positionBtn(video); }).observe(video); } catch (e) {} }
    // Discoverability: brief reveal on playback start (pointer may sit still
    // over video, no mousemove fires). Flash-hide respects pointer state.
    let flashTimer = null;
    const flashBtn = () => { positionBtn(video); showBtn(); if (flashTimer) clearTimeout(flashTimer); flashTimer = setTimeout(() => { if (!pointerInside && !focused && !drag) hideBtn(); }, 2500); };
    video.addEventListener("playing", flashBtn);
    if (window.matchMedia && matchMedia("(hover: none)").matches) { flashBtn(); video.addEventListener("loadedmetadata", flashBtn); video.addEventListener("play", flashBtn); }
    btn.addEventListener("mouseenter", () => { pointerInside = true; if (leaveTimer) { clearTimeout(leaveTimer); leaveTimer = null; } showBtn(); });
    btn.addEventListener("mouseleave", () => { pointerInside = false; scheduleHide(); });
    btn.addEventListener("focus", () => { focused = true; showBtn(); });
    btn.addEventListener("blur", () => { focused = false; scheduleHide(); });

    btnFor.set(video, btn);
    btnForEntries.push([video, btn]);
    if (!video._wcSeen) video._wcSeen = Date.now();
    (document.body || document.documentElement).appendChild(btn);
    syncBtnHost(video); // moves into fullscreen subtree when already FS
    hideBtn();
    return btn;
  }
  function scanVideo(video) {
    if (wired.has(video)) { attachBtn(video); return; }
    wired.add(video);
    // UNIVERSAL ALWAYS-ON (IDM-style): button for every <video> no matter
    // the state. attachBtn runs immediately — paused preload=none Reddit
    // <video>s (no poster/data, src only a property) and property-src-only
    // videos never passed the old poster/readyState gate, so no button ever
    // anchored to them. positionBtn's >=120x68 gate still display:none's
    // undersized elements; seat + rest opacity .42 clickable, .96 hover.
    attachBtn(video);
    const show = () => { const btn = btnFor.get(video); if (btn) { positionBtn(video); if (btn.style.display !== "none") { btn.style.opacity = ".96"; btn.style.pointerEvents = "auto"; } } };
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
  // Feed-card buttons (IDM-style): cards carry only a thumbnail link and
  // no <video> until hover-preview, so video-keyed buttons never appear on
  // YouTube home / feeds. A card whose item link is a registry "page" URL
  // gets a definite button at rest opacity .42 (clickable) rising on hover,
  // sending intent="page" straight to the app — the watch URL is the
  // downloadable handle; no panel round-trip. Cards already owning a
  // <video> keep the per-video button.
  const cardBtnFor = new WeakMap();
  const cardBtnEntries = [];
  function cardItemUrl(scope) {
    try {
      const links = [...scope.querySelectorAll('a[href*="/watch?v="],a[href*="/shorts/"],a[href*="youtu.be/"]')];
      let best = "";
      for (const a of links) {
        const h = itemHref(a.getAttribute("href"));
        if (h && typeof videoPageKind === "function" && videoPageKind(h) === "page") { best = h; break; }
      }
      if (!best) return "";
      // A nested scope owning exactly this link is a deeper card; the outer
      // scope must not claim it (dense shelves share one big container).
      const link = links.find((a) => itemHref(a.getAttribute("href")) === best);
      const inner = link && link.closest ? link.closest("ytd-rich-item-renderer,ytd-video-renderer,ytd-compact-video-renderer,ytd-reel-item-renderer,article,.thing,[data-testid='tweet'],[data-testid='post-container']") : null;
      if (inner && inner !== scope && link && !link.closest("video")) {
        // Only defer when the inner card is a distinct element inside scope.
        if (scope !== inner && scope.contains(inner)) return "";
      }
      return best;
    } catch (e) { return ""; }
  }
  function positionCardBtn(btn, scope) {
    try {
      const r = scope.getBoundingClientRect();
      if (r.width < 120 || r.height < 68) { btn.style.display = "none"; return; }
      btn.style.display = "";
      btn.style.left = (r.left - 10) + "px";
      btn.style.top = (r.top - 10) + "px";
    } catch (e) {}
  }
  function attachCardBtn(scope, url) {
    let btn = cardBtnFor.get(scope);
    if (btn) { positionCardBtn(btn, scope); return btn; }
    btn = document.createElement("button");
    btn.title = "Download video";
    btn.setAttribute("aria-label", "Download video");
    btn.setAttribute("data-downloadcraft-btn", "1");
    btn.setAttribute("data-downloadcraft-card-btn", "1");
    btn.style.cssText = "position:fixed;z-index:2147483647;background:rgba(20,26,25,.72);color:rgba(255,255,255,.92);border:1px solid rgba(255,255,255,.28);box-shadow:0 1px 5px rgba(0,0,0,.35);padding:0;cursor:pointer;text-align:center;opacity:.42;pointer-events:auto;display:none;touch-action:none;user-select:none;-webkit-user-select:none;transform:translateZ(0);font-family:\"Inter\",system-ui,sans-serif;";
    applyBtnStyle(btn);
    btn.addEventListener("mouseenter", () => { btn.style.opacity = ".96"; btn.style.background = "rgba(46,74,69,.94)"; btn.style.borderColor = "rgba(127,179,170,.9)"; });
    btn.addEventListener("mouseleave", () => { btn.style.opacity = ".42"; btn.style.background = "rgba(20,26,25,.72)"; btn.style.borderColor = "rgba(255,255,255,.28)"; });
    btn.addEventListener("focus", () => { btn.style.opacity = ".96"; });
    btn.addEventListener("blur", () => { btn.style.opacity = ".42"; });
    btn.addEventListener("click", (e) => {
      e.preventDefault(); e.stopPropagation();
      const live = cardItemUrl(scope) || url;
      if (!live) return;
      api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: live, intent: "page", pageUrl: live, pageTitle: document.title, filename: filenameFromUrl(live), mime: "" });
    });
    cardBtnFor.set(scope, btn);
    cardBtnEntries.push([scope, btn]);
    (document.body || document.documentElement).appendChild(btn);
    positionCardBtn(btn, scope);
    return btn;
  }
  function scanCards() {
    try {
      const scopes = [...document.querySelectorAll(CARD_SEL)];
      const seen = new Set();
      for (const scope of scopes) {
        // Per-video button already owns this card.
        if (scope.querySelector("video")) continue;
        const url = cardItemUrl(scope);
        if (!url || seen.has(url)) continue;
        seen.add(url);
        attachCardBtn(scope, url);
      }
    } catch (e) {}
  }
  function pruneCardBtns() {
    for (let i = cardBtnEntries.length - 1; i >= 0; i--) {
      const [s, b] = cardBtnEntries[i];
      // Card recycled/removed, or a hover-preview <video> took over the
      // card: the per-video button owns the UI from then on.
      if (!s.isConnected || s.querySelector("video")) { b.remove(); cardBtnEntries.splice(i, 1); }
    }
  }
  function repositionCardBtns() { cardBtnEntries.forEach(([s, b]) => positionCardBtn(b, s)); }
  function scan() { allVideos().forEach(scanVideo); scanCards(); } // video only: audio via context menu // scanManifests only at click: full innerHTML regex per observer scan = biggest page cost
  // Debounced: full-page rescan on every DOM mutation is too expensive.
  let pending = null;
  const mo = new MutationObserver(() => { if (pending) return; pending = setTimeout(() => { pending = null; scan(); pruneCardBtns(); for (let i = btnForEntries.length - 1; i >= 0; i--) { const [v, b] = btnForEntries[i]; if (!v.isConnected) { b.remove(); btnForEntries.splice(i, 1); } } document.querySelectorAll("video, audio").forEach(positionBtn); repositionCardBtns(); }, 400); });
  window.addEventListener("resize", () => { document.querySelectorAll("video, audio").forEach(positionBtn); repositionCardBtns(); }, { passive: true });
  // Capture: IG reels scroll inner snap containers, no window scroll fires.
  window.addEventListener("scroll", () => { document.querySelectorAll("video, audio").forEach(positionBtn); repositionCardBtns(); }, { passive: true, capture: true });
  // Fullscreen: re-host buttons (body <-> fullscreen subtree), stale
  // pre-FS drag coords invalid after viewport resize.
  const onFsChange = () => {
    btnForEntries.forEach(([v, b]) => { delete b.dataset.dragged; syncBtnHost(v); });
    requestAnimationFrame(() => btnForEntries.forEach(([v]) => positionBtn(v)));
  };
  document.addEventListener("fullscreenchange", onFsChange);
  document.addEventListener("webkitfullscreenchange", onFsChange);
  mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["src", "poster"] });
  // Declarative-player pages (Globo <bs-player>): no <video> exists until
  // play, so no per-video button can attach pre-play. Offer the article
  // page itself to yt-dlp via a discreet corner button instead.
  let pageBtn = null;
  function ensurePageBtn() {
    try {
      if (pageBtn && pageBtn.isConnected) return;
      if (typeof videoPageKind === "function" ? videoPageKind(location.href) === "feed" : isFeedHome(location.href)) return;
      if (!hasPlayerEvidence() && !pageMediaUrls().length && !embedPageItems().length) return;
      if (allVideos().length) return; // real <video>: per-video button owns UI
      const item = pageItem(null) || embedPageItems()[0] || genericPageItem();
      if (!item) return;
      const b = document.createElement("button");
      b.title = "Download video with webcraft";
      b.setAttribute("aria-label", "Download video with webcraft");
      b.setAttribute("data-downloadcraft-page-btn", "1");
      b.style.cssText = "position:fixed;right:14px;bottom:14px;z-index:2147483647;background:rgba(46,74,69,.94);color:rgba(255,255,255,.95);border:1px solid rgba(127,179,170,.9);box-shadow:0 1px 6px rgba(0,0,0,.4);padding:0;cursor:pointer;opacity:.92;pointer-events:auto;";
      applyBtnStyle(b);
      b.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: item.url, intent: "page", pageUrl: item.pageUrl, pageTitle: document.title, filename: item.filename, mime: "" }); });
      document.body.appendChild(b);
      pageBtn = b;
    } catch (e) {}
  }
  ensurePageBtn();
  new MutationObserver(() => ensurePageBtn()).observe(document.documentElement, { childList: true, subtree: true });
  scan();
  // Stored button style: background/popup answers {style:{...}}; apply on
  // arrival (may land after first buttons already rendered).
  try {
    const maybeInit = api.runtime.sendMessage({ type: "downloadcraft-get-button-style" });
    if (maybeInit && maybeInit.then) maybeInit.then((r) => { if (r && r.style) setBtnStyle(r.style); }, () => {});
  } catch (e) {}
  api.runtime.onMessage.addListener((msg) => {
    if (msg && msg.type === "downloadcraft-button-style" && msg.style) { setBtnStyle(msg.style); return; }
    // Sniffed candidates never touch UI: panel opens only on user click.
    if (msg.type === "downloadcraft-confirm" && self === top) ensureConfirm(msg);
    if (msg.type === "downloadcraft-scan") {
      let anchor = null, best = 0;
      allVideos().forEach((v) => {
        const r = videoRect(v); // Meta: wrapper-sized fallback, 0x0 raw rect loses toolbar scan
        const a = r.width * r.height;
        if (r.width >= 120 && r.height >= 68 && a > best) { best = a; anchor = v; }
      });
      if (anchor) { attachBtn(anchor); showForVideo(anchor); }
      else {
        // No sizable video element (registry page not yet attached, blob/MSE
        // shell, or media-evidenced generic page): offer the page itself for
        // yt-dlp — toolbar parity with Chromium. Feeds/empty pages stay silent
        // here; Chromium shows a note-only panel there instead.
        // Embedded player pages: embed (nocookie iframe etc.) is the video,
        // article is just its host page — offer the embed first.
        const pi = pageItem(null) || embedPageItems()[0] || genericPageItem();
        if (pi) api.runtime.sendMessage({ type: "downloadcraft-video-panel", url: pi.url, intent: "page", pageUrl: pi.pageUrl, pageTitle: document.title, filename: pi.filename, mime: "" });
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
