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
    return /(\.ts|\.m4s|\.cmfv|\.cmfa)$/i.test(base)
      || /\/chunklist|\/seg-|sequence/i.test(base);
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
    return kind === "file" ? 300 : kind === "playlist" ? 200 : 50;
  }

  function rankCandidates(list) {
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
        return { item: r.item, index: r.index, size: r.size, dur: durationOf(r.item), score: scoreOf(kindOf(r.item), hasPlaylist) };
      })
      .sort(function (a, b) {
        return (b.score - a.score) || (b.size - a.size) || (b.dur - a.dur) || (a.index - b.index);
      })
      .map(function (r) { return r.item; });
  }

  g.rankCandidates = rankCandidates;
  g.DCRank = { rankCandidates: rankCandidates, kindOf: kindOf, canonicalUrl: canonicalUrl };
  if (typeof module !== "undefined" && module.exports) module.exports = g.DCRank;
})(typeof globalThis !== "undefined" ? globalThis : this);
