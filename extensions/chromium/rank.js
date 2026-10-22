// Adapted from downloadcraft (MIT)
// Pure media-candidate ranking. No chrome.*/browser.* APIs, so Node tests
// can load this file. Loaded before background/content scripts (see
// manifest.json); background also importScripts() it where supported.
(function (g) {
  "use strict";

  function isPlaylistUrl(u, mime) {
    var base = String(u || "").split(/[?#]/)[0];
    return /(\.m3u8|\.mpd)$/i.test(base)
      || /m3u8|mpegurl|dash/i.test(String(mime || ""))
      || /m3u8/i.test(String(u || ""));
  }

  function isSegmentUrl(u) {
    var base = String(u || "").split(/[?#]/)[0];
    if (/(\.ts|\.m4s|\.cmfv|\.cmfa)$/i.test(base)
      || /\/chunklist|\/seg-|sequence/i.test(base)) return true;
    if (String(u || "").indexOf("$RepresentationID$") !== -1) return true;
    // .mp4/.cmfv/.cmfa only when basename carries seg/init token or trailing
    // index: plain movie.mp4/vid.mp4 stay files.
    var name = base.split("/").pop() || "";
    return /^(init|init\d*|initialization|header|segment|seg|chunk|frag)[-_]?\d*\./i.test(name)
      || /[-_]\d+\.(mp4|cmfv|cmfa)$/i.test(name)
      || /-init\./i.test(name) || /init-\d/i.test(name);
  }

  function isJunkUrl(u) {
    if (/^(blob|data):/i.test(String(u || ""))) return true;
    if (/(^|[/&?=._-])(license|widevine|drm)([/&?=._-]|$)/i.test(String(u || ""))) return true;
    // Page assets never downloads, whatever MIME claims: json/js/css/html etc.
    var base = String(u || "").split(/[?#]/)[0];
    return /\.(json|js|mjs|css|html?|xhtml|xml|txt|map)$/i.test(base);
  }

  // Slice selectors (bytestart/byteend) address same file: collapse them.
  function canonicalUrl(u) {
    try {
      var x = new URL(String(u || ""));
      x.searchParams.delete("bytestart");
      x.searchParams.delete("byteend");
      x.hash = "";
      return x.toString();
    } catch (e) {
      return String(u || "");
    }
  }

  // "playlist" | "segment" | "file". Our candidates carry MIME in `type`.
  function kindOf(entry) {
    var u = (entry && entry.url) || "";
    var mime = (entry && (entry.mime || entry.type)) || "";
    if (isPlaylistUrl(u, mime)) return "playlist";
    if (isSegmentUrl(u)) return "segment";
    return "file";
  }

  function num(v) {
    var n = typeof v === "string" ? parseInt(v, 10) : Number(v);
    return isFinite(n) && n > 0 ? n : 0;
  }
  function sizeOf(e) {
    return num(e.size) || num(e.contentLength) || num(e.content_length)
      || num(e._size) || num(e.fileSize) || num(e.bytes) || 0;
  }
  function durationOf(e) {
    return num(e.duration) || num(e.durationSecs) || num(e.durationSeconds)
      || num(e.seconds) || 0;
  }

  // Higher = offer first. Segment under known playlist = -1: parts engine
  // fetches itself, never offer parts when whole available.
  function scoreOf(kind, hasPlaylist) {
    if (kind === "segment" && hasPlaylist) return -1;
    if (kind === "playlist") return hasPlaylist ? 300 : 200;
    // file 300 bare / 200 under playlist; sort tiebreaks on size/dur/index.
    if (kind === "file") return hasPlaylist ? 200 : 300;
    return 50;
  }

  function rankCandidates(list, el) {
    // Optional el.since: closeness-to-firstPlay tie-break (after kind score,
    // before size) so own video's timing hits beat overlapping foreign ones.
    // No el -> dt all-equal, ordering unchanged.
    var since = el && el.since ? el.since : 0;
    function dtOf(e) { return since ? (e.time ? Math.abs(e.time - since) : Infinity) : 0; }
    var byCanon = new Map();
    (list || []).forEach(function (raw, idx) {
      var item = typeof raw === "string" ? { url: raw } : raw;
      if (!item || !item.url) return;
      if (isJunkUrl(item.url)) return;
      var key = canonicalUrl(item.url);
      var size = sizeOf(item);
      var hit = byCanon.get(key);
      if (!hit) byCanon.set(key, { item: item, size: size, index: idx });
      else if (size > hit.size) { hit.item = item; hit.size = size; }
    });
    var rows = Array.from(byCanon.values());
    var hasPlaylist = rows.some(function (r) { return kindOf(r.item) === "playlist"; });
    // Filter, don't demote: segments/init pieces under known playlist are parts
    // engine fetches itself. Kept bare when no playlist (only lead user has).
    if (hasPlaylist) rows = rows.filter(function (r) { return kindOf(r.item) !== "segment"; });
    return rows
      .map(function (r) {
        return { item: r.item, index: r.index, size: r.size, dur: durationOf(r.item), dt: dtOf(r.item), score: scoreOf(kindOf(r.item), hasPlaylist) };
      })
      .sort(function (a, b) {
        return (b.score - a.score) || (a.dt - b.dt) || (b.size - a.size) || (b.dur - a.dur) || (a.index - b.index);
      })
      .map(function (r) { return r.item; });
  }

  // ---- request<->element correlation (IDM model) ----
  // Records: {url, redirectFrom, hops:[origin..final], tabId, frameId, time,
  // type, size, status}. el: {tabId, frameId, srcs:[..], srcKey, postId:"",
  // streamId:"", since:ms, now:ms}. Returns matched records narrowed to {url,type,size,time}.
  // Tiers EXCLUSIVE: src/hops hit wins; else bounded postId hit; else bounded
  // streamId hit; else timing
  // (only when element has no real srcs, else timing leaks other videos).
  // Timing hits sorted closest-to-since first (out() preserves it; callers
  // ranking by closeness keep own video ahead of overlapping foreign one).
  // ponytail: timing still cannot disambiguate two concurrent no-id/no-src
  // videos; both returned, closest first (rank.test.mjs ceiling case).
  // Upgrade: performance-timeline resource naming if that proves too loose.
  // el.noTiming (background retry/IG only): retry widens frame, never tier,
  // so timing tier skipped; src/postId/streamId/hostExtra still apply.
  function boundedHit(u, pid) {
    if (!pid || !u) return false;
    var esc = String(pid).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    return new RegExp("[/=?&._-]" + esc + "([/?&#._-]|$)").test(String(u));
  }
  function matchRecordsForElement(records, el, now, extraPool) {
    var e = el || {};
    // Host-scoped fallback (background-only, NOT reachable from content
    // messages): IG/MSE pages can never src/id-match CDN URLs. Extra pool
    // entries without tab/frame recorded (e.g. MV3 SW / webNavigation SPA
    // ids) pass only when host-matched AND inside since window.
    var hostScope = String(e.pageHost || "").toLowerCase();
    var useExtra = !!(hostScope && e.since);
    var srcs = {};
    (e.srcs || []).forEach(function (s) { if (s && !/^(blob|data):/.test(s)) srcs[s] = 1; });
    if (e.srcKey && !/^(blob|data):/.test(e.srcKey)) srcs[e.srcKey] = 1;
    var srcKeys = Object.keys(srcs);
    var since = e.since || 0;
    var pool = [];
    (records || []).forEach(function (r) {
      if (!r || !r.url || /^(blob|data):/.test(r.url)) return;
      if (!(r.tabId >= 0)) return; // non-tab: not an element request
      if (e.tabId != null && r.tabId !== e.tabId) return;
      // Frame filter tolerant: reject only when both frameIds are numbers
      // and differ. Record with null/undefined frameId passes.
      if (typeof e.frameId === "number" && typeof r.frameId === "number" && r.frameId !== e.frameId) return;
      pool.push(r);
    });
    var extra = [];
    if (useExtra) {
      (extraPool || []).forEach(function (r) {
        if (!r || !r.url || /^(blob|data):/.test(r.url)) return;
        // Same-tab extras with different frame lose to tab-strict pool;
        // tabId==null entries (host-scoped) still pass.
        if (e.tabId != null && r.tabId != null && r.tabId !== e.tabId) return;
        try {
          var h = new URL(r.url).hostname.toLowerCase();
          // IG page (instagram.com) serves media from cdninstagram/fbcdn:
          // pageHost scope must accept CDN pair, not just subdomains.
          var igPair = (hostScope === "instagram.com" || hostScope.endsWith(".instagram.com"))
            && /(^|\.)(cdninstagram\.com|fbcdn\.net)$/.test(h);
          if (!igPair && h !== hostScope && !h.endsWith("." + hostScope)) return;
        } catch (err) { return; }
        if (!(r.time && r.time >= e.since - 2000 && r.time <= e.since + 45000)) return;
        extra.push(r);
      });
    }
    function hostExtra() { return extra.length ? out(extra) : []; }
    function out(list) { return list.map(function (r) { return { url: r.url, type: r.type || "", size: r.size || 0, time: r.time || 0 }; }); }
    function hopsOf(r) { return Array.isArray(r.hops) ? r.hops : []; }
    if (srcKeys.length) {
      var srcHits = pool.filter(function (r) {
        return !!(srcs[r.url] || (r.redirectFrom && srcs[r.redirectFrom]) || hopsOf(r).some(function (h) { return !!srcs[h]; }));
      });
      if (srcHits.length) return out(srcHits);
    }
    if (e.postId) {
      var postHits = pool.filter(function (r) {
        return boundedHit(r.url, e.postId) || boundedHit(r.redirectFrom, e.postId)
          || hopsOf(r).some(function (h) { return boundedHit(h, e.postId); });
      });
      if (postHits.length) return out(postHits);
    }
    if (e.streamId && e.streamId !== e.postId) {
      var streamHits = pool.filter(function (r) {
        return boundedHit(r.url, e.streamId) || boundedHit(r.redirectFrom, e.streamId)
          || hopsOf(r).some(function (h) { return boundedHit(h, e.streamId); });
      });
      if (streamHits.length) return out(streamHits);
    }
    // Timing anchored at firstPlay (since), never at click time (now):
    // late click must not widen window. No since -> no timing hits.
    if (!srcKeys.length && !e.noTiming && since) {
      var timed = out(pool
        .filter(function (r) { return !!(r.time && r.time >= since - 2000 && r.time <= since + 45000); })
        .sort(function (a, b) { return Math.abs(a.time - since) - Math.abs(b.time - since); }));
      if (timed.length) return timed;
    }
    // Host-wide fallback LAST resort (background-only, never reachable
    // via content messages): IG/MSE pages never src/id-match.
    return hostExtra();
  }

  g.rankCandidates = rankCandidates;
  g.matchRecordsForElement = matchRecordsForElement;
  g.DCRank = { rankCandidates: rankCandidates, kindOf: kindOf, canonicalUrl: canonicalUrl, matchRecordsForElement: matchRecordsForElement };
  if (typeof module !== "undefined" && module.exports) module.exports = g.DCRank;
})(typeof globalThis !== "undefined" ? globalThis : this);
