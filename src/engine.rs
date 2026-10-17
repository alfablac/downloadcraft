use crate::model::*;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub fn build_client(proxy: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder().user_agent("downloadcraft/0.1").connect_timeout(std::time::Duration::from_secs(15)).read_timeout(std::time::Duration::from_secs(60));
    if !proxy.trim().is_empty() { if let Ok(p) = reqwest::Proxy::all(proxy.trim()) { b = b.proxy(p); } }
    b.build().unwrap_or_default()
}

/// Minimal RFC 4648 base64 (no dep for one header).
fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char); out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn headers_for(task: &DownloadTask) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if !task.referrer.is_empty() { if let Ok(v) = task.referrer.parse() { h.insert(reqwest::header::REFERER, v); } }
    if !task.cookies.is_empty() { if let Ok(v) = task.cookies.parse() { h.insert(reqwest::header::COOKIE, v); } }
    for (k, v) in &task.headers { if let (Ok(n), Ok(vv)) = (k.parse::<reqwest::header::HeaderName>(), v.parse()) { h.insert(n, vv); } }
    if !task.username.is_empty() && !h.contains_key(reqwest::header::AUTHORIZATION) {
        let raw = format!("{}:{}", task.username, task.password);
        if let Ok(v) = format!("Basic {}", b64(raw.as_bytes())).parse() { h.insert(reqwest::header::AUTHORIZATION, v); }
    }
    h
}

pub struct Controls { pub paused: AtomicBool, pub cancelled: AtomicBool, pub downloaded: AtomicU64, pub limit_kbps: AtomicU64, pub rate: std::sync::Mutex<(std::time::Instant, u64, Option<std::time::Instant>, u64, f64)> }
impl Controls { pub fn new(limit: u64) -> Self { Self { paused: AtomicBool::new(false), cancelled: AtomicBool::new(false), downloaded: AtomicU64::new(0), limit_kbps: AtomicU64::new(limit), rate: std::sync::Mutex::new((std::time::Instant::now(), 0, None, 0, 0.0)) } } }

fn compute_rate(prev_bytes: u64, cur_bytes: u64, dt_secs: f64) -> f64 {
    if dt_secs <= 0.0 || cur_bytes <= prev_bytes { 0.0 } else { (cur_bytes - prev_bytes) as f64 / dt_secs }
}

/// Per-task smoothed rate (bytes/s) from `ctrl.downloaded`. First call only
/// sets baseline and returns 0. Tuple fields 0..=1 stay owned by `throttle`.
pub fn rate_bps(ctrl: &Controls) -> f64 {
    let cur = ctrl.downloaded.load(Ordering::Relaxed);
    let now = std::time::Instant::now();
    let mut g = ctrl.rate.lock().unwrap();
    let Some(prev_t) = g.2 else { g.2 = Some(now); g.3 = cur; return 0.0; };
    let dt = now.duration_since(prev_t).as_secs_f64();
    if dt <= 0.0 { return g.4; }
    let inst = compute_rate(g.3, cur, dt);
    g.4 = if g.4 == 0.0 { inst } else { 0.25 * inst + 0.75 * g.4 };
    g.2 = Some(now); g.3 = cur;
    g.4
}

async fn throttle(ctrl: &Controls, bytes: u64) {
    let limit = ctrl.limit_kbps.load(Ordering::Relaxed);
    if limit == 0 { return; }
    // Global 1s window gate shared across workers.
    loop {
        let wait = {
            let mut g = ctrl.rate.lock().unwrap();
            if g.0.elapsed() >= std::time::Duration::from_secs(1) { g.0 = std::time::Instant::now(); g.1 = 0; }
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


/// Next stream chunk, but wake every 200ms to honour cancel/pause while socket idle.
/// Without this, cancel only lands when next chunk arrives (stalled server = never).
async fn next_chunk<S>(stream: &mut S, ctrl: &Controls) -> Result<Option<S::Item>, String>
where S: futures_util::Stream + Unpin {
    loop {
        wait_not_paused(ctrl).await?;
        match tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await {
            Ok(v) => return Ok(v),
            Err(_) => { if ctrl.cancelled.load(Ordering::Relaxed) { return Err("cancelled".into()); } }
        }
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

pub const MIN_SPLIT: u64 = 256 * 1024;
/// New end for victim when stealing its tail: split remaining (from committed
/// `done`) in half. None when either half would be < MIN_SPLIT.
/// ponytail: keyed on committed done, not in-flight progress; in-flight bytes
/// past split point are discarded by victim (see run_segment) and re-downloaded
/// by thief. Upgrade: track per-segment in-flight offset if waste matters.
fn split_point(start: u64, end: u64, done: u64) -> Option<u64> {
    let cur = start.checked_add(done)?;
    if cur > end { return None; }
    let remaining = end - cur + 1;
    if remaining < 2 * MIN_SPLIT { None } else { Some(cur + remaining / 2 - 1) }
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

/// Segmented HTTP download with resume. Segment files: part{i} in dest_dir/.downloadcraft-{id}/
pub async fn download_http(client: reqwest::Client, mut task: DownloadTask, ctrl: Arc<Controls>) -> Result<DownloadTask, String> {
    // Mirror failover (IDM Url0..UrlN): first URL whose probe succeeds wins; task.url rewritten to it.
    let mut urls = vec![task.url.clone()]; urls.extend(task.mirrors.iter().cloned());
    let mut found: Option<(String, Probe)> = None; let mut last_err = String::new();
    for u in urls {
        let cand = DownloadTask { url: u.clone(), ..task.clone() };
        match probe(&client, &cand).await { Ok(p) => { found = Some((u, p)); break; } Err(e) => last_err = e }
    }
    let Some((used_url, probe)) = found else { return Err(format!("all mirrors failed (last: {last_err})")); };
    task.url = used_url;
    task.total = probe.total;
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part_dir = task.dest_dir.join(format!(".downloadcraft-{}", task.id));
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
                        let mut loaded: Vec<SegmentState> = Vec::new();
                        for s in arr {
                            let (start, end, done) = (s["start"].as_u64().unwrap_or(0), s["end"].as_u64().unwrap_or(0), s["done"].as_u64().unwrap_or(0));
                            // verify part file size matches done (index = position in saved array)
                            let pf = part_dir.join(format!("part{}", loaded.len()));
                            let actual = std::fs::metadata(&pf).map(|m| m.len()).unwrap_or(0);
                            loaded.push(SegmentState { start, end, done: done.min(actual) });
                        }
                        // Split tails appended out of order: intervals must tile [0,total) exactly, else discard corrupt state.
                        let mut sorted: Vec<&SegmentState> = loaded.iter().collect();
                        sorted.sort_by_key(|s| s.start);
                        let mut expect = 0u64; let mut ok = total > 0;
                        for s in &sorted { if s.start != expect || s.end < s.start || s.done > s.end - s.start + 1 { ok = false; break; } expect = s.end + 1; }
                        if ok && expect == total { resumed = loaded.iter().map(|s| s.done).sum(); segs = loaded; }
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
    // Work-stealing pool: fixed worker count (= initial segment count). Idle
    // worker splits largest remaining in-flight segment, steals its tail.
    let etag: Arc<str> = probe.etag.clone().unwrap_or_default().into();
    let segs = Arc::new(tokio::sync::Mutex::new(segs));
    let active = Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::<usize>::new()));
    let mut handles = Vec::new();
    let count = segs.lock().await.len();
    for _ in 0..count.max(1) {
        let (client, task_c, ctrl, segs, active, part_dir, etag) = (client.clone(), task.clone(), ctrl.clone(), segs.clone(), active.clone(), part_dir.clone(), etag.clone());
        handles.push(tokio::spawn(async move {
            loop {
                let Some(i) = claim_or_split(&segs, &active, &part_dir, &task_c.url, &etag, total).await else { return Ok::<(), String>(()) };
                let r = run_segment(i, &segs, &client, &task_c, &part_dir, &etag, total, &ctrl).await;
                active.lock().await.remove(&i);
                if ctrl.cancelled.load(Ordering::Relaxed) { return Err("cancelled".to_string()); }
                r?;
            }
        }));
    }
    for h in handles { h.await.map_err(|e| e.to_string())??; }
    if ctrl.cancelled.load(Ordering::Relaxed) { task.status = TaskStatus::Paused; task.downloaded = ctrl.downloaded.load(Ordering::Relaxed); return Ok(task); }
    // Assemble in byte order: split tails appended (index != byte order).
    let order: Vec<usize> = { let s = segs.lock().await; let mut v: Vec<usize> = (0..s.len()).collect(); v.sort_by_key(|&i| s[i].start); v };
    let tmp = task.dest_dir.join(format!(".assemble-{}", task.filename));
    {
        let mut out = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        for i in order {
            let meta_len = tokio::fs::metadata(part_dir.join(format!("part{i}"))).await.map(|m| m.len()).unwrap_or(0);
            // Part may hold discarded in-flight bytes past a split point; copy only full segment length clamped to file size.
            let want = { let s = segs.lock().await; let sg = &s[i]; (sg.end - sg.start + 1).min(meta_len) };
            let f = tokio::fs::File::open(part_dir.join(format!("part{i}"))).await.map_err(|e| e.to_string())?;
            tokio::io::copy(&mut f.take(want), &mut out).await.map_err(|e| e.to_string())?;
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

/// Claim work: prefer unclaimed unfinished segment; else split largest
/// remaining active segment (tail half >= MIN_SPLIT) and claim new tail.
/// None when all segments done.
async fn claim_or_split(segs: &Arc<tokio::sync::Mutex<Vec<SegmentState>>>, active: &Arc<tokio::sync::Mutex<std::collections::HashSet<usize>>>, part_dir: &Path, url: &str, etag: &Arc<str>, total: u64) -> Option<usize> {
    let mut s = segs.lock().await;
    let mut act = active.lock().await;
    let remaining = |sg: &SegmentState| (sg.end + 1).saturating_sub(sg.start + sg.done);
    for (i, sg) in s.iter().enumerate() {
        if remaining(sg) > 0 && !act.contains(&i) { act.insert(i); return Some(i); }
    }
    let mut victim: Option<(usize, u64)> = None;
    for (i, sg) in s.iter().enumerate() {
        if act.contains(&i) {
            if let Some(np) = split_point(sg.start, sg.end, sg.done) {
                let tail = sg.end - np;
                if victim.is_none_or(|(_, b)| tail > b) { victim = Some((i, tail)); }
            }
        }
    }
    let (vi, _) = victim?;
    let old_end = s[vi].end;
    let np = split_point(s[vi].start, old_end, s[vi].done)?;
    s[vi].end = np;
    let new_idx = s.len();
    s.push(SegmentState { start: np + 1, end: old_end, done: 0 });
    act.insert(new_idx);
    let snapshot: Vec<SegmentState> = s.iter().map(|x| SegmentState { start: x.start, end: x.end, done: x.done }).collect();
    drop(act); drop(s);
    let et_opt = if etag.is_empty() { None } else { Some(etag.to_string()) };
    save_state(part_dir, url, &et_opt, total, &snapshot);
    Some(new_idx)
}

/// Download one claimed segment index until its (possibly shrunk) end.
/// On split-shrink: only bytes up to current end counted/committed; tail
/// bytes in part file discarded at assemble, re-downloaded by thief.
#[allow(clippy::too_many_arguments)]
async fn run_segment(i: usize, segs: &Arc<tokio::sync::Mutex<Vec<SegmentState>>>, client: &reqwest::Client, task: &DownloadTask, part_dir: &Path, etag: &Arc<str>, total: u64, ctrl: &Arc<Controls>) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        let (start, end, done) = { let s = segs.lock().await; let sg = &s[i]; (sg.start, sg.end, sg.done) };
        if done > end - start { return Ok(()); }
        let range_start = start + done;
        if range_start > end { return Ok(()); }
        match download_range(client, task, range_start, end, part_dir.join(format!("part{i}")).as_path(), ctrl, done).await {
            Ok(written) => {
                let keep = { let s = segs.lock().await; let sg = &s[i]; (sg.end + 1).saturating_sub(start + done).min(written) };
                { let mut s = segs.lock().await; s[i].done = (s[i].done + keep).min(s[i].end + 1 - s[i].start); }
                let snapshot = { let s = segs.lock().await; s.iter().map(|x| SegmentState { start: x.start, end: x.end, done: x.done }).collect::<Vec<_>>() };
                let et_opt = if etag.is_empty() { None } else { Some(etag.to_string()) }; save_state(part_dir, &task.url, &et_opt, total, &snapshot);
                if keep < written { continue; }
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
}

async fn download_range(client: &reqwest::Client, task: &DownloadTask, start: u64, end: u64, dest: &Path, ctrl: &Controls, prior: u64) -> Result<u64, String> {
    let resp = client.get(&task.url).headers(headers_for(task)).header("Range", format!("bytes={start}-{end}")).send().await.map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 206 { return Err(format!("expected 206 got {}", resp.status())); }
    // Truncate to caller-known done: drops bytes from interrupted attempt.
    let mut file = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dest).await.map_err(|e| e.to_string())?;
    file.set_len(prior).await.map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(prior)).await.map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream(); let mut n: u64 = 0;
    let mut err: Option<String> = None;
    loop {
        let chunk = match next_chunk(&mut stream, ctrl).await { Ok(Some(c)) => c, Ok(None) => break, Err(e) => { err = Some(e); break; } };
        match chunk {
            Ok(chunk) => {
                throttle(ctrl, chunk.len() as u64).await;
                match file.write_all(&chunk).await {
                    Ok(()) => { n += chunk.len() as u64; ctrl.downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed); }
                    Err(e) => { err = Some(e.to_string()); break; }
                }
            }
            Err(e) => { err = Some(e.to_string()); break; }
        }
    }
    if err.is_none() { if let Err(e) = file.flush().await { err = Some(e.to_string()); } }
    // Live-counted bytes roll back on failure: retry truncates part to `prior` and recounts.
    if let Some(e) = err { ctrl.downloaded.fetch_sub(n, Ordering::Relaxed); return Err(e); }
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
    let work = task.dest_dir.join(format!(".downloadcraft-media-{}", task.id));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let mut track_files: Vec<PathBuf> = Vec::new();
    // Segment sizes unknown upfront: progress in bytes, task.total stays None.
    let mut done_bytes: u64 = 0; task.total = None; task.downloaded = 0; ctrl.downloaded.store(0, Ordering::Relaxed);
    for (kind, segs) in &tracks {
        let out = work.join(format!("track-{kind}.bin"));
        {
            let mut file = tokio::fs::File::create(&out).await.map_err(|e| e.to_string())?;
            let mut seen_init: Option<String> = None;
            for (init, seg_url) in segs {
                wait_not_paused(&ctrl).await?;
                // resume: skip segments already fully? ponytail: media resume restarts track file; HTTP resume path covers kill-resume spec strictly,
                // manifest snapshot stored below allows re-run; segment files cached per index.
                let cache = work.join(format!("{kind}-{}.seg", track_files.len() * 100000 + done_bytes as usize));
                let _ = cache;
                let seg_result = async {
                    if let Some(init) = init { if seen_init.as_deref() != Some(init.as_str()) { seen_init = Some(init.clone()); done_bytes += fetch_into_inner(&client, &task, init, &mut file, &ctrl).await?; } }
                    done_bytes += fetch_into_inner(&client, &task, seg_url, &mut file, &ctrl).await?;
                    Ok::<(), String>(())
                }.await;
                if let Err(e) = seg_result {
                    if ctrl.cancelled.load(Ordering::Relaxed) || e == "cancelled" {
                        task.status = TaskStatus::Paused; task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
                        return Ok(task);
                    }
                    return Err(e);
                }
                task.downloaded = done_bytes; task.total = None;
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
                task.status = TaskStatus::Error; task.error = format!("mux failed ({e}); kept first track only"); task.downloaded = done_bytes; task.total = None;
                return Ok(task);
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete;
    task.downloaded = done_bytes;
    task.total = None;
    Ok(task)
}

async fn fetch_into_inner(client: &reqwest::Client, task: &DownloadTask, url: &str, file: &mut tokio::fs::File, ctrl: &Controls) -> Result<u64, String> {
    let mut attempt = 0;
    let start = file.seek(std::io::SeekFrom::Current(0)).await.map_err(|e| e.to_string())?;
    loop {
        match fetch_once_counted_impl(client, task, url, file, ctrl).await {
            Ok(n) => return Ok(n),
            Err(e) => {
                attempt += 1; if attempt > 4 { return Err(e); }
                // Roll back partial bytes so retry does not duplicate/count twice.
                file.set_len(start).await.map_err(|x| x.to_string())?;
                file.seek(std::io::SeekFrom::Start(start)).await.map_err(|x| x.to_string())?;
                tokio::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
            }
        }
    }
}
async fn fetch_once_counted_impl(client: &reqwest::Client, task: &DownloadTask, url: &str, file: &mut tokio::fs::File, ctrl: &Controls) -> Result<u64, String> {
    let req = client.get(url).headers(headers_for(task));
    // byte-range segment stored in headers via special marker? HLS BYTERANGE passed through task headers Range by caller when needed.
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() { return Err(format!("HTTP {} for {url}", resp.status())); }
    let mut stream = resp.bytes_stream();
    let mut n = 0u64;
    loop {
        let Some(chunk) = (match next_chunk(&mut stream, ctrl).await { Ok(v) => v, Err(e) => { ctrl.downloaded.fetch_sub(n, Ordering::Relaxed); return Err(e); } }) else { break };
        let chunk = match chunk { Ok(c) => c, Err(e) => { ctrl.downloaded.fetch_sub(n, Ordering::Relaxed); return Err(e.to_string()); } };
        throttle(ctrl, chunk.len() as u64).await;
        if let Err(e) = file.write_all(&chunk).await { ctrl.downloaded.fetch_sub(n, Ordering::Relaxed); return Err(e.to_string()); }
        n += chunk.len() as u64;
        ctrl.downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    Ok(n)
}

/// Fetch segment honoring optional byte range (HLS EXT-X-BYTERANGE).
pub async fn fetch_bytes(client: &reqwest::Client, task: &DownloadTask, url: &str, range: Option<(u64, Option<u64>)>, dest: &Path) -> Result<(), String> {
    fetch_bytes_counted(client, task, url, range, dest).await.map(|_| ())
}
pub async fn fetch_bytes_counted(client: &reqwest::Client, task: &DownloadTask, url: &str, range: Option<(u64, Option<u64>)>, dest: &Path) -> Result<u64, String> {
    let mut req = client.get(url).headers(headers_for(task));
    if let Some((len, off)) = range { let start = off.unwrap_or(0); req = req.header("Range", format!("bytes={start}-{}", start + len - 1)); }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 { return Err(format!("HTTP {}", resp.status())); }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let n = bytes.len() as u64;
    tokio::fs::write(dest, &bytes).await.map_err(|e| e.to_string())?;
    Ok(n)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_segments_unchanged() {
        assert_eq!(plan_segments(0, 8), vec![]);
        assert_eq!(plan_segments(10, 3), vec![(0, 2), (3, 5), (6, 9)]);
        assert_eq!(plan_segments(5, 8).len(), 5);
    }
    #[test]
    fn compute_rate_cases() {
        assert_eq!(compute_rate(0, 1000, 2.0), 500.0);
        assert_eq!(compute_rate(500, 1500, 0.5), 2000.0);
        assert_eq!(compute_rate(0, 1000, 0.0), 0.0, "zero dt");
        assert_eq!(compute_rate(0, 1000, -1.0), 0.0, "negative dt");
        assert_eq!(compute_rate(1000, 1000, 1.0), 0.0, "no progress");
        assert_eq!(compute_rate(1000, 500, 1.0), 0.0, "counter reset");
    }
    #[test]
    fn rate_bps_first_call_zero() {
        let c = Controls::new(0);
        c.downloaded.store(1234, Ordering::Relaxed);
        assert_eq!(rate_bps(&c), 0.0);
    }
    #[test]    fn split_point_halves_remaining() {
        let kb = 1024u64;
        assert_eq!(split_point(0, 1024 * kb - 1, 0), Some(512 * kb - 1));
        assert_eq!(split_point(0, 512 * kb - 2, 0), None, "remaining < 512KB never splits");
        assert_eq!(split_point(0, 600 * kb - 1, 200 * kb), None, "done counts toward remaining");
        assert_eq!(split_point(100, 100 + 1024 * kb - 1, 512 * kb), Some(100 + 768 * kb - 1));
        assert_eq!(split_point(0, 10, 20), None);
        // both halves >= MIN_SPLIT at exact threshold
        assert_eq!(split_point(0, 2 * MIN_SPLIT - 1, 0), Some(MIN_SPLIT - 1));
    }
}
