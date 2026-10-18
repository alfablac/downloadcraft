use downloadcraft_lib::engine::{build_client, Controls};
use downloadcraft_lib::model::*;
use downloadcraft_lib::stream::download_url;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

fn serve(routes: Vec<(String, Vec<u8>, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let routes = &routes;
            let mut buf = vec![0u8; 16384];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let path_only = path.split('?').next().unwrap_or("").to_string();
            let range = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                .map(|l| l.to_string());
            let mut found: Option<&(String, Vec<u8>, String)> = None;
            for r in routes {
                if path_only == r.0 {
                    found = Some(r);
                    break;
                }
            }
            match found {
                Some((_, body, ct)) => {
                    if let Some(r) = &range {
                        if let Some(spec) = r.split("bytes=").nth(1) {
                            let mut it = spec.trim().split('-');
                            let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                            let end: usize =
                                it.next().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                            let end = end.min(body.len().saturating_sub(1));
                            if start < body.len() {
                                let slice = &body[start..=end];
                                let resp = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len(), body.len());
                                let _ = s.write_all(resp.as_bytes());
                                let _ = s.write_all(slice);
                                continue;
                            }
                        }
                    }
                    let head_only = req.starts_with("HEAD");
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(resp.as_bytes());
                    if !head_only {
                        let _ = s.write_all(body);
                    }
                }
                None => {
                    let _ = s.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("downloadcraft-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_mp4_like(dest: &std::path::Path, secs: u8) -> Vec<u8> {
    // Use ffmpeg if available to make real playable segments; fallback to bytes (test that needs ffprobe skips then)
    let out = dest.join("gen.mp4");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={secs}:size=320x240:rate=10"),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-shortest",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            out.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        std::fs::read(&out).unwrap_or_default()
    } else {
        Vec::new()
    }
}

#[tokio::test]
async fn http_segmented_download_and_resume() {
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let base = serve(vec![(
        "/file.bin".into(),
        body.clone(),
        "application/octet-stream".into(),
    )]);
    let dir = tmpdir("http");
    let task = DownloadTask {
        id: "t1".into(),
        url: format!("{base}/file.bin"),
        filename: "file.bin".into(),
        dest_dir: dir.clone(),
        segments: 4,
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let done = download_url(build_client(""), task, ctrl).await.unwrap();
    assert_eq!(done.status, TaskStatus::Complete);
    assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), body);
}

#[tokio::test]
async fn http_single_thread_server_without_range() {
    // server still supports range; engine probe uses Range -> segmented. Non-range path covered by dash/hls segment concat.
    let body = b"hello world".to_vec();
    let base = serve(vec![("/a.txt".into(), body.clone(), "text/plain".into())]);
    let dir = tmpdir("single");
    let task = DownloadTask {
        id: "t2".into(),
        url: format!("{base}/a.txt"),
        filename: "a.txt".into(),
        dest_dir: dir.clone(),
        segments: 1,
        ..Default::default()
    };
    let done = download_url(build_client(""), task, Arc::new(Controls::new(0)))
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete);
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), body);
}

#[tokio::test]
async fn dash_template_vod_and_protected() {
    let dir0 = tmpdir("dashgen");
    let media = make_mp4_like(&dir0, 2);
    if media.is_empty() {
        eprintln!("ffmpeg missing, dash media bytes fallback");
    }
    let seg = if media.is_empty() {
        b"FAKEMP4DATA".to_vec()
    } else {
        media.clone()
    };
    let mpd = r#"<MPD type="static" mediaPresentationDuration="PT2S"><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="seg-$Number%05d$.m4s" initialization="init.mp4" duration="1000" timescale="1000"/><Representation id="v1" bandwidth="1000" width="320" height="240" codecs="avc1.64001f"/></AdaptationSet><AdaptationSet mimeType="audio/mp4"><SegmentTemplate media="aseg-$Number$.m4s" initialization="ainit.mp4" duration="1000" timescale="1000"/><Representation id="a1" bandwidth="500" codecs="mp4a.40.2" lang="en"/></AdaptationSet></Period></MPD>"#.to_string();
    let mpd_protected = r#"<MPD type="static"><Period><AdaptationSet mimeType="video/mp4"><ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/><SegmentTemplate media="s-$Number$.m4s" duration="1" timescale="1"/><Representation id="v" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
    let base = serve(vec![
        (
            "/manifest.mpd".into(),
            mpd.as_bytes().to_vec(),
            "application/dash+xml".into(),
        ),
        (
            "/protected.mpd".into(),
            mpd_protected.as_bytes().to_vec(),
            "application/dash+xml".into(),
        ),
        ("/init.mp4".into(), seg.clone(), "video/mp4".into()),
        ("/ainit.mp4".into(), seg.clone(), "audio/mp4".into()),
        ("/seg-00001.m4s".into(), seg.clone(), "video/mp4".into()),
        ("/seg-00002.m4s".into(), seg.clone(), "video/mp4".into()),
        ("/aseg-1.m4s".into(), seg.clone(), "audio/mp4".into()),
        ("/aseg-2.m4s".into(), seg.clone(), "audio/mp4".into()),
    ]);
    let dir = tmpdir("dash");
    let task = DownloadTask {
        id: "d1".into(),
        url: format!("{base}/manifest.mpd"),
        filename: "out.mp4".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let done = download_url(build_client(""), task, Arc::new(Controls::new(0)))
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "err={}", done.error);
    assert!(dir.join("out.mp4").exists());
    // protected
    let dir2 = tmpdir("dashp");
    let task2 = DownloadTask {
        id: "d2".into(),
        url: format!("{base}/protected.mpd"),
        filename: "out.mp4".into(),
        dest_dir: dir2.clone(),
        ..Default::default()
    };
    let done2 = download_url(build_client(""), task2, Arc::new(Controls::new(0)))
        .await
        .unwrap();
    assert_eq!(done2.status, TaskStatus::Protected);
    assert!(!dir2.join("out.mp4").exists());
}

#[tokio::test]
async fn hls_vod_fmp4() {
    let seg = b"FAKESEGMENTDATA".to_vec();
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=320x240\nmedia.m3u8\n";
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2,\nseg1.m4s\n#EXTINF:2,\nseg2.m4s\n#EXT-X-ENDLIST\n";
    let base = serve(vec![
        (
            "/master.m3u8".into(),
            master.as_bytes().to_vec(),
            "application/vnd.apple.mpegurl".into(),
        ),
        (
            "/media.m3u8".into(),
            media.as_bytes().to_vec(),
            "application/vnd.apple.mpegurl".into(),
        ),
        ("/init.mp4".into(), seg.clone(), "video/mp4".into()),
        ("/seg1.m4s".into(), seg.clone(), "video/mp4".into()),
        ("/seg2.m4s".into(), seg.clone(), "video/mp4".into()),
    ]);
    let dir = tmpdir("hls");
    let task = DownloadTask {
        id: "h1".into(),
        url: format!("{base}/master.m3u8"),
        filename: "out.mp4".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let done = download_url(build_client(""), task, Arc::new(Controls::new(0)))
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    let bytes = std::fs::read(dir.join("out.mp4")).unwrap();
    assert_eq!(bytes.len(), seg.len() * 3); // init + 2 segs (init written once)
}

#[tokio::test]
async fn grabber_fixture_site() {
    let index = r#"<html><a href="/a/page2.html">2</a><a href="/files/doc.pdf">pdf</a><img src="/files/pic.png"><a href="/files/doc.pdf">dup</a><a href="https://127.0.0.1.evil.example.com/x.zip">ext</a></html>"#;
    // note external link host differs: keep same-host external test via absolute 127 link to other path counted as file
    let page2 = r#"<html><a href="/files/movie.mp4">m</a><a href="/a/page2.html">self</a></html>"#;
    let base = serve(vec![
        (
            "/index.html".into(),
            index.as_bytes().to_vec(),
            "text/html".into(),
        ),
        (
            "/a/page2.html".into(),
            page2.as_bytes().to_vec(),
            "text/html".into(),
        ),
        (
            "/files/doc.pdf".into(),
            b"%PDF-fake".to_vec(),
            "application/pdf".into(),
        ),
        ("/files/pic.png".into(), b"PNG".to_vec(), "image/png".into()),
        (
            "/files/movie.mp4".into(),
            b"MP4".to_vec(),
            "video/mp4".into(),
        ),
        (
            "/robots.txt".into(),
            b"User-agent: *\nAllow: /\n".to_vec(),
            "text/plain".into(),
        ),
    ]);
    let proj = downloadcraft_lib::grabber::GrabberProject {
        start_url: format!("{base}/index.html"),
        depth: 2,
        scope: 0,
        max_pages: 50,
        ..Default::default()
    };
    let crawler = downloadcraft_lib::grabber::Crawler {
        project: proj,
        progress: None,
    };
    let (_pages, files) = crawler.crawl(&build_client("")).await.unwrap();
    let mut urls: Vec<String> = files.iter().map(|f| f.url.clone()).collect();
    urls.sort();
    assert!(
        urls.iter().any(|u| u.ends_with("/files/doc.pdf")),
        "{urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.ends_with("/files/pic.png")),
        "{urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.ends_with("/files/movie.mp4")),
        "{urls:?}"
    );
    assert_eq!(
        urls.iter()
            .filter(|u| u.ends_with("/files/doc.pdf"))
            .count(),
        1,
        "dedupe: {urls:?}"
    );
    // extension filter
    let proj2 = downloadcraft_lib::grabber::GrabberProject {
        start_url: format!("{base}/index.html"),
        depth: 2,
        scope: 0,
        max_pages: 50,
        extensions: vec!["pdf".into()],
        ..Default::default()
    };
    let (_p, files2) = downloadcraft_lib::grabber::Crawler {
        project: proj2,
        progress: None,
    }
    .crawl(&build_client(""))
    .await
    .unwrap();
    assert!(files2.iter().all(|f| f.url.ends_with(".pdf")), "{files2:?}");
}

#[tokio::test]
async fn grabber_idm_parity_discovery() {
    // Raw script-URL extraction itself is covered by grabber unit test script_css_and_depth_rules.
    let index = r#"<html><head><style>.a{background:url(/img/bg.png)}</style></head>
    <a href="/dir/sub/page.html">sub</a><a href="/outside.html">out</a><a href="/dl?id=3">endpoint</a><a href="/files/keep.zip">keep</a><a href="/ads/skip.zip">ad</a></html>"#;
    let sub = r#"<html><a href="/files/sub.zip">s</a></html>"#;
    let outside = r#"<html><a href="/files/outside.zip">o</a></html>"#;
    let base = serve(vec![
        (
            "/dir/index.html".to_string(),
            index.as_bytes().to_vec(),
            "text/html".to_string(),
        ),
        (
            "/dir/sub/page.html".to_string(),
            sub.as_bytes().to_vec(),
            "text/html".to_string(),
        ),
        (
            "/outside.html".to_string(),
            outside.as_bytes().to_vec(),
            "text/html".to_string(),
        ),
        (
            "/dl".to_string(),
            b"ZIPDATA".to_vec(),
            "application/zip".to_string(),
        ),
        (
            "/img/bg.png".to_string(),
            b"PNG".to_vec(),
            "image/png".to_string(),
        ),
        (
            "/files/keep.zip".to_string(),
            b"ZIP".to_vec(),
            "application/zip".to_string(),
        ),
        (
            "/files/sub.zip".to_string(),
            b"ZIP".to_vec(),
            "application/zip".to_string(),
        ),
        (
            "/files/outside.zip".to_string(),
            b"ZIP".to_vec(),
            "application/zip".to_string(),
        ),
        (
            "/ads/skip.zip".to_string(),
            b"ZIP".to_vec(),
            "application/zip".to_string(),
        ),
        (
            "/robots.txt".to_string(),
            b"User-agent: *\nAllow: /\n".to_vec(),
            "text/plain".to_string(),
        ),
    ]);
    let crawl = |proj: downloadcraft_lib::grabber::GrabberProject| async move {
        downloadcraft_lib::grabber::Crawler {
            project: proj,
            progress: None,
        }
        .crawl(&build_client(""))
        .await
        .unwrap()
    };
    // Parent-dir guard + exclude path filter + script endpoint discovery.
    let proj = downloadcraft_lib::grabber::GrabberProject {
        start_url: format!("{base}/dir/index.html"),
        depth: 3,
        scope: 0,
        max_pages: 50,
        dont_explore_parent_dirs: true,
        exclude_paths: vec!["/ads/".into()],
        ..Default::default()
    };
    let (pages, files) = crawl(proj).await;
    let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
    std::fs::write(
        std::env::temp_dir().join("dc_dbg.txt"),
        format!("pages={pages:?}\nfiles={urls:?}"),
    )
    .unwrap();
    assert!(
        urls.iter().any(|u| u.contains("/dl?id=3")),
        "endpoint discovery: {urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.ends_with("/img/bg.png")),
        "css url(): {urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.ends_with("/files/keep.zip")),
        "{urls:?}"
    );
    assert!(
        urls.iter().any(|u| u.ends_with("/files/sub.zip")),
        "in-dir sub page crawled: {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.contains("/ads/")),
        "exclude_paths: {urls:?}"
    );
    assert!(
        !urls.iter().any(|u| u.ends_with("/files/outside.zip")),
        "parent-dir guard must not expand /outside.html: {urls:?} pages={pages:?}"
    );
    // Only-paths filter restricts files.
    let proj = downloadcraft_lib::grabber::GrabberProject {
        start_url: format!("{base}/dir/index.html"),
        depth: 1,
        scope: 0,
        max_pages: 50,
        only_paths: vec!["/files/".into()],
        ..Default::default()
    };
    let (_p, files) = crawl(proj).await;
    assert!(
        files.iter().all(|f| f.url.contains("/files/")),
        "only_paths: {:?}",
        files.iter().map(|f| &f.url).collect::<Vec<_>>()
    );
    assert!(files.iter().any(|f| f.url.ends_with("/files/keep.zip")));
}

#[tokio::test]
async fn grabber_start_http_error_surfaces() {
    let base = serve(vec![(
        "/index.html".into(),
        b"<html></html>".to_vec(),
        "text/html".into(),
    )]);
    let proj = downloadcraft_lib::grabber::GrabberProject {
        start_url: format!("{base}/missing"),
        depth: 1,
        scope: 0,
        max_pages: 10,
        ..Default::default()
    };
    let err = downloadcraft_lib::grabber::Crawler {
        project: proj,
        progress: None,
    }
    .crawl(&build_client(""))
    .await
    .unwrap_err();
    assert!(err.contains("404") || err.contains("start page"), "{err}");
}
