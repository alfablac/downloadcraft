// Fake in-process NNTP server (TcpListener): greeting, AUTHINFO, GROUP,
// BODY with dot-stuffed yEnc articles. Drives download_nzb end-to-end.
use downloadcraft_lib::engine::{build_client, Controls};
use downloadcraft_lib::model::*;
use downloadcraft_lib::nntp::crc32;
use downloadcraft_lib::stream::download_url_with_settings;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Minimal yEnc encoder (single line, escapes critical chars).
fn yenc(data: &[u8], name: &str) -> Vec<u8> {
    let mut out = format!("=ybegin size={} name={name}\r\n", data.len()).into_bytes();
    for &b in data {
        let c = b.wrapping_add(42);
        if matches!(c, 0 | 9 | 10 | 13 | 61) {
            out.push(b'=');
            out.push(c.wrapping_add(64));
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(
        format!(
            "\r\n=yend size={} pcrc32={:08x}\r\n",
            data.len(),
            crc32(data)
        )
        .as_bytes(),
    );
    out
}

struct Fake {
    cmds: Arc<Mutex<Vec<String>>>,
    port: u16,
}

fn fake_nntp(articles: Vec<(String, Vec<u8>)>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cmds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cmds2 = cmds.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let articles = articles.clone();
            let cmds = cmds2.clone();
            std::thread::spawn(move || serve_conn(&mut s, &articles, &cmds));
        }
    });
    Fake { cmds, port }
}

fn read_cmd(s: &mut TcpStream) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match s.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Some(String::from_utf8_lossy(&buf).trim_end().to_string());
                }
                buf.push(byte[0]);
            }
        }
    }
}

fn serve_conn(s: &mut TcpStream, articles: &[(String, Vec<u8>)], cmds: &Arc<Mutex<Vec<String>>>) {
    let _ = s.write_all(b"200 fake NNTP ready\r\n");
    while let Some(line) = read_cmd(s) {
        cmds.lock().unwrap().push(line.clone());
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("AUTHINFO USER") {
            let _ = s.write_all(b"381 password required\r\n");
        } else if upper.starts_with("AUTHINFO PASS") {
            let _ = s.write_all(b"281 welcome\r\n");
        } else if upper.starts_with("GROUP ") {
            let _ = s.write_all(b"211 3 1 3 alt.binaries.test\r\n");
        } else if upper.starts_with("BODY ") {
            let id = line[5..].trim().trim_matches(|c| c == '<' || c == '>');
            match articles.iter().find(|(k, _)| k == id) {
                Some((_, body)) => {
                    let _ = s.write_all(format!("222 0 <{id}> article retrieved\r\n").as_bytes());
                    // Dot-stuff lines starting with '.' per NNTP.
                    for l in body.split(|&b| b == b'\n') {
                        if l.starts_with(b".") {
                            let _ = s.write_all(b".");
                        }
                        let _ = s.write_all(l);
                        let _ = s.write_all(b"\n");
                    }
                    let _ = s.write_all(b".\r\n");
                }
                None => {
                    let _ = s.write_all(b"430 no such article\r\n");
                }
            }
        } else if upper.starts_with("QUIT") {
            let _ = s.write_all(b"205 bye\r\n");
            return;
        } else {
            let _ = s.write_all(b"500 unknown\r\n");
        }
    }
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("downloadcraft-nntp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

type SegSpec = (&'static str, u64, u32);

/// (subject, [(id, bytes, number)]) — explicit NZB numbers so tests can list
/// segments out of order.
fn nzb_xml(files: &[(&str, Vec<SegSpec>)]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    for (subject, segs) in files {
        out.push_str(&format!("<file poster=\"p@x\" date=\"1700000000\" subject=\"{subject}\"><groups><group>alt.binaries.test</group></groups><segments>"));
        for (id, bytes, number) in segs {
            out.push_str(&format!(
                "<segment bytes=\"{bytes}\" number=\"{number}\">{id}</segment>"
            ));
        }
        out.push_str("</segments></file>\n");
    }
    out.push_str("</nzb>");
    out
}

fn settings_for(fake: &Fake) -> Settings {
    Settings {
        nntp_host: "127.0.0.1".into(),
        nntp_port: fake.port,
        nntp_tls: false,
        nntp_user: "alice".into(),
        nntp_pass: "secret".into(),
        ..Default::default()
    }
}

async fn run_nzb(
    dir: &std::path::Path,
    nzb_path: &std::path::Path,
    settings: &Settings,
) -> (DownloadTask, Arc<Controls>) {
    let task = DownloadTask {
        id: "n1".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.to_path_buf(),
        ..Default::default()
    };
    let ctrl = Arc::new(Controls::new(0));
    let done = download_url_with_settings(build_client(""), task, ctrl.clone(), settings)
        .await
        .unwrap();
    (done, ctrl)
}

#[tokio::test]
async fn nntp_downloads_nzb_files_via_fake_server() {
    // Data spans all byte values (yEnc escapes + high bytes included).
    let data1: Vec<u8> = (0u32..600).map(|i| (i % 256) as u8).collect();
    let data2: Vec<u8> = (0u32..300).map(|i| (255 - (i % 256)) as u8).collect();
    let art = |d: &[u8], n: &str| yenc(d, n);
    let fake = fake_nntp(vec![
        ("s1@news".into(), art(&data1[..300], "file1.bin")),
        ("s2@news".into(), art(&data1[300..], "file1.bin")),
        ("s3@news".into(), art(&data2, "file2.bin")),
    ]);
    // nzb_xml emits subjects raw into XML attrs, so quotes arrive pre-escaped.
    let nzb = nzb_xml(&[
        (
            "poster &quot;file1.bin&quot; yEnc (1/2)",
            vec![("s2@news", 320, 2), ("s1@news", 320, 1)],
        ),
        (
            "poster &quot;file2.bin&quot; yEnc (1/1)",
            vec![("s3@news", 320, 1)],
        ),
    ]);
    // Segments listed out of order in file1: NZB number order must win (s1,s2).
    let dir = tmpdir("ok");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let settings = settings_for(&fake);
    let (done, ctrl) = run_nzb(&dir, &nzb_path, &settings).await;
    assert_eq!(done.status, TaskStatus::Complete, "{}", done.error);
    assert_eq!(std::fs::read(dir.join("file1.bin")).unwrap(), data1);
    assert_eq!(std::fs::read(dir.join("file2.bin")).unwrap(), data2);
    assert_eq!(done.downloaded, (data1.len() + data2.len()) as u64);
    assert_eq!(done.total, Some((data1.len() + data2.len()) as u64));
    assert_eq!(
        ctrl.downloaded.load(std::sync::atomic::Ordering::Relaxed),
        (data1.len() + data2.len()) as u64
    );
    assert!(ctrl
        .parts_are_segments
        .load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(ctrl.parts.lock().unwrap().clone(), vec![(3, 3)]);
    let cmds = fake.cmds.lock().unwrap().clone();
    assert!(cmds.iter().any(|c| c == "AUTHINFO USER alice"), "{cmds:?}");
    assert!(cmds.iter().any(|c| c == "AUTHINFO PASS secret"), "{cmds:?}");
    assert!(
        cmds.iter().any(|c| c == "GROUP alt.binaries.test"),
        "{cmds:?}"
    );
    assert!(cmds.iter().any(|c| c == "BODY <s1@news>"), "{cmds:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_missing_article_430_errors() {
    let fake = fake_nntp(vec![]);
    let nzb = nzb_xml(&[("poster &quot;gone.bin&quot;", vec![("nope@news", 100, 1)])]);
    let dir = tmpdir("430");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, &nzb).unwrap();
    let settings = settings_for(&fake);
    let task = DownloadTask {
        id: "n2".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url_with_settings(
        build_client(""),
        task,
        Arc::new(Controls::new(0)),
        &settings,
    )
    .await;
    assert!(r.is_err(), "430 must error, got {r:?}");
    assert!(r.unwrap_err().contains("430"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nntp_unconfigured_host_errors() {
    let dir = tmpdir("cfg");
    let nzb_path = dir.join("test.nzb");
    std::fs::write(
        &nzb_path,
        nzb_xml(&[("p &quot;f.bin&quot;", vec![("a@b", 1, 1)])]),
    )
    .unwrap();
    let task = DownloadTask {
        id: "n3".into(),
        url: nzb_path.to_string_lossy().into_owned(),
        filename: "x.nzb".into(),
        dest_dir: dir.clone(),
        ..Default::default()
    };
    let r = download_url_with_settings(
        build_client(""),
        task,
        Arc::new(Controls::new(0)),
        &Settings::default(),
    )
    .await;
    assert!(r.is_err() && r.unwrap_err().contains("not configured"));
    let _ = std::fs::remove_dir_all(&dir);
}
