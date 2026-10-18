// rune: node extensions/rank.test.mjs
import assert from "node:assert";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { rankCandidates } = require("./chromium/rank.js");
const also = require("./firefox/rank.js");
assert.equal(typeof also.rankCandidates, "function"); // firefox copy loads too

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
console.log("rank.test.mjs OK", urls);
