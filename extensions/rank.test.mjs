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
assert.equal(urls[0], "https://x/big.mp4", "biggest file first");
assert.equal(urls[1], "https://x/v.mp4?bytestart=500&byteend=999", "largest byte-range variant kept");
assert.equal(urls[2], "https://x/long.mp4", "duration beats equal-size file");
assert.equal(urls[3], "https://x/small.mp4");
assert.equal(urls[4], "https://x/movie.m3u8", "playlist below files");
assert.ok(!urls.some((u) => u.endsWith(".ts")), "segments filtered when playlist known, got " + urls.join(","));
// playlist present: segments filtered out entirely
const bare = rankCandidates([{ url: "https://x/a.ts" }, { url: "https://x/a.m3u8" }]);
assert.deepEqual(bare.map((e) => e.url), ["https://x/a.m3u8"]);
// segments kept bare when no playlist exists
assert.ok(rankCandidates([{ url: "https://x/a.ts" }]).length === 1);
// junk schemes + DRM endpoints never offered
assert.deepEqual(rankCandidates([{ url: "blob:https://x/abc" }, { url: "data:video/mp4;base64,AA" }, { url: "https://x/widevine/license" }]), []);
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

console.log("rank.test.mjs OK", urls);
