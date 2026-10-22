// rune: node extensions/rank.test.mjs
import assert from "node:assert";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { rankCandidates, matchRecordsForElement } = require("./chromium/rank.js");
const also = require("./firefox/rank.js");
assert.equal(typeof also.rankCandidates, "function"); // firefox copy loads too
assert.equal(typeof matchRecordsForElement, "function");
assert.equal(typeof also.matchRecordsForElement, "function");

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

// Background static invariants: IG gate (empty pool + trusted instagram
// host), noTiming retry/IG, rankCandidates(el), segment flood parity.
for (const bg of ["./chromium/background.js", "./firefox/background.js"]) {
  const s = readFileSync(new URL(bg, import.meta.url), "utf8");
  assert.match(s, /!pool\.length[\s\S]{0,400}instagram\\\.com/, bg + ": IG fallback gated on empty pool + instagram host");
  assert.match(s, /frameId: null, noTiming: true/, bg + ": frame-agnostic retry carries noTiming");
  assert.match(s, /noTiming: true, pageHost: "instagram\.com"/, bg + ": IG call carries noTiming");
  assert.match(s, /rankCandidates[^;]*msg\.element \? el : null/, bg + ": rankCandidates(el) closeness");
  // Segment flood: both reject HLS/DASH segment URLs pre-capture via same
  // filename regex; Firefox captureDecision also ext-rejects ts/m4s
  // (parity Chromium isSniff ext gate).
  assert.ok(s.includes("/^(seg|segment|chunk|frag)[-_]?\\d|\\.m4s$|[-_]\\d+\\.(ts|m4s)$/i"), bg + ": segment URL reject present (identical regex)");
  if (bg.includes("firefox")) assert.ok(s.includes('["ts", "m4s"].includes(extOfUrl(details.url))'), bg + ": segment ext reject parity");
}
console.log("rank.test.mjs OK", urls);
