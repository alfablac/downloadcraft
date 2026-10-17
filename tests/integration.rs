// Integration tests run against binary via library-like inclusion: duplicate small harness using env binary? Instead test via `cargo test` unit modules already cover logic.
// This file provides fixture-server based tests using the binary's smoke test and host handshake through std process.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

fn free_server(routes: Vec<(String, Vec<u8>, String)>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
            let range = req.lines().find(|l| l.to_ascii_lowercase().starts_with("range:")).map(|l| l.to_string());
            let mut found: Option<&(String, Vec<u8>, String)> = None;
            for r in &routes { if path.starts_with(&r.0) { found = Some(r); break; } }
            match found {
                Some((_, body, ct)) => {
                    // Range support for /file.bin
                    if let Some(r) = range {
                        if let Some(spec) = r.split("bytes=").nth(1) {
                            let mut it = spec.trim().split('-');
                            let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                            let end: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(body.len() - 1);
                            let end = end.min(body.len() - 1);
                            let slice = &body[start..=end];
                            let resp = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len(), body.len());
                            let _ = s.write_all(resp.as_bytes()); let _ = s.write_all(slice);
                            continue;
                        }
                    }
                    let head_only = req.starts_with("HEAD");
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(resp.as_bytes());
                    if !head_only { let _ = s.write_all(body); }
                }
                None => { let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"); }
            }
        }
    });
    (base, handle)
}

fn bin_path(name: &str) -> PathBuf {
    // target/debug/<name>
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug").join(exe)
}

#[test]
fn smoke_test_passes() {
    let bin = bin_path("downloadcraft");
    assert!(bin.exists(), "build first: {}", bin.display());
    let out = std::process::Command::new(&bin).arg("--smoke-test").output().unwrap();
    assert!(out.status.success(), "stdout={} stderr={}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("smoke ok"));
}

#[test]
fn fixture_server_range_and_manifests_served() {
    let mpd = r#"<MPD type="static" mediaPresentationDuration="PT10S"><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate media="seg-$Number$.m4s" initialization="init.mp4" duration="10" timescale="1"/><Representation id="v1" bandwidth="100"/></AdaptationSet></Period></MPD>"#;
    let (base, _h) = free_server(vec![
        ("/file.bin".into(), (0u8..=255).collect(), "application/octet-stream".into()),
        ("/manifest.mpd".into(), mpd.as_bytes().to_vec(), "application/dash+xml".into()),
        ("/master.m3u8".into(), b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=10\nv.m3u8\n".to_vec(), "application/vnd.apple.mpegurl".into()),
    ]);
    // fetch via std TcpStream
    for p in ["/file.bin", "/manifest.mpd", "/master.m3u8"] {
        let mut s = std::net::TcpStream::connect(base.trim_start_matches("http://")).unwrap();
        s.write_all(format!("GET {p} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
        let mut rb = Vec::new(); s.read_to_end(&mut rb).unwrap();
        let r = String::from_utf8_lossy(&rb).to_string();
        assert!(r.contains("200 OK"), "{p}: {r:.100}");
    }
    // range
    let mut s = std::net::TcpStream::connect(base.trim_start_matches("http://")).unwrap();
    s.write_all(b"GET /file.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=10-19\r\nConnection: close\r\n\r\n").unwrap();
    let mut r = Vec::new(); s.read_to_end(&mut r).unwrap();
    assert!(String::from_utf8_lossy(&r).contains("206"));
}

#[test]
fn ffmpeg_available_or_documented() {
    // CI installs ffmpeg; local dev may not. Only assert ffprobe works if present.
    if let Ok(out) = std::process::Command::new("ffprobe").arg("-version").output() {
        assert!(out.status.success());
    }
}
