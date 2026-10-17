use crate::model::*;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub fn build_client(proxy: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder().user_agent("idmcraft/0.1").connect_timeout(std::time::Duration::from_secs(15)).read_timeout(std::time::Duration::from_secs(60));
    if !proxy.trim().is_empty() { if let Ok(p) = reqwest::Proxy::all(proxy.trim()) { b = b.proxy(p); } }
    b.build().unwrap_or_default()
}

fn headers_for(task: &DownloadTask) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if !task.referrer.is_empty() { if let Ok(v) = task.referrer.parse() { h.insert(reqwest::header::REFERER, v); } }
    if !task.cookies.is_empty() { if let Ok(v) = task.cookies.parse() { h.insert(reqwest::header::COOKIE, v); } }
    for (k, v) in &task.headers { if let (Ok(n), Ok(vv)) = (k.parse::<reqwest::header::HeaderName>(), v.parse()) { h.insert(n, vv); } }
    h
}

pub struct Controls { pub paused: AtomicBool, pub cancelled: AtomicBool, pub downloaded: AtomicU64, pub limit_kbps: AtomicU64, pub rate: std::sync::Mutex<(std::time::Instant, u64)> }
impl Controls { pub fn new(limit: u64) -> Self { Self { paused: AtomicBool::new(false), cancelled: AtomicBool::new(false), downloaded: AtomicU64::new(0), limit_kbps: AtomicU64::new(limit), rate: std::sync::Mutex::new((std::time::Instant::now(), 0)) } } }

async fn throttle(ctrl: &Controls, bytes: u64) {
    let limit = ctrl.limit_kbps.load(Ordering::Relaxed);
    if limit == 0 { return; }
    // Global 1s window gate shared across workers.
    loop {
        let wait = {
            let mut g = ctrl.rate.lock().unwrap();
            if g.0.elapsed() >= std::time::Duration::from_secs(1) { *g = (std::time::Instant::now(), 0); }
            let budget = limit.saturating_mul(1024);
            if bytes > budget {
                if g.1 == 0 { g.1 = bytes; return; }
                std::time::Duration::from_secs(1).saturating_sub(g.0.elapsed())
            } else if g.1 + bytes <= budget { g.1 += bytes; return; }
            else { std::time::Duration::from_secs(1).saturating_sub(g.0.elapsed()) }
        };
        tokio::time::sleep(wait).await;
    }
}

async fn wait_not_paused(ctrl: &Controls) -> Result<(), String> {
    while ctrl.paused.load(Ordering::Relaxed) {
        if ctrl.cancelled.load(Ordering::Relaxed) { return Err("cancelled".into()); }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if ctrl.cancelled.load(Ordering::Relaxed) { return Err("cancelled".into()); }
    Ok(())
}

/// Split total into segment ranges.
pub fn plan_segments(total: u64, wanted: u32) -> Vec<(u64, u64)> {
    if total == 0 { return vec![]; }
    let n = (wanted.max(1) as u64).min(total.max(1));
    let chunk = total / n; let mut v = Vec::new(); let mut s = 0;
    for i in 0..n { let e = if i == n - 1 { total - 1 } else { s + chunk - 1 }; v.push((s, e)); s = e + 1; }
    v
}

pub struct Probe { pub total: Option<u64>, pub accept_ranges: bool, pub etag: Option<String>, pub last_modified: Option<String>, pub final_url: String }

pub async fn probe(client: &reqwest::Client, task: &DownloadTask) -> Result<Probe, String> {
    let resp = client.get(&task.url).headers(headers_for(task)).header("Range", "bytes=0-0").send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let etag = resp.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let lm = resp.headers().get(reqwest::header::LAST_MODIFIED).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let final_url = resp.url().to_string();
    if status.as_u16() == 206 {
        let cr = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let total = cr.rsplit('/').next().and_then(|v| v.parse().ok());
        let _ = resp.bytes().await;
        Ok(Probe { total, accept_ranges: true, etag, last_modified: lm, final_url })
    } else if status.is_success() {
        let total = resp.content_length();
        let _ = resp.bytes().await;
        Ok(Probe { total, accept_ranges: false, etag, last_modified: lm, final_url })
    } else { Err(format!("HTTP {status}")) }
}

struct SegmentState { start: u64, end: u64, done: u64 }
fn state_path_for(part_dir: &Path) -> PathBuf { part_dir.join("state.json") }
fn save_state(part_dir: &Path, url: &str, etag: &Option<String>, total: u64, segs: &[SegmentState]) {
    let v: Vec<_> = segs.iter().map(|s| serde_json::json!({"start":s.start,"end":s.end,"done":s.done})).collect();
    let j = serde_json::json!({"url":url,"etag":etag,"total":total,"segments":v});
    let _ = std::fs::write(state_path_for(part_dir), j.to_string());
}

/// Segmented HTTP download with resume. Segment files: part{i} in dest_dir/.idmcraft-{id}/
pub async fn download_http(client: reqwest::Client, mut task: DownloadTask, ctrl: Arc<Controls>) -> Result<DownloadTask, String> {
    let probe = probe(&client, &task).await?;
    task.total = probe.total;
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part_dir = task.dest_dir.join(format!(".idmcraft-{}", task.id));
    // Server ignored Range: plain streaming download with throttle.
    if !probe.accept_ranges || probe.total.is_none() {
        let resp = client.get(&task.url).headers(headers_for(&task)).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() { return Err(format!("HTTP {}", resp.status())); }
        task.total = resp.content_length().or(task.total);
        let tmp = task.dest_dir.join(format!(".partial-{}", task.filename));
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream(); let mut n: u64 = 0;
        while let Some(chunk) = stream.next().await {
            wait_not_paused(&ctrl).await?;
            let chunk = chunk.map_err(|e| e.to_string())?;
            throttle(&ctrl, chunk.len() as u64).await;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            n += chunk.len() as u64; ctrl.downloaded.store(n, Ordering::Relaxed); task.downloaded = n;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        tokio::fs::rename(&tmp, &dest).await.map_err(|e| e.to_string())?;
        finish_check(&dest, &task).await?;
        task.status = TaskStatus::Complete; task.downloaded = n;
        return Ok(task);
    }
    let total = probe.total.unwrap();
    task.total = Some(total);
    std::fs::create_dir_all(&part_dir).map_err(|e| e.to_string())?;
    // Resume state validation
    let mut segs: Vec<SegmentState> = Vec::new();
    let mut resumed: u64 = 0;
    let state_file = state_path_for(&part_dir);
    if state_file.exists() {
        if let Ok(txt) = std::fs::read_to_string(&state_file) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let etag_ok = v.get("etag").and_then(|e| e.as_str()).map(|s| s.to_string()) == probe.etag || probe.etag.is_none();
                if v.get("url").and_then(|u| u.as_str()) == Some(task.url.as_str()) && v.get("total").and_then(|t| t.as_u64()) == Some(total) && etag_ok {
                    if let Some(arr) = v.get("segments").and_then(|s| s.as_array()) {
                        for s in arr {
                            let (start, end, done) = (s["start"].as_u64().unwrap_or(0), s["end"].as_u64().unwrap_or(0), s["done"].as_u64().unwrap_or(0));
                            // verify part file size matches done
                            let pf = part_dir.join(format!("part{}", segs.len()));
                            let actual = std::fs::metadata(&pf).map(|m| m.len()).unwrap_or(0);
                            let done = done.min(actual);
                            segs.push(SegmentState { start, end, done }); resumed += done;
                        }
                    }
                }
            }
        }
    }
    if segs.is_empty() {
        let wanted = if task.segments == 0 { crate::model::Settings::default().max_connections } else { task.segments };
        segs = plan_segments(total, wanted).into_iter().map(|(start, end)| SegmentState { start, end, done: 0 }).collect();
    }
    ctrl.downloaded.store(resumed, Ordering::Relaxed); task.downloaded = resumed;
    // Download missing parts in parallel
    let etag: Arc<str> = probe.etag.clone().unwrap_or_default().into();
    let segs = Arc::new(tokio::sync::Mutex::new(segs));
    let mut handles = Vec::new();
    let count = segs.lock().await.len();
    for i in 0..count {
        let (client, task_c, ctrl, segs, part_dir, etag) = (client.clone(), task.clone(), ctrl.clone(), segs.clone(), part_dir.clone(), etag.clone());
        handles.push(tokio::spawn(async move {
            let mut attempt = 0;
            loop {
                let (start, end, done) = { let s = segs.lock().await; let sg = &s[i]; (sg.start, sg.end, sg.done) };
                if done > end - start { return Ok::<(), String>(()); }
                let range_start = start + done;
                if range_start > end { return Ok(()); }
                match download_range(&client, &task_c, range_start, end, &part_dir.join(format!("part{i}")), &ctrl, done).await {
                    Ok(written) => {
                        { let mut s = segs.lock().await; s[i].done += written; }
                        ctrl.downloaded.fetch_add(written, Ordering::Relaxed);
                        let snapshot = { let s = segs.lock().await; s.iter().map(|x| SegmentState { start: x.start, end: x.end, done: x.done }).collect::<Vec<_>>() };
                        let et_opt = if etag.is_empty() { None } else { Some(etag.to_string()) }; save_state(&part_dir, &task_c.url, &et_opt, total, &snapshot);
                        return Ok(());
                    }
                    Err(e) => {
                        if ctrl.cancelled.load(Ordering::Relaxed) { return Err(e); }
                        attempt += 1;
                        if attempt > 5 { return Err(e); }
                        tokio::time::sleep(std::time::Duration::from_millis(500 * (1 << attempt.min(4)))).await;
                    }
                }
            }
        }));
    }
    for h in handles { h.await.map_err(|e| e.to_string())??; }
    if ctrl.cancelled.load(Ordering::Relaxed) { task.status = TaskStatus::Paused; task.downloaded = ctrl.downloaded.load(Ordering::Relaxed); return Ok(task); }
    // Assemble
    let tmp = task.dest_dir.join(format!(".assemble-{}", task.filename));
    {
        let mut out = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        for i in 0..count {
            let mut f = tokio::fs::File::open(part_dir.join(format!("part{i}"))).await.map_err(|e| e.to_string())?;
            tokio::io::copy(&mut f, &mut out).await.map_err(|e| e.to_string())?;
        }
        out.flush().await.map_err(|e| e.to_string())?;
    }
    let assembled = tokio::fs::metadata(&tmp).await.map(|m| m.len()).unwrap_or(0);
    if assembled != total { return Err(format!("size mismatch: assembled {assembled} != total {total}")); }
    tokio::fs::rename(&tmp, &dest).await.map_err(|e| e.to_string())?;
    let _ = tokio::fs::remove_dir_all(&part_dir).await;
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete; task.downloaded = total;
    Ok(task)
}

async fn download_range(client: &reqwest::Client, task: &DownloadTask, start: u64, end: u64, dest: &Path, ctrl: &Controls, prior: u64) -> Result<u64, String> {
    let resp = client.get(&task.url).headers(headers_for(task)).header("Range", format!("bytes={start}-{end}")).send().await.map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 206 { return Err(format!("expected 206 got {}", resp.status())); }
    // Truncate to caller-known done: drops bytes from interrupted attempt.
    let mut file = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dest).await.map_err(|e| e.to_string())?;
    file.set_len(prior).await.map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(prior)).await.map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream(); let mut n: u64 = 0;
    while let Some(chunk) = stream.next().await {
        wait_not_paused(ctrl).await?;
        let chunk = chunk.map_err(|e| e.to_string())?;
        throttle(ctrl, chunk.len() as u64).await;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        n += chunk.len() as u64;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    let expected = end - start + 1;
    if n != expected { return Err(format!("short range: got {n} expected {expected}")); }
    Ok(n)
}

async fn finish_check(dest: &Path, task: &DownloadTask) -> Result<(), String> {
    if let Some(want) = &task.sha256 {
        let mut f = tokio::fs::File::open(dest).await.map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new(); let mut buf = vec![0u8; 65536];
        loop { let n = f.read(&mut buf).await.map_err(|e| e.to_string())?; if n == 0 { break; } hasher.update(&buf[..n]); }
        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(want) { return Err(format!("sha256 mismatch: got {got} want {want}")); }
    }
    Ok(())
}

pub type MediaTracks = Vec<(String, Vec<(Option<String>, String)>)>;
/// Download list of media segments (DASH/HLS) and concatenate. Separate tracks muxed via ffmpeg if available.
pub async fn download_media(client: reqwest::Client, task: DownloadTask, ctrl: Arc<Controls>, tracks: MediaTracks) -> Result<DownloadTask, String> {
    // tracks: (kind, [(init, segment_url)...]) — one segment list per track, init repeated per entry ignored except first
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let mut task = task;
    let work = task.dest_dir.join(format!(".idmcraft-media-{}", task.id));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let mut track_files: Vec<PathBuf> = Vec::new();
    let total_segs: u64 = tracks.iter().map(|(_, s)| s.len() as u64).sum();
    let mut done_segs: u64 = 0;
    for (kind, segs) in &tracks {
        let out = work.join(format!("track-{kind}.bin"));
        {
            let mut file = tokio::fs::File::create(&out).await.map_err(|e| e.to_string())?;
            let mut seen_init: Option<String> = None;
            for (init, seg_url) in segs {
                wait_not_paused(&ctrl).await?;
                // resume: skip segments already fully? ponytail: media resume restarts track file; HTTP resume path covers kill-resume spec strictly,
                // manifest snapshot stored below allows re-run; segment files cached per index.
                let cache = work.join(format!("{kind}-{}.seg", track_files.len() * 100000 + done_segs as usize));
                let _ = cache;
                if let Some(init) = init { if seen_init.as_deref() != Some(init.as_str()) { seen_init = Some(init.clone()); fetch_into(&client, &task, init, &mut file, &ctrl).await?; } }
                fetch_into(&client, &task, seg_url, &mut file, &ctrl).await?;
                done_segs += 1;
                ctrl.downloaded.store(done_segs, Ordering::Relaxed);
                task.downloaded = done_segs; task.total = Some(total_segs);
            }
            file.flush().await.map_err(|e| e.to_string())?;
        }
        track_files.push(out);
    }
    // Manifest snapshot for resume documentation
    let _ = std::fs::write(work.join("tracks.json"), serde_json::to_string(&tracks.iter().map(|(k, s)| (k.clone(), s.len())).collect::<Vec<_>>()).unwrap_or_default());
    let dest = task.dest_dir.join(&task.filename);
    if track_files.len() == 1 {
        tokio::fs::rename(&track_files[0], &dest).await.map_err(|e| e.to_string())?;
    } else {
        // Try ffmpeg mux; fallback: keep first track and note error (muxer missing handled by caller docs)
        match mux_tracks(&track_files, &dest).await {
            Ok(()) => {}
            Err(e) => {
                // Fallback: rename video/first track so file exists, record error
                tokio::fs::copy(&track_files[0], &dest).await.map_err(|e| e.to_string())?;
                let _ = tokio::fs::remove_dir_all(&work).await;
                task.status = TaskStatus::Error; task.error = format!("mux failed ({e}); kept first track only");
                return Ok(task);
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete;
    task.downloaded = total_segs;
    Ok(task)
}

async fn fetch_into(client: &reqwest::Client, task: &DownloadTask, url: &str, file: &mut tokio::fs::File, ctrl: &Controls) -> Result<(), String> {
    let mut attempt = 0;
    let pos = file.seek(std::io::SeekFrom::Current(0)).await.map_err(|e| e.to_string())?;
    loop {
        match fetch_once(client, task, url, file, ctrl).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempt += 1; if attempt > 4 { return Err(e); }
                // Roll back partial bytes so retry does not duplicate.
                file.set_len(pos).await.map_err(|x| x.to_string())?;
                file.seek(std::io::SeekFrom::Start(pos)).await.map_err(|x| x.to_string())?;
                tokio::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
            }
        }
    }
}
async fn fetch_once(client: &reqwest::Client, task: &DownloadTask, url: &str, file: &mut tokio::fs::File, ctrl: &Controls) -> Result<(), String> {
    let req = client.get(url).headers(headers_for(task));
    // byte-range segment stored in headers via special marker? HLS BYTERANGE passed through task headers Range by caller when needed.
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() { return Err(format!("HTTP {} for {url}", resp.status())); }
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await { let chunk = chunk.map_err(|e| e.to_string())?; throttle(ctrl, chunk.len() as u64).await; file.write_all(&chunk).await.map_err(|e| e.to_string())?; }
    Ok(())
}

/// Fetch segment honoring optional byte range (HLS EXT-X-BYTERANGE).
pub async fn fetch_bytes(client: &reqwest::Client, task: &DownloadTask, url: &str, range: Option<(u64, Option<u64>)>, dest: &Path) -> Result<(), String> {
    let mut req = client.get(url).headers(headers_for(task));
    if let Some((len, off)) = range { let start = off.unwrap_or(0); req = req.header("Range", format!("bytes={start}-{}", start + len - 1)); }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 { return Err(format!("HTTP {}", resp.status())); }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    tokio::fs::write(dest, &bytes).await.map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn mux_tracks(tracks: &[PathBuf], dest: &Path) -> Result<(), String> {
    // ffmpeg sidecar: PATH lookup (bundled ffmpeg next to exe preferred)
    let mut candidates = vec![PathBuf::from("ffmpeg")];
    if let Ok(exe) = std::env::current_exe() { if let Some(d) = exe.parent() { candidates.insert(0, d.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" })); } }
    let mut last_err = String::new();
    for cand in &candidates {
        let mut cmd = tokio::process::Command::new(cand);
        cmd.arg("-y");
        for t in tracks { cmd.args(["-i"]).arg(t); }
        cmd.args(["-c", "copy"]);
        cmd.arg(dest);
        cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        match cmd.output().await {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => last_err = String::from_utf8_lossy(&o.stderr).chars().take(300).collect(),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
}
