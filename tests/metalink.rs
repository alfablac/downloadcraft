use downloadcraft_lib::engine::{build_client, Controls};
use downloadcraft_lib::model::*;
use downloadcraft_lib::stream::download_url;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// Tiny Range-capable server: routes = (path, body, content-type).
fn serve(routes: Vec<(String, Vec<u8>, String)>) -> String {
    serve_on(0, routes)
}

fn serve_on(port: u16, routes: Vec<(String, Vec<u8>, String)>) -> String {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let routes = routes.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let range = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .map(|l| l.to_string());
                let Some((_, body, ct)) = routes.iter().find(|r| r.0 == path) else {
                    let _ = s.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                };
                if let Some(r) = &range {
                    if let Some(spec) = r.split("bytes=").nth(1) {
                        let mut it = spec.trim().split('-');
                        let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                        let end: usize =
                            it.next().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                        let end = end.min(body.len().saturating_sub(1));
                        if start < body.len() {
                            let slice = &body[start..=end];
                            let resp = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n", slice.len(), body.len());
                            let _ = s.write_all(resp.as_bytes());
                            let _ = s.write_all(slice);
                            return;
                        }
                    }
                }
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(resp.as_bytes());
                let _ = s.write_all(body);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "downloadcraft-metalink-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn metalink_downloads_first_file_with_mirror_and_hash() {
    let body: Vec<u8> = (0..60_000u32).map(|i| (i % 251) as u8).collect();
    let hash = format!("{:x}", Sha256::digest(&body));
    // XML embeds absolute mirror URLs, so reserve a port first to learn it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let xml = format!(
        r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="file.bin"><size>{}</size><hash type="sha-256">{}</hash><url>{base}/file.bin</url><url>{base}/mirror.bin</url></file></metalink>"#,
        body.len(),
        hash
    );
    let served = serve_on(
        port,
        vec![
            (
                "/link.meta4".into(),
                xml.into_bytes(),
                "application/metalink+xml".into(),
            ),
            (
                "/file.bin".into(),
                body.clone(),
                "application/octet-stream".into(),
            ),
            (
                "/mirror.bin".into(),
                body.clone(),
                "application/octet-stream".into(),
            ),
        ],
    );
    assert_eq!(served, base);
    let dir = tmpdir("first");
    let task = DownloadTask {
        id: "ml1".into(),
        url: format!("{base}/link.meta4"),
        filename: "link.meta4".into(),
        dest_dir: dir.clone(),
        segments: 4,
        ..Default::default()
    };
    let done = download_url(build_client(""), task, Arc::new(Controls::new(0)))
        .await
        .unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    assert_eq!(done.filename, "file.bin");
    assert_eq!(done.url, format!("{base}/file.bin"));
    assert_eq!(done.mirrors, vec![format!("{base}/mirror.bin")]);
    assert_eq!(done.sha256.as_deref(), Some(hash.as_str()));
    assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), body);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn metalink_bad_fetch_errors_not_silent() {
    let base = serve(vec![]);
    let dir = tmpdir("missing");
    let task = DownloadTask {
        id: "ml2".into(),
        url: format!("{base}/nope.meta4"),
        filename: "nope.meta4".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url(build_client(""), task, Arc::new(Controls::new(0))).await;
    assert!(r.is_err(), "missing .meta4 must error, got {r:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
