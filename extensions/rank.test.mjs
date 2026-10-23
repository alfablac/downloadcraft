// rune: node extensions/rank.test.mjs
import assert from "node:assert";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { rankCandidates, matchRecordsForElement, videoPageKind, videoIdOf, hostMatchesScope, siteOf } = require("./chromium/rank.js");
const also = require("./firefox/rank.js");
assert.equal(typeof also.rankCandidates, "function"); // firefox copy loads too
assert.equal(typeof matchRecordsForElement, "function");
assert.equal(typeof also.matchRecordsForElement, "function");
for (const fn of ["videoPageKind", "videoIdOf", "hostMatchesScope", "siteOf"]) {
  assert.equal(typeof { videoPageKind, videoIdOf, hostMatchesScope, siteOf }[fn], "function", fn + " exported (chromium)");
  assert.equal(typeof also[fn], "function", fn + " exported (firefox parity)");
}

const input = [
  { url: "https://x/seg1.ts" },
  { url: "https://x/movie.m3u8" },
  { url: "https://x/chunk.ts" },
  { url: "https://x/small.mp4", contentLength: 100 },
  { url: "https://x/big.mp4", contentLength: 9000 },
  { url: "https://x/big.mp4", contentLength: 9000 },
  { url: "https://x/v.mp4?bytestart=0&byteend=499" },
  { url: "https://x/v.mp4?bytestart=500&byteend=999", contentLength: 5000 },
  { url: "https://x/long.mp4", contentLength: 100, duration: 600 },
];
const out = rankCandidates(input);
const urls = out.map((e) => e.url);
assert.equal(urls.length, 5, "dedupe + byte-range collapse + segments filtered under playlist; got " + urls.join(","));
assert.equal(urls[0], "https://x/movie.m3u8", "playlist first when present");
assert.equal(urls[1], "https://x/big.mp4", "biggest file first among files");
assert.equal(rankCandidates([{ url: "https://x/movie.mp4", size: 100 }, { url: "https://x/big.mp4", size: 999999 }])[0].url, "https://x/big.mp4", "biggest file first without playlist");
assert.equal(urls[2], "https://x/v.mp4?bytestart=500&byteend=999", "largest byte-range variant kept");
assert.equal(urls[3], "https://x/long.mp4", "duration beats equal-size file");
assert.equal(urls[4], "https://x/small.mp4");
assert.ok(!urls.some((u) => u.endsWith(".ts")), "segments filtered when playlist known, got " + urls.join(","));
// playlist present: segments filtered out entirely
const bare = rankCandidates([{ url: "https://x/a.ts" }, { url: "https://x/a.m3u8" }]);
assert.deepEqual(bare.map((e) => e.url), ["https://x/a.m3u8"]);
// segments kept bare when no playlist exists
assert.ok(rankCandidates([{ url: "https://x/a.ts" }]).length === 1);
// junk schemes + DRM endpoints never offered
assert.deepEqual(rankCandidates([{ url: "blob:https://x/abc" }, { url: "data:video/mp4;base64,AA" }, { url: "https://x/widevine/license" }]), []);
// segment-ish .mp4 filtered when playlist present; plain movie.mp4 kept
assert.deepEqual(rankCandidates([{ url: "https://x/seg-1.mp4" }, { url: "https://x/a.m3u8" }]).map((e) => e.url), ["https://x/a.m3u8"], "seg-1.mp4 filtered under playlist");
assert.deepEqual(rankCandidates([{ url: "https://x/movie.mp4" }, { url: "https://x/a.m3u8" }]).map((e) => e.url), ["https://x/a.m3u8", "https://x/movie.mp4"], "movie.mp4 kept below playlist");
assert.ok(rankCandidates([{ url: "https://x/movie.mp4" }]).length === 1, "movie.mp4 kept bare");
assert.ok(rankCandidates([{ url: "https://x/video-2.cmfv" }, { url: "https://x/a.m3u8" }]).every((e) => !e.url.endsWith(".cmfv")), "indexed .cmfv filtered under playlist");
assert.ok(rankCandidates([{ url: "https://x/init-1.mp4" }, { url: "https://x/a.m3u8" }]).every((e) => !e.url.endsWith(".mp4")), "init-1.mp4 filtered under playlist");
assert.ok(rankCandidates([{ url: "https://x/$RepresentationID$-init.mp4" }, { url: "https://x/a.m3u8" }]).every((e) => !e.url.endsWith(".mp4")), "$RepresentationID$ filtered under playlist");
// both copies behave identically
assert.deepEqual(also.rankCandidates(input).map((e) => e.url), urls, "firefox parity");
// empty
assert.deepEqual(rankCandidates([]), []);
// matchRecordsForElement: same tab+frame only; src / redirectFrom /
// postId / timing fallback; blob srcs ignored; non-tab excluded.
const T = 1_700_000_000_000;
const recs = [
  { url: "https://cdn/a.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4", size: 10 },
  { url: "https://cdn/final.mp4", redirectFrom: "https://cdn/a.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" },
  { url: "https://cdn/other.mp4", tabId: 7, frameId: 9, time: T, type: "video/mp4" }, // other frame
  { url: "https://cdn/tab.mp4", tabId: 8, frameId: 3, time: T, type: "video/mp4" }, // other tab
  { url: "https://cdn/nontab.mp4", tabId: -1, frameId: -1, time: T, type: "video/mp4" }, // non-tab
  { url: "https://cdn/post_abc123_x.mp4", tabId: 7, frameId: 3, time: T - 99999, type: "video/mp4" }, // id hit, old
];
let m = matchRecordsForElement(recs, { tabId: 7, frameId: 3, srcs: ["https://cdn/a.mp4"], since: T, now: T }, T);
assert.deepEqual(m.map((r) => r.url), ["https://cdn/a.mp4", "https://cdn/final.mp4"]); // src + redirectFrom
m = matchRecordsForElement(recs, { tabId: 7, frameId: 3, srcs: [], postId: "abc123", since: 0, now: T }, T);
assert.deepEqual(m.map((r) => r.url), ["https://cdn/post_abc123_x.mp4"]); // bounded id, no timing (since=0)
m = matchRecordsForElement(recs, { tabId: 7, frameId: 3, srcs: ["blob:https://x/1"], since: T, now: T }, T);
assert.deepEqual(m.map((r) => r.url), ["https://cdn/a.mp4", "https://cdn/final.mp4"]); // blob srcs empty -> timing+frame
assert.deepEqual(matchRecordsForElement(recs, { tabId: 7, frameId: 3, srcs: ["https://nope/z.mp4"], since: T, now: T }, T), []); // srcs present: no timing leak
// F10(a) src hit EXCLUSIVE: foreign postId+timing records excluded.
const excl = [
  { url: "https://cdn/mine.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" },
  { url: "https://cdn/post_abc123_foreign.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" },
  { url: "https://cdn/timing.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" },
];
assert.deepEqual(
  matchRecordsForElement(excl, { tabId: 7, frameId: 3, srcs: ["https://cdn/mine.mp4"], postId: "abc123", since: T, now: T }, T).map((r) => r.url),
  ["https://cdn/mine.mp4"], "F10a src tier exclusive");
// F10(b) late click: window anchored at since (firstPlay), not now.
// now=since+10min must not rescue stream at since+60s.
const late = [{ url: "https://cdn/late.mp4", tabId: 7, frameId: 3, time: T + 60000, type: "video/mp4" }];
assert.deepEqual(matchRecordsForElement(late, { tabId: 7, frameId: 3, srcs: [], since: T, now: T + 10 * 60000 }, T + 10 * 60000), [], "F10b late click excludes since+60s");
assert.equal(matchRecordsForElement([{ url: "https://cdn/in.mp4", tabId: 7, frameId: 3, time: T + 44000, type: "video/mp4" }], { tabId: 7, frameId: 3, srcs: [], since: T, now: T + 10 * 60000 }, T).length, 1, "F10b inside since+45s still hits");
assert.deepEqual(matchRecordsForElement(late, { tabId: 7, frameId: 3, srcs: [], since: 0, now: T }, T), [], "F10b since=0 no timing");
// F10(c) redirect hops: hops [A,B,C], element srcs=[B] hits final record.
const hopsRecs = [{ url: "https://cdn/C.mp4", redirectFrom: "https://cdn/A.mp4", hops: ["https://cdn/A.mp4", "https://cdn/B.mp4", "https://cdn/C.mp4"], tabId: 7, frameId: 3, time: T, type: "video/mp4" }];
assert.deepEqual(matchRecordsForElement(hopsRecs, { tabId: 7, frameId: 3, srcs: ["https://cdn/B.mp4"], since: T, now: T }, T).map((r) => r.url), ["https://cdn/C.mp4"], "F10c hops intersection");
// F10(d) frameId undefined/null record passes tolerant frame filter.
const noFrame = [{ url: "https://cdn/nf.mp4", tabId: 7, time: T, type: "video/mp4" }, { url: "https://cdn/nf2.mp4", tabId: 7, frameId: null, time: T, type: "video/mp4" }];
assert.equal(matchRecordsForElement(noFrame, { tabId: 7, frameId: 3, srcs: [], since: T, now: T }, T).length, 2, "F10d undefined/null frameId passes");
assert.equal(also.matchRecordsForElement(noFrame, { tabId: 7, frameId: 3, srcs: [], since: T, now: T }, T).length, 2, "F10d firefox parity");
// F10(e) ceiling (documented limit): two concurrent no-id/no-src videos in
// same tab+frame both timing-match; matcher cannot disambiguate. Both
// included, closest-to-since first.
const ceil = [
  { url: "https://cdn/v1.mp4", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp4" },
  { url: "https://cdn/v2.mp4", tabId: 7, frameId: 3, time: T + 2000, type: "video/mp4" },
];
assert.deepEqual(matchRecordsForElement(ceil, { tabId: 7, frameId: 3, srcs: [], since: T, now: T }, T).map((r) => r.url), ["https://cdn/v1.mp4", "https://cdn/v2.mp4"], "F10e ceiling: both timing videos included");
// Two blob videos, same tab+frame, DISJOINT since windows: each element
// gets only own-window records (windows: [s-2s, s+45s]).
const twoBlob = [
  { url: "https://cdn/first.mp4", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp4" },
  { url: "https://cdn/second.mp4", tabId: 7, frameId: 3, time: T + 120000, type: "video/mp4" },
];
assert.deepEqual(matchRecordsForElement(twoBlob, { tabId: 7, frameId: 3, srcs: ["blob:https://x/1"], since: T, now: T }, T).map((r) => r.url), ["https://cdn/first.mp4"], "two blob videos: video1 own window only");
assert.deepEqual(matchRecordsForElement(twoBlob, { tabId: 7, frameId: 3, srcs: ["blob:https://x/2"], since: T + 119000, now: T + 119000 }, T + 119000).map((r) => r.url), ["https://cdn/second.mp4"], "two blob videos: video2 own window only");
assert.deepEqual(also.matchRecordsForElement(twoBlob, { tabId: 7, frameId: 3, srcs: ["blob:https://x/1"], since: T, now: T }, T).map((r) => r.url), ["https://cdn/first.mp4"], "two blob videos firefox parity");
// Overlapping windows: both records returned, closest to own since first,
// and rankCandidates(el) keeps closest ahead despite smaller size.
const overlap = [
  { url: "https://cdn/foreign-big.mp4", tabId: 7, frameId: 3, time: T + 40000, type: "video/mp4", size: 99999 },
  { url: "https://cdn/own-small.mp4", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp4", size: 10 },
];
const om = matchRecordsForElement(overlap, { tabId: 7, frameId: 3, srcs: [], since: T, now: T }, T);
assert.deepEqual(om.map((r) => r.url), ["https://cdn/own-small.mp4", "https://cdn/foreign-big.mp4"], "overlap: timing sorted closest-first");
assert.equal(rankCandidates(om, { since: T })[0].url, "https://cdn/own-small.mp4", "rankCandidates el.since: closest beats bigger foreign");
assert.equal(rankCandidates(om)[0].url, "https://cdn/foreign-big.mp4", "rankCandidates no el: size order unchanged");
assert.equal(also.rankCandidates(om, { since: T })[0].url, "https://cdn/own-small.mp4", "rankCandidates closeness firefox parity");
// noTiming (background frame-agnostic retry): retry widens frame, never
// tier. Element HAD srcs/postId but matched nothing in own frame ->
// cross-frame timing records must NOT attach; cross-frame postId hits may.
const retryRecs = [
  { url: "https://cdn/other-frame-timing.mp4", tabId: 7, frameId: 9, time: T + 1000, type: "video/mp4" },
  { url: "https://cdn/post_abc123_otherframe.mp4", tabId: 7, frameId: 9, time: T - 99999, type: "video/mp4" },
];
assert.deepEqual(matchRecordsForElement(retryRecs, { tabId: 7, frameId: null, noTiming: true, srcs: ["https://cdn/gone.mp4"], postId: "abc123", since: T, now: T }, T).map((r) => r.url), ["https://cdn/post_abc123_otherframe.mp4"], "noTiming retry: cross-frame postId hits, timing does not leak");
assert.deepEqual(matchRecordsForElement(retryRecs, { tabId: 7, frameId: null, noTiming: true, srcs: [], since: T, now: T }, T), [], "noTiming: pure timing element gets nothing cross-frame");
assert.deepEqual(matchRecordsForElement(retryRecs, { tabId: 7, frameId: 3, srcs: [], since: T, now: T }, T), [], "own frame: other-frame records excluded");
// bounded postId: hit does not pull ALL same-tab records (bounded tier only,
// foreign timing-window record excluded).
const postAll = [
  { url: "https://cdn/post_abc123_mine.mp4", tabId: 7, frameId: 3, time: T - 99999, type: "video/mp4" },
  { url: "https://cdn/foreign.mp4", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp4" },
  { url: "https://cdn/abc1234.mp4", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp4" }, // unbounded prefix
];
assert.deepEqual(matchRecordsForElement(postAll, { tabId: 7, frameId: 3, srcs: [], postId: "abc123", since: T, now: T }, T).map((r) => r.url), ["https://cdn/post_abc123_mine.mp4"], "postId bounded hit does not match ALL");

// streamId tier: reddit comments id != v.redd.it stream id; DASH url carries stream id only.
const streamRecs = [
  { url: "https://v.redd.it/cz2wlzfaeiwh1/DASHPlaylist.mpd", tabId: 7, frameId: 3, time: T, type: "application/dash+xml" },
  { url: "https://v.redd.it/otherstream9/DASHPlaylist.mpd", tabId: 7, frameId: 3, time: T, type: "application/dash+xml" },
];
assert.deepEqual(matchRecordsForElement(streamRecs, { tabId: 7, frameId: 3, srcs: [], postId: "", streamId: "cz2wlzfaeiwh1", since: 0, now: T }, T).map((r) => r.url), ["https://v.redd.it/cz2wlzfaeiwh1/DASHPlaylist.mpd"], "streamId tier hits v.redd.it playlist");
assert.deepEqual(also.matchRecordsForElement(streamRecs, { tabId: 7, frameId: 3, srcs: [], postId: "", streamId: "cz2wlzfaeiwh1", since: 0, now: T }, T).map((r) => r.url), ["https://v.redd.it/cz2wlzfaeiwh1/DASHPlaylist.mpd"], "streamId firefox parity");
// postId tier precedence over streamId (exclusive tiers, postId before streamId)
assert.deepEqual(matchRecordsForElement([...streamRecs, { url: "https://cdn/post_abc123_x.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" }], { tabId: 7, frameId: 3, srcs: [], postId: "abc123", streamId: "cz2wlzfaeiwh1", since: 0, now: T }, T).map((r) => r.url), ["https://cdn/post_abc123_x.mp4"], "postId tier before streamId");

// IG host-scoped fallback (background-only extraPool): CDN URLs carry no
// shortcode, MSE src is blob:, so src/postId/timing tiers all miss.
const igRecs = [
  { url: "https://scontent.cdninstagram.com/v/t50.2886-16/abc.mp4?efg=xyz", tabId: 7, frameId: 3, time: T + 5000, type: "video/mp4" },
  { url: "https://scontent.xx.fbcdn.net/v/t39.30808-6/xyz.mp4", tabId: 7, frameId: 3, time: T + 6000, type: "video/mp4" },
  { url: "https://scontent.cdninstagram.com/v/t50.2886-16/old.mp4", tabId: 7, frameId: 3, time: T - 99999, type: "video/mp4" }, // outside since window
  { url: "https://other.cdn.example/v.mp4", tabId: 7, frameId: 3, time: T + 5000, type: "video/mp4" }, // wrong host
];
const igEl = { tabId: 7, frameId: 3, srcs: ["blob:https://www.instagram.com/1"], postId: "DAbc123XyZ", since: T, now: T, pageHost: "instagram.com" };
// main pool empty (records evicted/killed), extraPool = candidates: host+window hit
assert.deepEqual(
  matchRecordsForElement([], igEl, T, igRecs).map((r) => r.url),
  ["https://scontent.cdninstagram.com/v/t50.2886-16/abc.mp4?efg=xyz", "https://scontent.xx.fbcdn.net/v/t39.30808-6/xyz.mp4"],
  "IG host fallback: cdninstagram+fbcdn in-window, old + wrong-host excluded");
// fbcdn passes when scope is fbcdn.net
assert.deepEqual(
  matchRecordsForElement([], { ...igEl, pageHost: "fbcdn.net" }, T, igRecs).map((r) => r.url),
  ["https://scontent.xx.fbcdn.net/v/t39.30808-6/xyz.mp4"],
  "IG host fallback: fbcdn.net scope");
// no pageHost -> no fallback (content-reachable calls never widen)
assert.deepEqual(matchRecordsForElement([], { tabId: 7, frameId: 3, srcs: [], postId: "DAbc123XyZ", since: T, now: T }, T, igRecs), [], "no pageHost: extraPool ignored");
// no since -> no fallback
assert.deepEqual(matchRecordsForElement([], { ...igEl, since: 0 }, T, igRecs), [], "since=0: no host fallback");
// src tier still wins over host fallback
const igSrc = [{ url: "https://scontent.cdninstagram.com/v/mine.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" }];
assert.deepEqual(
  matchRecordsForElement(igSrc, { tabId: 7, frameId: 3, srcs: ["https://scontent.cdninstagram.com/v/mine.mp4"], postId: "DAbc123XyZ", since: T, now: T, pageHost: "instagram.com" }, T, igRecs).map((r) => r.url),
  ["https://scontent.cdninstagram.com/v/mine.mp4"], "src tier exclusive over host fallback");
// firefox parity
assert.deepEqual(
  also.matchRecordsForElement([], igEl, T, igRecs).map((r) => r.url),
  ["https://scontent.cdninstagram.com/v/t50.2886-16/abc.mp4?efg=xyz", "https://scontent.xx.fbcdn.net/v/t39.30808-6/xyz.mp4"], "IG host fallback firefox parity");

// ---- site registry: videoPageKind / videoIdOf / hostMatchesScope / siteOf ----
// `page` = single video item (offer page URL to yt-dlp), `feed` = grid/home
// (never a single item). Chromium + Firefox copies must agree on every case.
const kindCases = [
  ["youtube watch=page", "https://www.youtube.com/watch?v=dQw4w9WgXcQ", "page"],
  ["youtube shorts=page", "https://www.youtube.com/shorts/dQw4w9WgXcQ", "page"],
  ["youtube feed=feed", "https://www.youtube.com/feed/subscriptions", "feed"],
  ["instagram reel=page", "https://www.instagram.com/reel/DAbc123XyZ/", "page"],
  ["instagram explore=feed", "https://www.instagram.com/explore/", "feed"],
  ["facebook /share/v/=page", "https://www.facebook.com/share/v/AbC123XyZ/", "page"],
  ["threads /@u/post/ID=page", "https://www.threads.net/@someuser/post/DAbc123XyZ", "page"],
  ["tiktok /@u/photo/<digits>=page", "https://www.tiktok.com/@someuser/photo/1234567890123456789", "page"],
  ["twitch /{ch}/clip/{slug}=page", "https://www.twitch.tv/somechannel/clip/SomeSlug-ABC_123", "page"],
  ["clips.twitch.tv/<slug>=page", "https://clips.twitch.tv/SomeSlugABC123", "page"],
  ["vimeo /channels/x/123456=page", "https://vimeo.com/channels/staffpicks/123456789", "page"],
  ["dailymotion /embed/video/x=page", "https://www.dailymotion.com/embed/video/x8abc12", "page"],
  // pathname is lowercased inside videoPageKind, so BV id matches lowercase-compatible pattern
  ["bilibili /video/BV...=page", "https://www.bilibili.com/video/BV1xx411c7mu", "page"],
  ["bilibili /bangumi/play/ep123=page", "https://www.bilibili.com/bangumi/play/ep123", "page"],
  ["nicovideo /watch/sm123=page", "https://www.nicovideo.jp/watch/sm123", "page"],
  ["rumble /vXXXX=page", "https://rumble.com/v1a2b3c", "page"],
  // PeerTube: /w/<16+ chars> on any host
  ["peertube /w/<16+ chars>=page (arbitrary host)", "https://videos.example.org/w/abcdefghijklmnop", "page"],
];
for (const [name, u, want] of kindCases) {
  assert.equal(videoPageKind(u), want, "videoPageKind " + name);
  assert.equal(also.videoPageKind(u), want, "videoPageKind firefox parity " + name);
}
const idCases = [
  ["youtube watch?v=", "https://www.youtube.com/watch?v=dQw4w9WgXcQ", "dQw4w9WgXcQ"],
  ["instagram reel code", "https://www.instagram.com/reel/DAbc123XyZ/", "DAbc123XyZ"],
  ["twitch clip slug", "https://www.twitch.tv/somechannel/clip/SomeSlug-ABC_123", "SomeSlug-ABC_123"],
  ["tiktok photo id", "https://www.tiktok.com/@someuser/photo/1234567890123456789", "1234567890123456789"],
  ["vimeo bare id", "https://vimeo.com/123456789", "123456789"],
  ["vimeo channels id", "https://vimeo.com/channels/staffpicks/123456789", "123456789"],
  ["vimeo groups videos id", "https://vimeo.com/groups/filmmakers/videos/123456789", "123456789"],
  ["facebook share/v id", "https://www.facebook.com/share/v/SandboxShare1/", "SandboxShare1"],
];
for (const [name, u, want] of idCases) {
  assert.equal(videoIdOf(u), want, "videoIdOf " + name);
  assert.equal(also.videoIdOf(u), want, "videoIdOf firefox parity " + name);
}
const scopeCases = [
  ["instagram.com->scontent.cdninstagram.com", "instagram.com", "scontent.cdninstagram.com", true],
  ["facebook.com->fbcdn", "facebook.com", "scontent.xx.fbcdn.net", true],
  ["threads.net->fbcdn", "threads.net", "scontent.xx.fbcdn.net", true],
  ["youtube.com->googlevideo", "youtube.com", "rr1---sn-abc.googlevideo.com", true],
  ["x.com->twimg", "x.com", "video.twimg.com", true],
  ["reddit.com->v.redd.it", "reddit.com", "v.redd.it", true],
  ["cross-site: instagram page vs googlevideo", "instagram.com", "rr1---sn-abc.googlevideo.com", false],
];
for (const [name, ph, uh, want] of scopeCases) {
  assert.equal(hostMatchesScope(ph, uh), want, "hostMatchesScope " + name);
  assert.equal(also.hostMatchesScope(ph, uh), want, "hostMatchesScope firefox parity " + name);
}
const siteCases = [
  ["threads.net->threads", "https://www.threads.net/@someuser/post/DAbc123XyZ", "threads"],
  ["m.youtube.com->youtube", "https://m.youtube.com/watch?v=dQw4w9WgXcQ", "youtube"],
  ["fb.watch->facebook", "https://fb.watch/AbC123XyZ/", "facebook"],
];
for (const [name, u, want] of siteCases) {
  assert.equal(siteOf(u), want, "siteOf " + name);
  assert.equal(also.siteOf(u), want, "siteOf firefox parity " + name);
}

// Background static invariants: host-scoped fallback gate (empty pool +
// trusted sender-tab hostname via pageHost: ph), noTiming retry/host,
// rankCandidates(el), segment flood parity. Generalized from the old
// Instagram-only gate (hardcoded pageHost: "instagram.com").
for (const bg of ["./chromium/background.js", "./firefox/background.js"]) {
  const s = readFileSync(new URL(bg, import.meta.url), "utf8");
  assert.match(s, /!pool\.length[\s\S]{0,400}pageHost: ph/, bg + ": host fallback gated on empty pool + pageHost: ph");
  assert.match(s, /new URL\(\(sender\.tab/, bg + ": page host from sender tab URL");
  assert.match(s, /\.hostname\.toLowerCase\(\)/, bg + ": sender tab hostname extracted");
  assert.match(s, /pageHost: ph/, bg + ": host fallback passes sender tab hostname");
  assert.match(s, /frameId: null, noTiming: true/, bg + ": frame-agnostic retry carries noTiming");
  assert.match(s, /noTiming: true, pageHost: ph/, bg + ": host call carries noTiming + pageHost: ph");
  assert.match(s, /rankCandidates[^;]*msg\.element \? el : null/, bg + ": rankCandidates(el) closeness");
  // Segment flood: both reject HLS/DASH segment URLs pre-capture via same
  // filename regex; Firefox captureDecision also ext-rejects ts/m4s
  // (parity Chromium isSniff ext gate).
  assert.ok(s.includes("/^(seg|segment|chunk|frag)[-_]?\\d|\\.m4s$|[-_]\\d+\\.(ts|m4s)$/i"), bg + ": segment URL reject present (identical regex)");
  if (bg.includes("firefox")) assert.ok(s.includes('["ts", "m4s"].includes(extOfUrl(details.url))'), bg + ": segment ext reject parity");
}
console.log("rank.test.mjs OK", urls);
