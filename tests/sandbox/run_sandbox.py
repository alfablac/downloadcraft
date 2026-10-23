#!/usr/bin/env python3
"""Local sandbox for the Chromium extension video-capture button.

No real / login-gated sites are contacted: Playwright request routing
fulfills fake-host URLs (youtube.com, instagram.com, tiktok.com, *.example.test)
with inline fixture HTML, and every other request is fulfilled locally or
aborted by the same route handler.

What is exercised, per fixture:
  * extensions/chromium/rank.js then extensions/chromium/content.js are
    loaded (in manifest order) into the fixture page;
  * the per-video download button (selector [data-downloadcraft-btn="1"],
    created by content.js attachBtn) appears where a sized <video> exists;
  * activating the button (or dispatching the toolbar message
    {type: "downloadcraft-scan"} for the no-<video> og:video fixture)
    opens #downloadcraft-panel, and clicking a panel item produces a
    captured chrome.runtime.sendMessage payload of type
    "downloadcraft-video-panel" offering either intent="page" with the
    per-video page URL, or a stream/manifest URL.

Usage (from repo root):
    python tests/sandbox/run_sandbox.py
    python tests/sandbox/run_sandbox.py --headed

Exit code is non-zero if any fixture fails.
"""

from __future__ import annotations

import argparse
import sys
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse

REPO_ROOT = Path(__file__).resolve().parents[2]
RANK_JS_PATH = REPO_ROOT / "extensions" / "chromium" / "rank.js"
CONTENT_JS_PATH = REPO_ROOT / "extensions" / "chromium" / "content.js"

# ---------------------------------------------------------------------------
# Extension API stub. Installed with add_init_script so window.chrome exists
# before rank.js / content.js run. content.js only touches:
#   api.runtime.sendMessage(msg[, cb])   (fetchCandidates + panel clicks)
#   api.runtime.onMessage.addListener(fn) (downloadcraft-scan / -grab-all)
# chrome.storage is stubbed minimally as well (unused by content.js today,
# but background-facing code paths and future content code expect it).
# All sendMessage payloads are recorded in window.__wcSent for assertions,
# and window.__wcDispatch(msg) lets the harness invoke the registered
# onMessage listeners (simulating the toolbar / context-menu scan).
# ---------------------------------------------------------------------------
STUB_JS = r"""
window.__wcSent = [];
window.__wcListeners = [];
window.__wcCandidates = [];
window.__wcDispatch = function (msg) {
  window.__wcListeners.slice().forEach(function (fn) {
    try { fn(msg, {}, function () {}); } catch (e) { console.error(e); }
  });
};
(function () {
  var storageArea = {
    get: function (keys, cb) { if (typeof cb === "function") cb({}); return Promise.resolve({}); },
    set: function (obj, cb) { if (typeof cb === "function") cb(); return Promise.resolve(); },
    remove: function (keys, cb) { if (typeof cb === "function") cb(); return Promise.resolve(); },
    clear: function (cb) { if (typeof cb === "function") cb(); return Promise.resolve(); }
  };
  window.chrome = {
    runtime: {
      id: "webcraft-sandbox",
      lastError: null,
      sendMessage: function (msg, cb) {
        var copy;
        try { copy = JSON.parse(JSON.stringify(msg)); } catch (e) { copy = msg; }
        window.__wcSent.push(copy);
        var resp = (msg && msg.type === "getCandidates")
          ? { type: "candidates", candidates: window.__wcCandidates || [] }
          : { type: "ok" };
        if (typeof cb === "function") setTimeout(function () { cb(resp); }, 0);
        return Promise.resolve(resp);
      },
      onMessage: {
        addListener: function (fn) { window.__wcListeners.push(fn); },
        removeListener: function (fn) {
          var i = window.__wcListeners.indexOf(fn);
          if (i >= 0) window.__wcListeners.splice(i, 1);
        }
      }
    },
    storage: { local: storageArea, session: storageArea, sync: storageArea }
  };
})();
"""


def page_html(title: str, body: str, head: str = "") -> str:
    return (
        "<!doctype html><html><head><meta charset=\"utf-8\">"
        f"<title>{title}</title>{head}</head>"
        f"<body style=\"margin:0;font-family:sans-serif\">{body}</body></html>"
    )


@dataclass
class Fixture:
    fid: str
    name: str
    url: str
    html: str
    expect_buttons: int
    activation: str = "button"  # "button" | "scan"
    button_index: int = 0
    panel_text: str | None = None  # click panel item containing this; None = first
    # Acceptable (intent, url) pairs for the captured download payload.
    acceptable: list = field(default_factory=list)


YOUTUBE_WATCH = "https://www.youtube.com/watch?v=SandboxVid1"
INSTAGRAM_REEL = "https://www.instagram.com/reel/SandboxReel1/"
GENERIC_PAGE = "https://videos.example.test/title/sandbox-movie-7"
GENERIC_STREAM = "https://cdn.example.test/media/sandbox-movie-7.mp4"
FEED_PAGE_B = "https://www.youtube.com/watch?v=FeedVideoB2"
TIKTOK_PAGE = "https://www.tiktok.com/@sandbox.creator/video/7298765432101234567"
OG_PAGE = "https://blog.example.test/posts/sandbox-og-story"
OG_STREAM = "https://cdn.example.test/media/og-only-movie.mp4"

FIXTURES: list[Fixture] = [
    # (a) YouTube-like watch page, direct <video src> (googlevideo-shaped).
    Fixture(
        fid="a-youtube-watch",
        name="youtube watch <video src>",
        url=YOUTUBE_WATCH,
        html=page_html(
            "Sandbox Video - YouTube",
            '<div id="player" style="width:640px;height:360px">'
            '<video controls preload="metadata" '
            'src="https://rr1---sn-sandbox.googlevideo.com/videoplayback?id=SandboxVid1&amp;itag=22" '
            'style="display:block;width:640px;height:360px"></video></div>',
        ),
        expect_buttons=1,
        acceptable=[("page", YOUTUBE_WATCH)],
    ),
    # (b) Instagram-like reel: blob: <video> is 0x0, wrapper carries the size
    # (Meta layout quirk exercised by content.js videoRect()).
    Fixture(
        fid="b-instagram-reel",
        name="instagram reel blob: 0x0 video, sized wrapper",
        url=INSTAGRAM_REEL,
        html=page_html(
            "Sandbox Reel - Instagram",
            '<div class="reel-wrap" style="width:360px;height:640px;position:relative;background:#111">'
            '<video muted loop playsinline style="width:0;height:0"></video></div>'
            "<script>(function(){var v=document.querySelector('video');"
            "try{v.src=URL.createObjectURL(new Blob(['sandbox'],{type:'video/mp4'}));}catch(e){}})();</script>",
        ),
        expect_buttons=1,
        # content.js cleanPermalink() truncates at the video-claiming segment,
        # dropping the trailing slash: both forms name the same reel.
        acceptable=[("page", INSTAGRAM_REEL), ("page", INSTAGRAM_REEL.rstrip("/"))],
    ),
    # (c) Generic <video> page on an unknown (non-registry) host shape.
    Fixture(
        fid="c-generic-video",
        name="generic <video> unknown host",
        url=GENERIC_PAGE,
        html=page_html(
            "Sandbox Movie",
            f'<video controls preload="metadata" src="{GENERIC_STREAM}" '
            'style="display:block;width:640px;height:360px"></video>',
        ),
        expect_buttons=1,
        acceptable=[("page", GENERIC_PAGE), ("stream", GENERIC_STREAM)],
    ),
    # (d) Feed page: multiple video cards, each with a per-video anchor
    # permalink. Activating the *second* card's button must offer the
    # second video's page URL, never the feed URL.
    Fixture(
        fid="d-feed-cards",
        name="feed, multiple cards w/ per-video anchors",
        url="https://www.youtube.com/",
        html=page_html(
            "YouTube",
            "".join(
                f'<ytd-rich-item-renderer style="display:block;width:320px;height:180px">'
                f'<a href="/watch?v=FeedVideo{letter}{n}">'
                f'<video preload="metadata" src="https://cdn.example.test/feed/{letter}.mp4" '
                f'style="display:block;width:320px;height:180px"></video></a>'
                f"</ytd-rich-item-renderer>"
                for letter, n in (("A", 1), ("B", 2), ("C", 3))
            ),
        ),
        expect_buttons=3,
        button_index=1,
        acceptable=[("page", FEED_PAGE_B)],
    ),
    # (e) TikTok-like photo/video page (registry /video/<id> shape).
    Fixture(
        fid="e-tiktok-video",
        name="tiktok video page",
        url=TIKTOK_PAGE,
        html=page_html(
            "Sandbox TikTok",
            '<div style="width:360px;height:640px">'
            '<video controls preload="metadata" '
            'src="https://v19.tiktokcdn-us.com/sandbox/tiktok-sandbox.mp4" '
            'style="display:block;width:360px;height:640px"></video></div>',
        ),
        expect_buttons=1,
        acceptable=[("page", TIKTOK_PAGE)],
    ),
    # (f) og:video meta only, no <video> element: no per-video button is
    # expected; the toolbar scan must surface the og:video stream.
    Fixture(
        fid="f-og-video-only",
        name="og:video meta only, no <video>",
        url=OG_PAGE,
        html=page_html(
            "Sandbox OG Story",
            "<article><h1>Sandbox OG Story</h1><p>Text only; media is declared in head meta.</p></article>",
            head=(
                f'<meta property="og:url" content="{OG_PAGE}">'
                f'<meta property="og:video" content="{OG_STREAM}">'
                '<meta property="og:video:type" content="video/mp4">'
            ),
        ),
        expect_buttons=0,
        activation="scan",
        panel_text="og-only-movie.mp4",
        acceptable=[("stream", OG_STREAM), ("page", OG_PAGE)],
    ),
]


@dataclass
class Result:
    fixture: Fixture
    passed: bool
    detail: str
    buttons: int = -1
    panel_items: list = field(default_factory=list)
    payload: dict | None = None


def run_fixture(browser, fixture: Fixture, rank_js: str, content_js: str) -> Result:
    context = browser.new_context(viewport={"width": 1280, "height": 800})
    page = context.new_page()
    page_errors: list[str] = []
    page.on("pageerror", lambda exc: page_errors.append(str(exc)))

    def handle(route):
        req = route.request
        parsed = urlparse(req.url)
        if req.url == fixture.url or (
            req.is_navigation_request() and req.frame_id == page.main_frame
        ):
            route.fulfill(status=200, content_type="text/html", body=fixture.html)
        elif parsed.path.lower().endswith((".mp4", ".webm", ".m3u8", ".mpd")) or (
            "videoplayback" in parsed.path
        ):
            # Dummy media bytes: enough for the element to keep currentSrc;
            # decode success is irrelevant to button/panel behaviour.
            route.fulfill(status=200, content_type="video/mp4", body=b"")
        else:
            route.fulfill(status=404, content_type="text/plain", body="")

    try:
        page.add_init_script(script=STUB_JS)
        page.route("**/*", handle)
        page.goto(fixture.url, wait_until="domcontentloaded", timeout=15000)
        # Manifest order: rank.js before content.js.
        page.add_script_tag(content=rank_js)
        page.add_script_tag(content=content_js)

        btn_locator = page.locator('[data-downloadcraft-btn="1"]')
        if fixture.expect_buttons > 0:
            page.wait_for_selector(
                '[data-downloadcraft-btn="1"]', state="attached", timeout=5000
            )
            # Give the debounced MutationObserver scans a chance to attach
            # buttons for later cards too.
            page.wait_for_timeout(700)
        else:
            page.wait_for_timeout(900)
        buttons = btn_locator.count()
        if buttons != fixture.expect_buttons:
            return Result(
                fixture, False,
                f"button count {buttons}, expected {fixture.expect_buttons}"
                + (f"; pageerrors={page_errors}" if page_errors else ""),
                buttons=buttons,
            )

        # Activate: click the per-video button, or simulate the toolbar scan.
        if fixture.activation == "button":
            clicked = page.evaluate(
                """(idx) => {
                  const b = document.querySelectorAll('[data-downloadcraft-btn="1"]')[idx];
                  if (!b) return false; b.click(); return true;
                }""",
                fixture.button_index,
            )
            if not clicked:
                return Result(fixture, False, "button click target missing", buttons=buttons)
        else:
            page.evaluate("() => window.__wcDispatch({type: 'downloadcraft-scan'})")

        page.wait_for_selector(
            "#downloadcraft-panel .downloadcraft-items button",
            state="attached", timeout=7000,
        )
        panel_items = page.locator(
            "#downloadcraft-panel .downloadcraft-items button"
        ).all_inner_texts()
        if not panel_items:
            return Result(fixture, False, "panel opened with no items", buttons=buttons)

        clicked_panel = page.evaluate(
            """(needle) => {
              const bs = [...document.querySelectorAll('#downloadcraft-panel .downloadcraft-items button')];
              const b = needle ? bs.find((x) => x.textContent.includes(needle)) : bs[0];
              if (!b) return false; b.click(); return true;
            }""",
            fixture.panel_text,
        )
        if not clicked_panel:
            return Result(
                fixture, False,
                f"panel item containing {fixture.panel_text!r} not found; items={panel_items}",
                buttons=buttons, panel_items=panel_items,
            )

        page.wait_for_function(
            "() => window.__wcSent.some((m) => m && m.type === 'downloadcraft-video-panel')",
            timeout=5000,
        )
        payload = page.evaluate(
            """() => {
              const hits = window.__wcSent.filter((m) => m && m.type === 'downloadcraft-video-panel');
              return hits.length ? hits[hits.length - 1] : null;
            }"""
        )
        if not payload:
            return Result(
                fixture, False, "no downloadcraft-video-panel payload captured",
                buttons=buttons, panel_items=panel_items,
            )
        got = (payload.get("intent") or "stream", payload.get("url") or "")
        if got in [tuple(x) for x in fixture.acceptable]:
            return Result(
                fixture, True, f"payload intent={got[0]} url={got[1]}",
                buttons=buttons, panel_items=panel_items, payload=payload,
            )
        return Result(
            fixture, False,
            f"payload intent={got[0]} url={got[1]} not in acceptable {fixture.acceptable}",
            buttons=buttons, panel_items=panel_items, payload=payload,
        )
    except Exception as exc:  # keep going: one bad fixture must not hide others
        detail = f"{type(exc).__name__}: {exc}"
        if page_errors:
            detail += f"; pageerrors={page_errors}"
        return Result(fixture, False, detail)
    finally:
        try:
            context.close()
        except Exception:
            pass


def main(argv=None) -> int:
    # Panel labels contain "▶"; Windows consoles default to cp1252.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--headed", action="store_true", help="run Chromium headed")
    parser.add_argument(
        "--only", default="", help="comma-separated fixture ids to run (default: all)"
    )
    args = parser.parse_args(argv)

    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print("playwright is not installed (python -m pip install playwright)", file=sys.stderr)
        return 2

    rank_js = RANK_JS_PATH.read_text(encoding="utf-8")
    content_js = CONTENT_JS_PATH.read_text(encoding="utf-8")

    fixtures = FIXTURES
    if args.only:
        wanted = {x.strip() for x in args.only.split(",") if x.strip()}
        fixtures = [f for f in FIXTURES if f.fid in wanted]
        if not fixtures:
            print(f"no fixtures matched --only={args.only!r}", file=sys.stderr)
            return 2

    results: list[Result] = []
    with sync_playwright() as pw:
        browser = pw.chromium.launch(headless=not args.headed)
        try:
            for fixture in fixtures:
                try:
                    results.append(run_fixture(browser, fixture, rank_js, content_js))
                except Exception:
                    results.append(
                        Result(fixture, False, "harness crash:\n" + traceback.format_exc())
                    )
        finally:
            browser.close()

    print()
    print(f"{'fixture':<20} {'buttons':<8} {'result':<6} detail")
    print("-" * 110)
    for r in results:
        print(
            f"{r.fixture.fid:<20} {r.buttons:<8} "
            f"{'PASS' if r.passed else 'FAIL':<6} {r.fixture.name} :: {r.detail}"
        )
        if r.panel_items:
            print(f"{'':<20} {'':<8} {'':<6} panel: {' | '.join(r.panel_items)[:180]}")
    passed = sum(1 for r in results if r.passed)
    print("-" * 110)
    print(f"{passed}/{len(results)} fixtures passed")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
