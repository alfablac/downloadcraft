use idmcraft_lib::engine::{build_client, Controls};
use idmcraft_lib::model::*;
use idmcraft_lib::stream::download_hls;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

fn serve(routes: Vec<(String, Vec<u8>, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = vec![0u8; 16384];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").split('?').next().unwrap_or("").to_string();
            let range = req.lines().find(|l| l.to_ascii_lowercase().starts_with("range:")).map(|l| l.to_string());
            let mut found: Option<&(String, Vec<u8>, String)> = None;
            for r in &routes { if path == r.0 { found = Some(r); break; } }
            match found {
                Some((_, body, ct)) => {
                    if let Some(r) = &range {
                        if let Some(spec) = r.split("bytes=").nth(1) {
                            let mut it = spec.trim().split('-');
                            let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                            let end: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                            let end = end.min(body.len().saturating_sub(1));
                            if start < body.len() {
                                let slice = &body[start..=end];
                                let resp = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", slice.len(), body.len());
                                let _ = s.write_all(resp.as_bytes()); let _ = s.write_all(slice); continue;
                            }
                        }
                    }
                    let head_only = req.starts_with("HEAD");
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(resp.as_bytes()); if !head_only { let _ = s.write_all(body); }
                }
                None => { let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"); }
            }
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("idmcraft-hlsaes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn encrypt(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    let mut buf = vec![0u8; plain.len() + 16];
    buf[..plain.len()].copy_from_slice(plain);
    cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into()).encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len()).unwrap().to_vec()
}

#[tokio::test]
async fn hls_aes128_sequence_iv_decrypts() {
    let key: [u8; 16] = [0x10, 0x22, 0x35, 0x47, 0x59, 0x6b, 0x7d, 0x8f, 0xa1, 0xb3, 0xc5, 0xd7, 0xe9, 0xfb, 0x0d, 0x1f];
    let plain1 = b"first plaintext segment data".to_vec();
    let plain2 = b"second segment, different length!!".to_vec();
    // No IV attribute: IV = media_sequence (5) + segment index.
    let seq = 5u64;
    let enc1 = encrypt(&key, &idmcraft_lib::hls::iv_for_sequence(seq), &plain1);
    let enc2 = encrypt(&key, &idmcraft_lib::hls::iv_for_sequence(seq + 1), &plain2);
    let media = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{seq}\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\nseg1.ts\n#EXTINF:4,\nseg2.ts\n#EXT-X-ENDLIST\n");
    let base = serve(vec![
        ("/media.m3u8".into(), media.into_bytes(), "application/vnd.apple.mpegurl".into()),
        ("/key.bin".into(), key.to_vec(), "application/octet-stream".into()),
        ("/seg1.ts".into(), enc1, "video/mp2t".into()),
        ("/seg2.ts".into(), enc2, "video/mp2t".into()),
    ]);
    let dir = tmpdir("seqiv");
    let task = DownloadTask { id: "a1".into(), url: format!("{base}/media.m3u8"), filename: "out.ts".into(), dest_dir: dir.clone(), ..Default::default() };
    let done = download_hls(build_client(""), task, Arc::new(Controls::new(0))).await.unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    let mut want = plain1.clone(); want.extend_from_slice(&plain2);
    assert_eq!(std::fs::read(dir.join("out.ts")).unwrap(), want);
}

#[tokio::test]
async fn hls_aes128_explicit_iv_decrypts() {
    let key: [u8; 16] = [7u8; 16];
    let iv: [u8; 16] = [0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    let plain = b"explicit IV segment".to_vec();
    let enc = encrypt(&key, &iv, &plain);
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0xdeadbeef000102030405060708090a0b\n#EXTINF:4,\nseg.ts\n#EXT-X-ENDLIST\n";
    let base = serve(vec![
        ("/media.m3u8".into(), media.as_bytes().to_vec(), "application/vnd.apple.mpegurl".into()),
        ("/k.bin".into(), key.to_vec(), "application/octet-stream".into()),
        ("/seg.ts".into(), enc, "video/mp2t".into()),
    ]);
    let dir = tmpdir("expiv");
    let task = DownloadTask { id: "a2".into(), url: format!("{base}/media.m3u8"), filename: "out.ts".into(), dest_dir: dir.clone(), ..Default::default() };
    let done = download_hls(build_client(""), task, Arc::new(Controls::new(0))).await.unwrap();
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    assert_eq!(std::fs::read(dir.join("out.ts")).unwrap(), plain);
}

#[tokio::test]
async fn hls_sample_aes_refused_protected() {
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\",IV=0x00\n#EXTINF:4,\nseg.ts\n#EXT-X-ENDLIST\n";
    let base = serve(vec![
        ("/media.m3u8".into(), media.as_bytes().to_vec(), "application/vnd.apple.mpegurl".into()),
        ("/seg.ts".into(), b"whatever".to_vec(), "video/mp2t".into()),
    ]);
    let dir = tmpdir("sampleaes");
    let task = DownloadTask { id: "a3".into(), url: format!("{base}/media.m3u8"), filename: "out.ts".into(), dest_dir: dir.clone(), ..Default::default() };
    let done = download_hls(build_client(""), task, Arc::new(Controls::new(0))).await.unwrap();
    assert_eq!(done.status, TaskStatus::Protected);
    assert!(!dir.join("out.ts").exists());
}
