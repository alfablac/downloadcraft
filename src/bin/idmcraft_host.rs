use std::io::{Read, Write};
use std::path::PathBuf;

fn inbox_dir() -> PathBuf {
    // Must match main app config_dir without depending on it (host is standalone small binary).
    let base = directories::ProjectDirs::from("", "", "idmcraft").map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".idmcraft"));
    base.join("inbox")
}

fn handle(msg: &serde_json::Value) -> serde_json::Value {
    let ty = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "ping" => serde_json::json!({"type":"pong"}),
        "download" | "add" => {
            let url = msg.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() { return serde_json::json!({"type":"error","error":"missing url"}); }
            let dir = inbox_dir();
            let _ = std::fs::create_dir_all(&dir);
            let name = format!("msg-{}.txt", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
            let mut body = format!("url={url}\n");
            for k in ["filename", "referrer", "cookies", "referer", "cookie", "queue"] {
                if let Some(v) = msg.get(k).and_then(|v| v.as_str()) { body.push_str(&format!("{k}={v}\n")); }
            }
            // headers object flattened as header lines are ignored by parser except known keys; store referrer/cookies above
            match std::fs::write(dir.join(name), body) {
                Ok(()) => serde_json::json!({"type":"ok"}),
                Err(e) => serde_json::json!({"type":"error","error":e.to_string()}),
            }
        }
        _ => serde_json::json!({"type":"error","error":"unknown type"}),
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
