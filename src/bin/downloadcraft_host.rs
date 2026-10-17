use std::io::{Read, Write};
use std::path::PathBuf;

fn inbox_dir() -> PathBuf {
    // Must match main app config_dir without depending on it (host is standalone small binary).
    let base = directories::ProjectDirs::from("", "", "downloadcraft").map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".downloadcraft"));
    base.join("inbox")
}

/// Basename only: strip `/` and `\` components, reject empty / "." / "..".
fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().filter(|s| !s.is_empty() && *s != "." && *s != "..")?;
    Some(base.to_string())
}

fn handle(msg: &serde_json::Value) -> serde_json::Value {
    let ty = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Echo request `id` verbatim when present; harmless for old clients that ignore it.
    let with_id = |mut resp: serde_json::Value| {
        if let Some(id) = msg.get("id") {
            if let Some(obj) = resp.as_object_mut() { obj.insert("id".into(), id.clone()); }
        }
        resp
    };
    match ty {
        "ping" => with_id(serde_json::json!({"type":"pong"})),
        "download" | "add" => {
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() { return with_id(serde_json::json!({"type":"error","error":"missing url"})); }
            let dir = inbox_dir();
            let _ = std::fs::create_dir_all(&dir);
            let name = format!("msg-{}.txt", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
            let mut body = format!("url={url}\n");
            for k in ["filename", "referrer", "cookies", "referer", "cookie", "queue", "mime", "type"] {
                if let Some(v) = msg.get(k).and_then(|v| v.as_str()) {
                    if k == "filename" {
                        // Drop traversal: write basename only; drop if basename invalid/empty.
                        if let Some(b) = sanitize_filename(v) { body.push_str(&format!("filename={b}\n")); }
                    } else if k == "mime" {
                        // Desktop parser ignores unknown keys; kept for forward-compat.
                        if !v.contains(['\n', '\r']) { body.push_str(&format!("mime={v}\n")); }
                    } else if k != "type" { body.push_str(&format!("{k}={v}\n")); }
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
        if inp.read_exact(&mut lenbuf).is_err() { break; }
        let n = u32::from_le_bytes(lenbuf) as usize;
        if n > 10_000_000 { break; }
        let mut buf = vec![0u8; n];
        if inp.read_exact(&mut buf).is_err() { break; }
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
}