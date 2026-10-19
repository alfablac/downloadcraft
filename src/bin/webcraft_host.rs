#![cfg_attr(windows, windows_subsystem = "windows")]
use std::io::{Read, Write};
use std::path::PathBuf;

fn config_dir() -> PathBuf {
    // Must match main app config_dir without depending on it (host is standalone small binary).
    directories::ProjectDirs::from("", "", "webcraft")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".webcraft"))
}

fn inbox_dir() -> PathBuf {
    config_dir().join("inbox")
}

fn ext_of(filename: &str, url: &str) -> String {
    let seg = filename
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            url.split(['?', '#'])
                .next()
                .unwrap_or(url)
                .rsplit('/')
                .next()
                .unwrap_or("")
        });
    match seg.rfind('.') {
        Some(i) if i > 0 => seg[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// Capture filter from `capture-filter.json` in config dir, written by desktop
/// whenever capture settings change: {"exts":["mp4","zip"],"minBytes":123}.
/// Missing/corrupt file = no filter (desktop never ran / settings default).
/// Unknown size (0/missing) always allowed: cannot prove small.
/// Empty ext list = all types. Explicit sends (captured=false) never reach here.
pub fn capture_allowed(
    exts: &[String],
    min_bytes: u64,
    filename: &str,
    url: &str,
    mime: &str,
    size: u64,
) -> bool {
    if min_bytes > 0 && size > 0 && size < min_bytes {
        return false;
    }
    if exts.is_empty() {
        return true;
    }
    let ext = ext_of(filename, url);
    if !ext.is_empty() {
        return exts.iter().any(|e| e == &ext);
    }
    // No extension in filename/URL: fall back to MIME subtype ("video/mp4" -> "mp4").
    let sub = mime
        .split(['/', ';'])
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    !sub.is_empty() && exts.iter().any(|e| e == &sub)
}

fn load_capture_filter() -> (Vec<String>, u64) {
    // Legacy fallback: pre-rename desktop wrote filter under downloadcraft dir.
    let new_path = config_dir().join("capture-filter.json");
    let legacy_path = directories::ProjectDirs::from("", "", "downloadcraft")
        .map(|d| d.config_dir().join("capture-filter.json"))
        .unwrap_or_else(|| PathBuf::from(".downloadcraft/capture-filter.json"));
    let path = if new_path.exists() {
        new_path
    } else {
        legacy_path
    };
    let Ok(txt) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return (Vec::new(), 0);
    };
    let exts = v
        .get("exts")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    e.as_str()
                        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let min = v.get("minBytes").and_then(|m| m.as_u64()).unwrap_or(0);
    (exts, min)
}

/// Basename only: strip `/` and `\` components, reject empty / "." / "..".
fn sanitize_filename(name: &str) -> Option<String> {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")?;
    Some(base.to_string())
}

fn handle(msg: &serde_json::Value) -> serde_json::Value {
    let ty = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Echo request `id` verbatim when present; harmless for old clients that ignore it.
    let with_id = |mut resp: serde_json::Value| {
        if let Some(id) = msg.get("id") {
            if let Some(obj) = resp.as_object_mut() {
                obj.insert("id".into(), id.clone());
            }
        }
        resp
    };
    match ty {
        "ping" => with_id(serde_json::json!({"type":"pong"})),
        // Pre-flight for captured downloads: extension asks BEFORE cancelling
        // browser download. allow=false -> browser keeps file (filter is
        // webcraft Settings via capture-filter.json, host is sole decider).
        // No inbox write here; actual send is separate "download" msg.
        "captureCheck" => {
            let (exts, min_bytes) = load_capture_filter();
            let filename = msg.get("filename").and_then(|f| f.as_str()).unwrap_or("");
            let url = msg.get("url").and_then(|f| f.as_str()).unwrap_or("");
            let mime = msg.get("mime").and_then(|f| f.as_str()).unwrap_or("");
            let size = msg.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            let allow = capture_allowed(&exts, min_bytes, filename, url, mime, size);
            with_id(serde_json::json!({"type":"captureCheck","allow":allow}))
        }
        "download" | "add" => {
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                return with_id(serde_json::json!({"type":"error","error":"missing url"}));
            }
            // Captured downloads pass capture filter (webcraft Settings).
            // Explicit sends omit `captured` and always go: user intent.
            // Extension pre-checks via captureCheck before cancelling, so
            // filtered normally never reaches here; skipped reply = raced
            // filter change, extension resumes browser download on it.
            if msg
                .get("captured")
                .and_then(|c| c.as_bool())
                .unwrap_or(false)
            {
                let (exts, min_bytes) = load_capture_filter();
                let filename = msg.get("filename").and_then(|f| f.as_str()).unwrap_or("");
                let mime = msg.get("mime").and_then(|f| f.as_str()).unwrap_or("");
                let size = msg.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                if !capture_allowed(&exts, min_bytes, filename, url, mime, size) {
                    return with_id(serde_json::json!({"type":"ok","skipped":"filtered"}));
                }
            }
            let dir = inbox_dir();
            let _ = std::fs::create_dir_all(&dir);
            let name = format!(
                "msg-{}.txt",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let mut body = format!("url={url}\n");
            // size arrives as JSON number: write explicitly or KV drops it.
            if let Some(sz) = msg.get("size").and_then(|v| v.as_u64()) {
                if sz > 0 {
                    body.push_str(&format!("size={sz}\n"));
                }
            }
            for k in [
                "filename", "referrer", "cookies", "referer", "cookie", "queue", "mime", "type",
                "pageUrl", "intent", "size",
            ] {
                if k == "size" {
                    continue;
                }
                if let Some(v) = msg.get(k).and_then(|v| v.as_str()) {
                    if k == "filename" {
                        // Drop traversal: write basename only; drop if basename invalid/empty.
                        if let Some(b) = sanitize_filename(v) {
                            body.push_str(&format!("filename={b}\n"));
                        }
                    } else if k == "mime" {
                        // Desktop parser ignores unknown keys; kept for forward-compat.
                        if !v.contains(['\n', '\r']) {
                            body.push_str(&format!("mime={v}\n"));
                        }
                    } else if k != "type" {
                        body.push_str(&format!("{k}={v}\n"));
                    }
                }
            }
            // headers object flattened as header lines are ignored by parser except known keys; store referrer/cookies above
            match std::fs::write(dir.join(name), body) {
                Ok(()) => with_id(serde_json::json!({"type":"ok"})),
                Err(e) => with_id(serde_json::json!({"type":"error","error":e.to_string()})),
            }
        }
        _ => with_id(serde_json::json!({"type":"error","error":"unknown type"})),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut inp = stdin.lock();
    let mut out = stdout.lock();
    loop {
        let mut lenbuf = [0u8; 4];
        if inp.read_exact(&mut lenbuf).is_err() {
            break;
        }
        let n = u32::from_le_bytes(lenbuf) as usize;
        if n > 10_000_000 {
            break;
        }
        let mut buf = vec![0u8; n];
        if inp.read_exact(&mut buf).is_err() {
            break;
        }
        let msg: serde_json::Value = serde_json::from_slice(&buf).unwrap_or(serde_json::json!({}));
        let resp = handle(&msg);
        let body = resp.to_string();
        let _ = out.write_all(&(body.len() as u32).to_le_bytes());
        let _ = out.write_all(body.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handle_echoes_id_and_stays_backward_compatible() {
        let r = handle(&serde_json::json!({"type":"ping","id":7}));
        assert_eq!(r["type"], "pong");
        assert_eq!(r["id"], 7);
        let r = handle(&serde_json::json!({"type":"ping"}));
        assert!(r.get("id").is_none());
        let r = handle(&serde_json::json!({"type":"download","id":"a1"}));
        assert_eq!(r["type"], "error");
        assert_eq!(r["id"], "a1");
    }

    fn exts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn capture_check_msg_answers_allow() {
        // Defaults = empty exts + min 0: always allow. Pure via capture_allowed
        // (handle() would read real user capture-filter.json — non-hermetic).
        assert!(capture_allowed(&[], 0, "f.bin", "https://x/f.bin", "", 10));
        assert!(!capture_allowed(&exts(&["zip"]), 0, "f.bin", "https://x/f.bin", "", 10));
        assert!(!capture_allowed(&[], 100, "f.zip", "https://x/f.zip", "", 10));
    }

    #[test]
    fn capture_empty_exts_allows_all() {
        assert!(capture_allowed(
            &[],
            0,
            "a.bin",
            "https://x/a.bin",
            "application/octet-stream",
            5
        ));
        assert!(capture_allowed(&[], 0, "", "https://x/noext", "", 0));
    }

    #[test]
    fn capture_allowlist_matches_filename_or_url_ext_case_insensitively() {
        let e = exts(&["mp4", "zip"]);
        assert!(capture_allowed(
            &e,
            0,
            "Movie.MP4",
            "https://x/movie",
            "video/mp4",
            10
        ));
        assert!(capture_allowed(
            &e,
            0,
            "",
            "https://x/file.ZIP?token=1",
            "",
            10
        ));
        assert!(!capture_allowed(
            &e,
            0,
            "notes.pdf",
            "https://x/notes.pdf",
            "application/pdf",
            10
        ));
    }

    #[test]
    fn capture_allowlist_falls_back_to_mime_subtype_when_no_ext() {
        let e = exts(&["mp4"]);
        assert!(capture_allowed(
            &e,
            0,
            "download",
            "https://x/download",
            "video/mp4",
            10
        ));
        assert!(!capture_allowed(
            &e,
            0,
            "download",
            "https://x/download",
            "application/pdf",
            10
        ));
        assert!(!capture_allowed(
            &e,
            0,
            "download",
            "https://x/download",
            "",
            10
        ));
    }

    #[test]
    fn capture_min_size_skips_only_known_small() {
        assert!(!capture_allowed(
            &[],
            10 * 1024 * 1024,
            "a.zip",
            "https://x/a.zip",
            "",
            5 * 1024 * 1024
        ));
        assert!(capture_allowed(
            &[],
            10 * 1024 * 1024,
            "a.zip",
            "https://x/a.zip",
            "",
            10 * 1024 * 1024
        ));
        assert!(capture_allowed(
            &[],
            10 * 1024 * 1024,
            "a.zip",
            "https://x/a.zip",
            "",
            0
        )); // unknown size: cannot prove small
        assert!(capture_allowed(&[], 0, "a.zip", "https://x/a.zip", "", 1)); // 0 = no minimum
    }
}
