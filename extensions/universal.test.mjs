// Universal-capture contract, no browser. Sources read as text; behavioral
// reuse of rank.js via createRequire like rank.test.mjs. Hard invariants from
// v0.22.4 cross-site regression fix must never regress:
//  - button anchored in document.body, fullscreen subtree only during FS
//  - containment hover non-IG, coordinate hover IG-scoped only
//  - videoRect ancestor fallback IG-scoped only
//  - Chromium segment sniff reject present (restored in v0.22.4)
// Run: node extensions/universal.test.mjs
import { readFileSync } from "node:fs";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const read = (p) => readFileSync(new URL(p, import.meta.url), "utf8");

let n = 0;
const ok = (cond, msg) => { assert(cond, msg); n++; };

// Hostnames that must never appear in matching/ranking/sniffing logic.
// og:/twitter: meta names in content pageMediaUrls are standard meta tags,
// not hostnames; they are not listed here.
// Word-boundary hostname match: bare "x.com" substring false-positives on
// prose ("max.com", example text in comments). Comments mentioning a site
// are not logic; the regex/symbol checks below catch actual logic use.
// Comments are not logic: strip // and /* */ before host scan (string
// literals with URLs survive; only comment prose is dropped).
function stripComments(src) {
  return src.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/(^|[^:"'])\/\/[^\n]*/g, "$1");
}
function mentionsHost(src, h) {
  const esc = h.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp("(^|[^a-z0-9-])" + esc + "([^a-z0-9-]|$)", "i").test(src);
}
const SITE_HOSTS = [
  "instagram.com", "cdninstagram.com", "fbcdn.net",
  "twitter.com", "twimg.com", "x.com",
  "reddit.com", "redd.it",
  "youtube.com", "youtu.be", "googlevideo.com",
  "tiktok.com", "vimeo.com", "dailymotion.com", "dai.ly",
  "twitch.tv", "facebook.com", "fb.watch", "soundcloud.com",
  "bilibili.com", "globo.com", "sharepoint.com", "onedrive",
];

// (a) rank.js: pure matching — zero site hostnames, chromium+firefox.
for (const rel of ["./chromium/rank.js", "./firefox/rank.js"]) {
  const src = read(rel);
  for (const h of SITE_HOSTS) ok(!mentionsHost(stripComments(src), h), `${rel}: forbidden site host ${h}`);
  ok(!/pageHost|igPair/.test(src), `${rel}: pageHost host-scoping removed, generic blob fallback only`);
  ok(src.includes("blobOnly"), `${rel}: generic blobOnly extra-pool signal present`);
}

// (a)+(b) background.js: sniff + element fallback host-agnostic.
for (const rel of ["./chromium/background.js", "./firefox/background.js"]) {
  const src = read(rel);
  for (const h of SITE_HOSTS) ok(!mentionsHost(stripComments(src), h), `${rel}: forbidden site host ${h}`);
  ok(!/IG_CDN_RE|IG_PAGE_RE|isIgCdn|isIgDoc|pageHost/.test(src), `${rel}: IG sniff/fallback symbols removed`);
  ok(src.includes("blobOnly: true"), `${rel}: bg derives generic blobOnly fallback (blob src, no host)`);
  ok(src.includes("matchRecordsForElement"), `${rel}: delegates matching to rank.js`);
}

// Chromium segment sniff reject restored (v0.22.4): video/audio MIME with
// segment-shaped basename rejected; Firefox relies on rank segment drop.
{
  const src = read("./chromium/background.js");
  ok(/seg\|segment\|chunk\|frag/.test(src) && src.includes("return false"), "chromium background: segment sniff reject present");
  ok(src.includes('path.endsWith(".mpd")') && src.includes('path.endsWith(".m3u8")'), "chromium background: generic playlist sniff present");
  const ff = read("./firefox/background.js");
  ok(ff.includes('path.endsWith(".mpd")') || ff.includes('path.endsWith(".m3u8")') || /\.mpd\|\.m3u8|isPlaylist/.test(ff), "firefox background: generic playlist sniff present");
}

// (c) content.js: matching/permalink/visibility machinery fully generic —
// no site hostname anywhere; obscured-player detection is geometric
// (elementFromPoint at video center), not host-based.
for (const rel of ["./chromium/content.js", "./firefox/content.js"]) {
  const src = read(rel);
  for (const h of SITE_HOSTS) ok(!mentionsHost(stripComments(src), h), `${rel}: forbidden site host ${h}`);
  ok(!/cdninstagram|fbcdn|v\.redd|amplify_video|ext_tw_video/.test(src), `${rel}: per-site CDN/stream-id regexes removed`);
  ok(!/streamIdFor|streamIdFromUrl/.test(src), `${rel}: per-site streamId extraction removed`);
  ok(!/isVideoPageUrl/.test(src), `${rel}: per-site video-page allowlist removed (yt-dlp page row unconditional)`);
  ok(!/CARD_SEL\s*=.*(tweet|shreddit|ytd-)/.test(src), `${rel}: per-site card selectors removed`);
  // Generic mechanisms present.
  ok(/link\[rel="canonical"\]/.test(src), `${rel}: canonical permalink present`);
  ok(src.includes("data-permalink"), `${rel}: data-permalink card permalink present`);
  ok(/closest\("a\[href\]"\)/.test(src), `${rel}: wrapping-anchor permalink present`);
  ok(/querySelector\("time"\)/.test(src), `${rel}: time-anchor permalink present`);
  ok(/TRACKING_KEYS|utm_/.test(src), `${rel}: generic tracking-param strip present`);
  ok(src.includes("since - 2000") && src.includes("since + 45000"), `${rel}: timing window generic`);
  // Page (yt-dlp) row unconditional: page-intent row built for every video
  // (chromium mapped.unshift(pageItem), firefox items.push intent:"page").
  ok(/intent: "page"/.test(src) && !/if \([^\n]*\) [^\n]*intent: "page"/.test(src), `${rel}: yt-dlp page row offered for every video/page`);
  // ---- v0.22.4 hard invariants ----
  ok(/appendChild\(btn\)/.test(src) && /\(document\.body \|\| document\.documentElement\)\.appendChild\(btn\)/.test(src), `${rel}: button anchored in document.body`);
  ok(!/video\.parentElement\.appendChild\(btn\)|parentElement\.appendChild\(btn\)/.test(src), `${rel}: no parent-anchoring of button`);
  ok(/fullscreen/.test(src) && /fs !== video && inFs\(video, fs\) \? fs : null/.test(src), `${rel}: button moves into fullscreen subtree only during fullscreen`);
  ok((src.match(/appendChild\(btn\)/g) || []).length === 2 && src.includes("host.appendChild(btn)"), `${rel}: btn anchors only via document.body + fullscreen host re-host`);
  ok(!/isSemiVisibleHost|isInstagram/.test(src), `${rel}: host-scoped visibility helper removed`);
  ok(/function videoObscured/.test(src) && /elementFromPoint/.test(src), `${rel}: generic obscured-player detection present`);
  const obBranch = src.indexOf("if (videoObscured(video)) {", src.indexOf("let hoverRaf") - 200);
  const elseBranch = src.indexOf("} else {", obBranch);
  ok(obBranch !== -1 && elseBranch > obBranch, `${rel}: hover branch scoped: obscured vs containment`);
  const obHover = src.slice(obBranch, elseBranch);
  const elseHover = src.slice(elseBranch, elseBranch + 400);
  ok(/clientX >= r\.left/.test(obHover), `${rel}: coordinate hover inside obscured branch only`);
  ok(/contains\(e\.target\)/.test(elseHover), `${rel}: containment hover in non-obscured branch`);
  ok((src.match(/clientX >= r\.left/g) || []).length === 1, `${rel}: coordinate hover appears exactly once (obscured-scoped)`);
  // videoRect ancestor fallback obscured-scoped, not global.
  const vr = src.slice(src.indexOf("function videoRect"));
  ok(/if \(!videoObscured\(video\)\) return r;/.test(vr.slice(0, 600)), `${rel}: videoRect ancestor fallback obscured-scoped`);
  ok(/playerRoot/.test(src), `${rel}: playerRoot containment hover host present`);
}

// (b) Chromium/Firefox copies behaviorally in sync: rank.js byte-identical,
// background/content differ only in browser-API surface (spot-checked above).
{
  const a = read("./chromium/rank.js"), b = read("./firefox/rank.js");
  ok(a === b, "rank.js chromium/firefox byte-identical");
  for (const name of ["rankCandidates", "matchRecordsForElement", "kindOf", "canonicalUrl"]) {
    ok(a.includes(name) && b.includes(name), `rank exports ${name} both browsers`);
  }
}

// Behavioral spot-check on shipped functions (generic hosts only).
const { rankCandidates, matchRecordsForElement } = require("./chromium/rank.js");
const T = 1_700_000_000_000;
assert.deepEqual(
  matchRecordsForElement([], { tabId: 1, srcs: ["blob:https://any.example/x"], since: T, blobOnly: true }, T,
    [{ url: "https://cdn.anything.example/v/list.m3u8", tabId: 1, time: T + 10, type: "application/x-mpegurl" },
     { url: "https://cdn.anything.example/v/seg-1.ts", tabId: 1, time: T + 10, type: "video/mp2t" }]).map((r) => r.url),
  ["https://cdn.anything.example/v/list.m3u8"], "behavioral: unknown-host blob fallback playlist-preference");
n++;
assert.deepEqual(rankCandidates([
  { url: "https://unknown.example/watch/abc" }, { url: "https://unknown.example/v.m3u8" },
]).map((e) => e.url), ["https://unknown.example/v.m3u8", "https://unknown.example/watch/abc"], "behavioral: ranking host-agnostic");
n++;

// (d) Rust side: delegate to yt-dlp binary, no ported extractor logic,
// no per-site page allowlist left in is_video_page.
{
  const rs = read("../src/ytdlp.rs");
  const fnStart = rs.indexOf("pub fn is_video_page");
  ok(fnStart !== -1, "ytdlp.rs: is_video_page exists");
  const body = rs.slice(fnStart, rs.indexOf("\n}", fnStart));
  for (const h of ["youtube.com", "youtu.be", "instagram.com", "vimeo.com", "tiktok.com", "twitter.com", "reddit.com", "facebook.com", "dailymotion.com", "twitch.tv", "soundcloud.com", "bilibili.com"]) {
    ok(!body.includes(`"${h}"`), `ytdlp.rs is_video_page: no per-site allowlist entry ${h}`);
  }
  ok(/is_manifest_url/.test(body) && /is_direct_media_url/.test(body), "ytdlp.rs is_video_page: generic file/manifest exclusion only");
  ok(/Command::new|yt-dlp/.test(rs), "ytdlp.rs: delegates to real yt-dlp binary");
  ok(!/fn extract_|struct .*Extractor|impl .*Extractor/.test(rs), "ytdlp.rs: no ported extractor implementations");
}

console.log(`universal.test.mjs OK (${n} assertions)`);
