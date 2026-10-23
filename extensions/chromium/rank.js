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
          // Page host serves media from its CDN pair (instagram->cdninstagram/
          // fbcdn, youtube->googlevideo, x->twimg, reddit->v.redd.it, ...).
          if (!hostMatchesScope(hostScope, h)) return;
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

  // ---- yt-dlp site registry (single source; Node-tested) ----
  // Mirrors src/ytdlp.rs is_video_page + research overrides. `page` = URL
  // is a single video item: offer it to yt-dlp (intent=page). `feed` = never
  // a single item (home/explore/channel/grid): only per-video anchors count.
  // `cdns` = media hosts for host-scoped element correlation (blob:/MSE
  // players whose CDN urls carry no page id). Sites not listed fall back to
  // generic: page ok when it carries a <video>, item link, or media meta.
  function hostIs(h, list) { return list.some(function (d) { return h === d || h.endsWith("." + d); }); }
  function videoPageKind(u) {
    var x; try { x = new URL(String(u || "")); } catch (e) { return "other"; }
    if (!/^https?:$/.test(x.protocol)) return "other";
    var h = x.hostname.toLowerCase().replace(/^(www\.|m\.|music\.)/, "");
    var p = x.pathname.toLowerCase();
    var ytId = (p === "/watch" && (x.searchParams.get("v") || "").length >= 6)
      || /^\/(shorts|live|embed|v)\/[A-Za-z0-9_-]{6,}/.test(p) || /^\/clip\/[A-Za-z0-9_-]{6,}/.test(p);
    if (h === "youtu.be" || h === "youtube-nocookie.com") return p.length > 1 ? "page" : "other";
    if (h === "youtube.com") return ytId ? "page" : (/^\/(feed|results|playlist|channel|@|c\/|user\/)/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["instagram.com"])) return /^\/(reel|reels|p|tv)\/[A-Za-z0-9_-]{5,}|\/stories\/[^/]+\/\d+/.test(p) ? "page" : (/^\/(explore|reels|direct|accounts)/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["facebook.com"]) || h === "fb.watch") return (h === "fb.watch" && p.length > 1) || /^\/watch\/?(\?|$)/.test(p + x.search) && (x.searchParams.get("v") || "").length >= 1 || /^\/(reel|share\/[rv]|[^/]+\/videos?)\//.test(p) || /\/videos?\/\d+/.test(p) ? "page" : (/^\/(home|feed|watch$|reel$|marketplace|gaming|groups|events)/.test(p.replace(/\/$/, "")) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["threads.net", "threads.com"])) return /^\/@[^/]+\/post\/[A-Za-z0-9_-]{5,}/.test(p) ? "page" : (p === "/" ? "feed" : "other");
    if (hostIs(h, ["tiktok.com"]) || h === "vm.tiktok.com" || h === "vt.tiktok.com") return /\/@(.[^/]*)\/(video|photo)\/\d{5,}|\/(video|photo)\/\d{5,}|\/t\/[A-Za-z0-9]+/.test(p) || /^(vm|vt)\.tiktok\.com$/.test(h) && p.length > 1 ? "page" : (/^\/(foryou|following|explore|discover|live|tag|music)/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["twitter.com", "x.com"])) return /\/status\/\d+/.test(p) ? "page" : (/^\/(home|explore|notifications|messages|search|i\/(flow|bookmarks))/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["reddit.com"]) || h === "redd.it" || h === "v.redd.it") return /\/comments\/[a-z0-9]+/.test(p) || h !== "reddit.com" && p.length > 1 ? "page" : (p === "/" || /^\/(r\/[^/]+\/?|hot|new|top|best)/.test(p) ? "feed" : "other");
    if (hostIs(h, ["vimeo.com"])) return /^\/\d{5,}([/?]|$)|\/(channels|groups)\/[^/]+\/(videos\/)?\d{5,}|\/video\/\d{5,}/.test(p) ? "page" : (["/", "/watch", "/feed", "/categories", "/channels", "/search"].includes(p.replace(/\/$/, "") || "/") ? "feed" : "other");
    if (hostIs(h, ["twitch.tv"]) || h === "clips.twitch.tv") return /^\/videos\/\d+/.test(p) || /^\/[^/]+\/clip\/[A-Za-z0-9_-]+/.test(p) || h === "clips.twitch.tv" && p.length > 1 || /^\/[^/]+\/?$/.test(p) && !/^\/(directory|following|search|settings|subscriptions)/.test(p) ? "page" : (/^\/(directory|following|search|settings|subscriptions)/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["dailymotion.com"]) || h === "dai.ly" || h === "geo.dailymotion.com") return /\/(video|embed\/video)\/[a-z0-9]+/.test(p) || h === "dai.ly" && p.length > 1 ? "page" : (p === "/" ? "feed" : "other");
    if (hostIs(h, ["bilibili.com"]) || h === "b23.tv") return /\/video\/(bv[0-9a-z]{8,}|av\d+)|\/bangumi\/play\/(ep|ss)\d+/.test(p) || h === "b23.tv" && p.length > 1 ? "page" : (/^\/(v\/|dynamic|read|account)/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["nicovideo.jp"])) return /\/watch\/(sm|nm|so|lv)?\d+/.test(p) ? "page" : "other";
    if (hostIs(h, ["soundcloud.com"]) || h === "on.soundcloud.com") return p.split("/").filter(Boolean).length >= 2 && !/^\/(discover|stream|you|search|upload|charts)/.test(p) ? "page" : (/^\/(discover|stream|you|search|upload|charts)/.test(p) || p === "/" ? "feed" : "other");
    if (hostIs(h, ["rumble.com"])) return /^\/(embed\/)?v[0-9a-z]{4,}/.test(p) ? "page" : (p === "/" ? "feed" : "other");
    if (hostIs(h, ["odysee.com", "lbry.tv"])) return /\/[^:]*:[0-9a-f]{6,}/.test(p) ? "page" : (p === "/" ? "feed" : "other");
    if (hostIs(h, ["pornhub.com"])) return /view_video\.php/.test(p) ? "page" : "other";
    if (hostIs(h, ["xvideos.com"])) return /\/video/.test(p) ? "page" : "other";
    if (/^\/(w|videos\/(watch|embed))\/[A-Za-z0-9-]{16,}/.test(p)) return "page"; // PeerTube, any host
    // Globo: g1/video pages (.ghtml articles carry embedded video; Rust parity).
    if (/globo\.com$/.test(h) && /\.ghtml$/.test(p)) return "page";
    return "other";
  }
  function isVideoPageUrl2(u) { return videoPageKind(u) === "page"; }
  function videoIdOf(u) {
    var s = String(u || "");
    var m = s.match(/\/status\/(\d+)|\/comments\/([a-z0-9]{5,})|[?&]v=([A-Za-z0-9_-]{6,})|youtu\.be\/([A-Za-z0-9_-]{6,})|\/shorts\/([A-Za-z0-9_-]{6,})|\/(?:reel|reels|p|tv)\/([A-Za-z0-9_-]{5,})|\/stories\/[^/]+\/(\d+)|\/post\/([A-Za-z0-9_-]{5,})|\/video\/(BV[0-9A-Za-z]{8,}|\d{5,}|[a-z0-9]{3,})|\/videos\/(\d{5,})|\/videos?\/(\d{5,})|\/clip\/([A-Za-z0-9_-]{5,})|clips\.twitch\.tv\/([A-Za-z0-9_-]{5,})|v\.redd\.it\/([a-z0-9]{5,})|dai\.ly\/([a-z0-9]{3,})|\/watch\/((?:sm|nm|so|lv)?\d{5,})/i);
    if (m) { for (var i = 1; i < m.length; i++) if (m[i]) return m[i]; }
    // Host-scoped bare-id shapes: vimeo.com/123, fb.watch/CODE, vm.tiktok.
    try {
      var x = new URL(s); var h = x.hostname.toLowerCase();
      if (hostIs(h.replace(/^(www\.|m\.)/, ""), ["vimeo.com"])) { var vm = x.pathname.match(/^\/(\d{5,})/) || x.pathname.match(/\/(?:channels|groups)\/[^/]+\/(?:videos\/)?(\d{5,})/); if (vm) return vm[1]; }
      if (hostIs(h.replace(/^(www\.|m\.)/, ""), ["facebook.com"])) { var fs = x.pathname.match(/\/share\/[rv]\/([A-Za-z0-9_-]{5,})/); if (fs) return fs[1]; }
      if (h === "fb.watch" || h === "vm.tiktok.com" || h === "vt.tiktok.com" || h === "b23.tv" || h === "redd.it") { var seg = x.pathname.split("/").filter(Boolean)[0]; if (seg) return seg; }
      if (/tiktok\.com$/i.test(h)) { var tk = x.pathname.match(/\/(?:video|photo)\/(\d{5,})/); if (tk) return tk[1]; }
    } catch (e) {}
    return "";
  }
  // Host-scoped correlation pairs: page host -> accepted media hosts.
  // cdninstagram/fbcdn serve instagram, facebook AND threads; googlevideo
  // serves youtube; twimg serves x; v.redd.it serves reddit.
  function hostMatchesScope(pageHost, urlHost) {
    var ph = String(pageHost || "").toLowerCase().replace(/^(www\.|m\.)/, "");
    var uh = String(urlHost || "").toLowerCase();
    if (!ph || !uh) return false;
    if (uh === ph || uh.endsWith("." + ph)) return true;
    var pairs = [
      [["instagram.com", "facebook.com", "threads.net", "threads.com"], ["cdninstagram.com", "fbcdn.net"]],
      [["youtube.com", "youtu.be", "youtube-nocookie.com"], ["googlevideo.com", "youtube.com", "youtu.be"]],
      [["twitter.com", "x.com"], ["twimg.com", "twitter.com", "x.com"]],
      [["reddit.com", "redd.it"], ["v.redd.it", "redd.it", "redditmedia.com"]],
      [["tiktok.com"], ["tiktokcdn.com", "tiktokcdn-us.com", "ibytedtos.com", "byteoversea.com", "tiktok.com"]],
      [["vimeo.com"], ["vimeocdn.com", "vimeo.com"]],
      [["twitch.tv", "clips.twitch.tv"], ["twitch.tv", "cloudfront.net", "twitchcdn.net"]],
      [["dailymotion.com", "dai.ly"], ["dmcdn.net", "dailymotion.com"]],
      [["bilibili.com"], ["bilivideo.com", "bilivideo.cn", "hdslb.com", "bilibili.com"]],
    ];
    for (var i = 0; i < pairs.length; i++) {
      if (hostIs(ph, pairs[i][0]) && hostIs(uh, pairs[i][1])) return true;
    }
    return false;
  }
  function siteOf(u) {
    var h = ""; try { h = new URL(String(u || "")).hostname.toLowerCase().replace(/^(www\.|m\.|music\.)/, ""); } catch (e) { return ""; }
    if (h === "youtu.be" || hostIs(h, ["youtube.com", "youtube-nocookie.com"])) return "youtube";
    if (hostIs(h, ["instagram.com"])) return "instagram";
    if (hostIs(h, ["facebook.com"]) || h === "fb.watch") return "facebook";
    if (hostIs(h, ["threads.net", "threads.com"])) return "threads";
    if (hostIs(h, ["tiktok.com"]) || h === "vm.tiktok.com" || h === "vt.tiktok.com") return "tiktok";
    if (hostIs(h, ["twitter.com", "x.com"])) return "x";
    if (hostIs(h, ["reddit.com", "redd.it", "v.redd.it"])) return "reddit";
    if (hostIs(h, ["vimeo.com"])) return "vimeo";
    if (hostIs(h, ["twitch.tv", "clips.twitch.tv"])) return "twitch";
    if (hostIs(h, ["dailymotion.com"]) || h === "dai.ly") return "dailymotion";
    if (hostIs(h, ["bilibili.com"]) || h === "b23.tv") return "bilibili";
    return "";
  }

  g.rankCandidates = rankCandidates;
  g.matchRecordsForElement = matchRecordsForElement;
  g.videoPageKind = videoPageKind;
  g.videoIdOf = videoIdOf;
  g.hostMatchesScope = hostMatchesScope;
  g.siteOf = siteOf;
  g.DCRank = { rankCandidates: rankCandidates, kindOf: kindOf, canonicalUrl: canonicalUrl, matchRecordsForElement: matchRecordsForElement, videoPageKind: videoPageKind, videoIdOf: videoIdOf, hostMatchesScope: hostMatchesScope, siteOf: siteOf };
  if (typeof module !== "undefined" && module.exports) module.exports = g.DCRank;
})(typeof globalThis !== "undefined" ? globalThis : this);
