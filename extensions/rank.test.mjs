// rune: node extensions/rank.test.mjs
import assert from "node:assert";
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
// same tab+frame both timing-match; matcher cannot disambiguate. Both included.
const ceil = [
  { url: "https://cdn/v1.mp4", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp4" },
  { url: "https://cdn/v2.mp4", tabId: 7, frameId: 3, time: T + 2000, type: "video/mp4" },
];
assert.deepEqual(matchRecordsForElement(ceil, { tabId: 7, frameId: 3, srcs: [], since: T, now: T }, T).map((r) => r.url), ["https://cdn/v1.mp4", "https://cdn/v2.mp4"], "F10e ceiling: both timing videos included");

// Generic opaque-id tier: any delimiter-bounded token matches stream URLs on
// any host (no per-site id extraction). Comments id != stream id case:
// stream URL carries its own id only.
const streamRecs = [
  { url: "https://cdn.example/zx9stream42/DASHPlaylist.mpd", tabId: 7, frameId: 3, time: T, type: "application/dash+xml" },
  { url: "https://cdn.example/otherstream9/DASHPlaylist.mpd", tabId: 7, frameId: 3, time: T, type: "application/dash+xml" },
];
assert.deepEqual(matchRecordsForElement(streamRecs, { tabId: 7, frameId: 3, srcs: [], postId: "", streamId: "zx9stream42", since: 0, now: T }, T).map((r) => r.url), ["https://cdn.example/zx9stream42/DASHPlaylist.mpd"], "streamId tier hits opaque stream id");
assert.deepEqual(also.matchRecordsForElement(streamRecs, { tabId: 7, frameId: 3, srcs: [], postId: "", streamId: "zx9stream42", since: 0, now: T }, T).map((r) => r.url), ["https://cdn.example/zx9stream42/DASHPlaylist.mpd"], "streamId firefox parity");
// postId tier precedence over streamId (exclusive tiers, postId before streamId)
assert.deepEqual(matchRecordsForElement([...streamRecs, { url: "https://cdn/post_abc123_x.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" }], { tabId: 7, frameId: 3, srcs: [], postId: "abc123", streamId: "zx9stream42", since: 0, now: T }, T).map((r) => r.url), ["https://cdn/post_abc123_x.mp4"], "postId tier before streamId");
// postId alone on generic CDN host (no streamId field needed)
assert.deepEqual(matchRecordsForElement([{ url: "https://media.example/embed/abc123/player.m3u8", tabId: 7, frameId: 3, time: T, type: "application/x-mpegurl" }], { tabId: 7, frameId: 3, srcs: [], postId: "abc123", since: 0, now: T }, T).map((r) => r.url), ["https://media.example/embed/abc123/player.m3u8"], "postId tier generic host");

// Generic blob/MSE fallback (background-only extraPool): element src blob:
// only, so src/postId tiers all miss; any-host same-tab candidates inside
// since window win (background sets blobOnly from el srcs, never page host).
const mseRecs = [
  { url: "https://media.example/v/abc123/movie.mp4?sig=xyz", tabId: 7, frameId: 3, time: T + 5000, type: "video/mp4" },
  { url: "https://static.other-cdn.net/v/xyz.mp4", tabId: 7, frameId: 3, time: T + 6000, type: "video/mp4" },
  { url: "https://media.example/v/old.mp4", tabId: 7, frameId: 3, time: T - 99999, type: "video/mp4" }, // outside since window
  { url: "https://media.example/v/wrongtab.mp4", tabId: 8, frameId: 3, time: T + 5000, type: "video/mp4" }, // wrong tab
];
const mseEl = { tabId: 7, frameId: 3, srcs: ["blob:https://player.example/1"], postId: "DAbc123XyZ", since: T, now: T, blobOnly: true };
// main pool empty (records evicted/killed), extraPool = candidates: window hit, any host
assert.deepEqual(
  matchRecordsForElement([], mseEl, T, mseRecs).map((r) => r.url),
  ["https://media.example/v/abc123/movie.mp4?sig=xyz", "https://static.other-cdn.net/v/xyz.mp4"],
  "blob fallback: any-host in-window, old + wrong-tab excluded");
// playlist-preference: playlist under same blob element drops segments/files
const msePl = [
  { url: "https://media.example/v/clip/seg-1.ts", tabId: 7, frameId: 3, time: T + 1000, type: "video/mp2t" },
  { url: "https://media.example/v/clip/movie.mp4", tabId: 7, frameId: 3, time: T + 2000, type: "video/mp4" },
  { url: "https://media.example/v/clip/list.m3u8", tabId: 7, frameId: 3, time: T + 3000, type: "application/x-mpegurl" },
];
assert.deepEqual(
  matchRecordsForElement([], mseEl, T, msePl).map((r) => r.url),
  ["https://media.example/v/clip/list.m3u8"],
  "blob fallback: playlist alone, segment + file dropped");
// segment-under-playlist already in rank pool: rank keeps playlist+file only
assert.deepEqual(
  rankCandidates(msePl).map((e) => e.url),
  ["https://media.example/v/clip/list.m3u8", "https://media.example/v/clip/movie.mp4"],
  "rank: playlist preferred, segment dropped, file kept");
// no blobOnly -> no fallback (content-reachable calls never widen)
assert.deepEqual(matchRecordsForElement([], { tabId: 7, frameId: 3, srcs: [], postId: "DAbc123XyZ", since: T, now: T }, T, mseRecs), [], "no blobOnly: extraPool ignored");
assert.deepEqual(matchRecordsForElement([], { ...mseEl, blobOnly: false }, T, mseRecs), [], "blobOnly=false: extraPool ignored");
// real (non-blob) src present -> blobOnly signal does not arm fallback
assert.deepEqual(matchRecordsForElement([], { ...mseEl, srcs: ["https://media.example/direct.mp4"] }, T, mseRecs), [], "non-blob src: no blob fallback");
// no since -> no fallback
assert.deepEqual(matchRecordsForElement([], { ...mseEl, since: 0 }, T, mseRecs), [], "since=0: no blob fallback");
// src tier still wins over blob fallback
const mseSrc = [{ url: "https://media.example/v/mine.mp4", tabId: 7, frameId: 3, time: T, type: "video/mp4" }];
assert.deepEqual(
  matchRecordsForElement(mseSrc, { tabId: 7, frameId: 3, srcs: ["https://media.example/v/mine.mp4"], postId: "DAbc123XyZ", since: T, now: T, blobOnly: true }, T, mseRecs).map((r) => r.url),
  ["https://media.example/v/mine.mp4"], "src tier exclusive over blob fallback");
// cross-feed isolation: two blob videos, disjoint timing windows -> own window only
const feedRecs = [
  { url: "https://media.example/feed/v1.mp4", tabId: 7, frameId: 3, time: T + 5000, type: "video/mp4" },
  { url: "https://media.example/feed/v2.mp4", tabId: 7, frameId: 3, time: T + 120000, type: "video/mp4" },
];
assert.deepEqual(
  matchRecordsForElement([], { tabId: 7, frameId: 3, srcs: ["blob:https://player.example/a"], since: T, now: T, blobOnly: true }, T, feedRecs).map((r) => r.url),
  ["https://media.example/feed/v1.mp4"], "cross-feed: video A window excludes video B");
assert.deepEqual(
  matchRecordsForElement([], { tabId: 7, frameId: 3, srcs: ["blob:https://player.example/b"], since: T + 115000, now: T + 115000, blobOnly: true }, T + 115000, feedRecs).map((r) => r.url),
  ["https://media.example/feed/v2.mp4"], "cross-feed: video B window excludes video A");
// firefox parity
assert.deepEqual(
  also.matchRecordsForElement([], mseEl, T, mseRecs).map((r) => r.url),
  ["https://media.example/v/abc123/movie.mp4?sig=xyz", "https://static.other-cdn.net/v/xyz.mp4"], "blob fallback firefox parity");
assert.deepEqual(
  also.matchRecordsForElement([], mseEl, T, msePl).map((r) => r.url),
  ["https://media.example/v/clip/list.m3u8"], "blob playlist-preference firefox parity");

console.log("rank.test.mjs OK", urls);
