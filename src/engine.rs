use crate::model::*;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub fn build_client(proxy: &str) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("webcraft/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .pool_idle_timeout(std::time::Duration::from_secs(20))
        .pool_max_idle_per_host(8);
    if !proxy.trim().is_empty() {
        if let Ok(p) = reqwest::Proxy::all(proxy.trim()) {
            b = b.proxy(p);
        }
    }
    b.build().unwrap_or_default()
}

/// Minimal RFC 4648 base64 (no dep for one header).
fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn headers_for(task: &DownloadTask) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if !task.referrer.is_empty() {
        if let Ok(v) = task.referrer.parse() {
            h.insert(reqwest::header::REFERER, v);
        }
    }
    if !task.cookies.is_empty() {
        if let Ok(v) = task.cookies.parse() {
            h.insert(reqwest::header::COOKIE, v);
        }
    }
    for (k, v) in &task.headers {
        // Pseudo-headers are task config, never sent on wire.
        if crate::ytdlp::YT_HEADER_KEYS.iter().any(|x| k.eq_ignore_ascii_case(x))
            || k.eq_ignore_ascii_case("x-page-url")
            || k.eq_ignore_ascii_case("x-page-stream")
            || k.eq_ignore_ascii_case("x-page-intent")
            || k.eq_ignore_ascii_case("x-rep-id")
            || k.eq_ignore_ascii_case("x-variant-index")
            || k.eq_ignore_ascii_case("x-nzb")
        {
            continue;
        }
        if let (Ok(n), Ok(vv)) = (k.parse::<reqwest::header::HeaderName>(), v.parse()) {
            h.insert(n, vv);
        }
    }
    if !task.username.is_empty() && !h.contains_key(reqwest::header::AUTHORIZATION) {
        let raw = format!("{}:{}", task.username, task.password);
        if let Ok(v) = format!("Basic {}", b64(raw.as_bytes())).parse() {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    h
}

pub struct Controls {
    pub paused: AtomicBool,
    pub cancelled: AtomicBool,
    pub downloaded: AtomicU64,
    pub limit_kbps: AtomicU64,
    pub rate: std::sync::Mutex<(
        std::time::Instant,
        u64,
        Option<std::time::Instant>,
        u64,
        f64,
    )>,
    /// Total bytes when known (0 = unknown). GUI needs this live; task.total only lands at finish.
    pub total: AtomicU64,
    /// Per-part (done, expected). Segmented HTTP/FTP/single-file use bytes;
    /// manifest downloads use segment counts and set `parts_are_segments`.
    /// Multi-file NZB: one entry per file (bytes, flag false); single-file
    /// NZB keeps segment-count convention (flag true).
    pub parts: std::sync::Mutex<Vec<(u64, u64)>>,
    /// Torrent files aligned 1:1 with `parts`: (librqbit index, relative path).
    /// All torrent files published, unselected included (progress checkboxes);
    /// empty for non-torrent downloads (generic Part/File label fallback).
    pub part_files: std::sync::Mutex<Vec<(usize, String)>>,
    /// Pending live reselection: GUI writes Some(normalized selection, empty=all),
    /// torrent poll loop consumes + applies. None = no request.
    pub desired_files: std::sync::Mutex<Option<Vec<usize>>>,
    pub parts_are_segments: AtomicBool,
    /// Torrent swarm snapshot (only torrent worker writes). Live-only:
    /// never persisted, GUI renders last known for Complete tasks.
    pub torrent: std::sync::Mutex<Option<TorrentSnap>>,
}

/// Plain-data torrent swarm snapshot for Info dialog. No seeds/leecher split
/// exists in librqbit: peers_live = connected peers (seeds+leechers mixed).
#[derive(Clone, Default)]
pub struct TorrentSnap {
    pub trackers: Vec<String>,
    pub peers_live: u32,
    pub peers_connecting: u32,
    pub peers_seen: u32,
    pub uploaded: u64,
    pub pieces_done: u64,
    pub pieces_total: u32,
    pub info_hash: String,
    pub live: bool,
}
impl Controls {
    pub fn new(limit: u64) -> Self {
        Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            downloaded: AtomicU64::new(0),
            limit_kbps: AtomicU64::new(limit),
            rate: std::sync::Mutex::new((std::time::Instant::now(), 0, None, 0, 0.0)),
            total: AtomicU64::new(0),
            parts: std::sync::Mutex::new(Vec::new()),
            part_files: std::sync::Mutex::new(Vec::new()),
            desired_files: std::sync::Mutex::new(None),
            parts_are_segments: AtomicBool::new(false),
            torrent: std::sync::Mutex::new(None),
        }
    }
}

fn compute_rate(prev_bytes: u64, cur_bytes: u64, dt_secs: f64) -> f64 {
    if dt_secs <= 0.0 || cur_bytes <= prev_bytes {
        0.0
    } else {
        (cur_bytes - prev_bytes) as f64 / dt_secs
    }
}

/// Per-task smoothed rate (bytes/s) from `ctrl.downloaded`. First call only
/// sets baseline and returns 0. Tuple fields 0..=1 stay owned by `throttle`.
pub fn rate_bps(ctrl: &Controls) -> f64 {
    let cur = ctrl.downloaded.load(Ordering::Relaxed);
    let now = std::time::Instant::now();
    let mut g = ctrl.rate.lock().unwrap_or_else(|e| e.into_inner());
    let Some(prev_t) = g.2 else {
        g.2 = Some(now);
        g.3 = cur;
        return 0.0;
    };
    let dt = now.duration_since(prev_t).as_secs_f64();
    if dt <= 0.0 {
        return g.4;
    }
    let inst = compute_rate(g.3, cur, dt);
    g.4 = if g.4 == 0.0 {
        inst
    } else {
        0.25 * inst + 0.75 * g.4
    };
    g.2 = Some(now);
    g.3 = cur;
    g.4
}

async fn throttle(ctrl: &Controls, bytes: u64) {
    let limit = ctrl.limit_kbps.load(Ordering::Relaxed);
    if limit == 0 {
        return;
    }
    // Global 1s window gate shared across workers.
    loop {
        let wait = {
            let mut g = ctrl.rate.lock().unwrap_or_else(|e| e.into_inner());
            if g.0.elapsed() >= std::time::Duration::from_secs(1) {
                g.0 = std::time::Instant::now();
                g.1 = 0;
            }
            let budget = limit.saturating_mul(1024);
            if bytes > budget {
                if g.1 == 0 {
                    g.1 = bytes;
                    return;
                }
                std::time::Duration::from_secs(1).saturating_sub(g.0.elapsed())
            } else if g.1 + bytes <= budget {
                g.1 += bytes;
                return;
            } else {
                std::time::Duration::from_secs(1).saturating_sub(g.0.elapsed())
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// Next stream chunk, but wake every 200ms to honour cancel/pause while socket idle.
/// Without this, cancel only lands when next chunk arrives (stalled server = never).
async fn next_chunk<S>(stream: &mut S, ctrl: &Controls) -> Result<Option<S::Item>, String>
where
    S: futures_util::Stream + Unpin,
{
    loop {
        wait_not_paused(ctrl).await?;
        match tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await {
            Ok(v) => return Ok(v),
            Err(_) => {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
            }
        }
    }
}

pub async fn wait_not_paused(ctrl: &Controls) -> Result<(), String> {
    while ctrl.paused.load(Ordering::Relaxed) {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if ctrl.cancelled.load(Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    Ok(())
}

/// Split total into segment ranges.
pub fn plan_segments(total: u64, wanted: u32) -> Vec<(u64, u64)> {
    if total == 0 {
        return vec![];
    }
    let n = (wanted.max(1) as u64).min(total.max(1));
    let chunk = total / n;
    let mut v = Vec::new();
    let mut s = 0;
    for i in 0..n {
        let e = if i == n - 1 { total - 1 } else { s + chunk - 1 };
        v.push((s, e));
        s = e + 1;
    }
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
    if cur > end {
        return None;
    }
    let remaining = end - cur + 1;
    if remaining < 2 * MIN_SPLIT {
        None
    } else {
        Some(cur + remaining / 2 - 1)
    }
}

pub struct Probe {
    pub total: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub final_url: String,
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
}

/// Percent-decode to bytes (no '+' handling: CD filenames are not form-encoded).
fn pct_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Sanitize a server-supplied filename: strip path separators + control
/// chars, reject empty/dot-only results. Reuses model::sanitize_filename
/// (which already strips dirs and maps <>:"/\|?* and controls).
pub fn sanitize_cd_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base.chars().all(|c| c == '.') {
        return None;
    }
    let cleaned: String = base.chars().filter(|c| (*c as u32) >= 32).collect();
    if cleaned.trim().is_empty() {
        return None;
    }
    let s = crate::model::sanitize_filename(&cleaned);
    if s == "download.bin" && !cleaned.eq_ignore_ascii_case("download.bin") {
        None // sanitizer fallback means nothing usable survived
    } else {
        Some(s)
    }
}

/// Parse Content-Disposition filename. RFC 5987 `filename*` (charset'lang'%XX)
/// wins over plain `filename=`. Returns sanitized name.
pub fn parse_content_disposition(header: &str) -> Option<String> {
    // filename* first
    for part in header.split(';') {
        let part = part.trim();
        let Some(eq) = part.find('=') else { continue };
        if !part[..eq].trim().eq_ignore_ascii_case("filename*") {
            continue;
        }
        let v = part[eq + 1..].trim().trim_matches('"');
        // charset'lang'value
        let mut it = v.splitn(3, '\'');
        let charset = it.next().unwrap_or("utf-8").to_ascii_lowercase();
        let _lang = it.next();
        let Some(encoded) = it.next() else { continue };
        let bytes = pct_decode_bytes(encoded);
        let text = if charset.contains("utf-8") || charset.contains("utf8") {
            String::from_utf8_lossy(&bytes).into_owned()
        } else if charset.contains("iso-8859-1") || charset.contains("latin1") {
            bytes.iter().map(|&b| b as char).collect()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        if let Some(s) = sanitize_cd_filename(&text) {
            return Some(s);
        }
    }
    for part in header.split(';') {
        let part = part.trim();
        let Some(eq) = part.find('=') else { continue };
        if !part[..eq].trim().eq_ignore_ascii_case("filename") {
            continue;
        }
        let raw = part[eq + 1..].trim();
        let unq = if raw.len() >= 2 && raw.starts_with('"') {
            // Quoted-string: unescape \" and \\, stop at closing quote.
            let mut out = String::new();
            let mut chars = raw[1..].chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' => break,
                    _ => out.push(c),
                }
            }
            out
        } else {
            raw.trim_matches('"').to_string()
        };
        // Plain filename= may still be percent-encoded by broken servers.
        let decoded = String::from_utf8_lossy(&pct_decode_bytes(&unq)).into_owned();
        if let Some(s) = sanitize_cd_filename(&decoded) {
            return Some(s);
        }
    }
    None
}

/// Generic placeholder names eligible for server-name adoption. Protects
/// user-renamed tasks: capture dialog sets filename via sanitize_filename,
/// so any specific name (with extension, not in this set) is left alone.
pub fn is_generic_filename(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() {
        return true;
    }
    let stem = n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n);
    stem.eq_ignore_ascii_case("download")
        || stem.eq_ignore_ascii_case("index")
        || Path::new(n).extension().is_none()
}

fn mime_ext(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match ct.as_str() {
        "application/zip" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/pdf" => "pdf",
        "application/octet-stream" => "bin",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        "audio/ogg" => "ogg",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "text/plain" => "txt",
        "application/json" => "json",
        "application/x-iso9660-image" => "iso",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" => "msi",
        "application/vnd.android.package-archive" => "apk",
        _ => return None,
    })
}

/// Adopt server filename when current name is generic. CD name wins; else
/// append MIME extension when current name lacks one.
pub fn adopt_server_filename(
    current: &str,
    content_disposition: Option<&str>,
    content_type: Option<&str>,
) -> Option<String> {
    if !is_generic_filename(current) {
        return None;
    }
    if let Some(cd) = content_disposition.and_then(parse_content_disposition) {
        if !cd.eq_ignore_ascii_case(current) {
            return Some(cd);
        }
    }
    if Path::new(current).extension().is_none() {
        if let Some(ext) = content_type.and_then(mime_ext) {
            let stem = if current.trim().is_empty() {
                "download"
            } else {
                current.trim()
            };
            return Some(format!("{stem}.{ext}"));
        }
    }
    None
}

/// Same-destination guard for final rename: dest exists with different
/// size than incoming file -> pick `name (1).ext`, `name (2).ext`, ...
/// Same size (likely own redownload) or absent dest: keep dest (rename replaces).
pub fn dedup_dest(dest: &Path, incoming_size: Option<u64>) -> PathBuf {
    let occupied_diff = |p: &Path| -> bool {
        match (std::fs::metadata(p).map(|m| m.len()).ok(), incoming_size) {
            (Some(existing), Some(inc)) => existing != inc,
            (Some(_), None) => true,
            _ => false,
        }
    };
    if !occupied_diff(dest) {
        return dest.to_path_buf();
    }
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    let ext = dest.extension().map(|s| s.to_string_lossy().into_owned());
    let parent = dest.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for n in 1..1000 {
        let cand = match &ext {
            Some(e) => parent.join(format!("{stem} ({n}).{e}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        if !occupied_diff(&cand) && !cand.exists() {
            return cand;
        }
    }
    dest.to_path_buf()
}

pub async fn probe(client: &reqwest::Client, task: &DownloadTask) -> Result<Probe, String> {
    let resp = client
        .get(&task.url)
        .headers(headers_for(task))
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let lm = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let final_url = resp.url().to_string();
    let content_disposition = resp
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if status.as_u16() == 206 {
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let total = cr.rsplit('/').next().and_then(|v| v.parse().ok());
        let _ = resp.bytes().await;
        Ok(Probe {
            total,
            accept_ranges: true,
            etag,
            last_modified: lm,
            final_url,
            content_disposition: content_disposition.clone(),
            content_type: content_type.clone(),
        })
    } else if status.is_success() {
        let total = resp.content_length();
        let _ = resp.bytes().await;
        Ok(Probe {
            total,
            accept_ranges: false,
            etag,
            last_modified: lm,
            final_url,
            content_disposition,
            content_type,
        })
    } else {
        Err(format!("HTTP {status}"))
    }
}

struct SegmentState {
    start: u64,
    end: u64,
    done: u64,
}
fn state_path_for(part_dir: &Path) -> PathBuf {
    part_dir.join("state.json")
}
fn save_state(
    part_dir: &Path,
    url: &str,
    etag: &Option<String>,
    total: u64,
    segs: &[SegmentState],
) {
    let v: Vec<_> = segs
        .iter()
        .map(|s| serde_json::json!({"start":s.start,"end":s.end,"done":s.done}))
        .collect();
    let j = serde_json::json!({"url":url,"etag":etag,"total":total,"segments":v});
    let _ = std::fs::write(state_path_for(part_dir), j.to_string());
}

/// Segmented HTTP download with resume. Segment files: part{i} in dest_dir/.downloadcraft-{id}/
pub async fn download_http(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
) -> Result<DownloadTask, String> {
    // Mirror failover (IDM Url0..UrlN): first URL whose probe succeeds wins; task.url rewritten to it.
    let mut urls = vec![task.url.clone()];
    urls.extend(task.mirrors.iter().cloned());
    let mut found: Option<(String, Probe)> = None;
    let mut last_err = String::new();
    for u in urls {
        let cand = DownloadTask {
            url: u.clone(),
            ..task.clone()
        };
        match probe(&client, &cand).await {
            Ok(p) => {
                found = Some((u, p));
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some((used_url, probe)) = found else {
        return Err(format!("all mirrors failed (last: {last_err})"));
    };
    task.url = used_url;
    task.total = probe.total;
    // Filename repair: adopt CD/MIME name only for generic placeholders,
    // never overriding user-renamed (specific) filenames.
    if let Some(name) = adopt_server_filename(
        &task.filename,
        probe.content_disposition.as_deref(),
        probe.content_type.as_deref(),
    ) {
        task.filename = name;
    }
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part_dir = task.dest_dir.join(format!(".downloadcraft-{}", task.id));
    // Server ignored Range: plain streaming download with throttle.
    if !probe.accept_ranges || probe.total.is_none() {
        let resp = client
            .get(&task.url)
            .headers(headers_for(&task))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        task.total = resp.content_length().or(task.total);
        if let Some(t) = task.total {
            ctrl.total.store(t, Ordering::Relaxed);
            ctrl.parts_are_segments.store(false, Ordering::Relaxed);
            *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(0, t)];
        }
        // Plain path: response headers carry CD/MIME too.
        if let Some(name) = adopt_server_filename(
            &task.filename,
            resp.headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
        ) {
            task.filename = name;
        }
        let dest = task.dest_dir.join(&task.filename);
        let tmp = task.dest_dir.join(format!(".partial-{}", task.filename));
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        let mut n: u64 = 0;
        loop {
            let Some(chunk) = next_chunk(&mut stream, &ctrl).await? else {
                break;
            };
            let chunk = chunk.map_err(|e| e.to_string())?;
            throttle(&ctrl, chunk.len() as u64).await;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            n += chunk.len() as u64;
            ctrl.downloaded.store(n, Ordering::Relaxed);
            task.downloaded = n;
            if let Some(t) = task.total {
                if let Some(part) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).first_mut() {
                    part.0 = n.min(t);
                }
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        drop(file);
        let dest = dedup_dest(&dest, Some(n));
        if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
            if name != task.filename {
                task.filename = name;
            }
        }
        tokio::fs::rename(&tmp, &dest)
            .await
            .map_err(|e| e.to_string())?;
        // Validate before Complete: manifest/HTML/magic mismatch quarantined.
        match validate_output(&dest, &task.filename) {
            Ok(_) => {}
            Err(ValidateFail::TsInMp4) => {
                let final_path = ensure_mp4_or_ts(&dest).await?;
                if let Some(name) = final_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                {
                    task.filename = name;
                }
                finish_check(&final_path, &task).await?;
                task.status = TaskStatus::Complete;
                task.downloaded = n;
                return Ok(task);
            }
            Err(e) => return Err(quarantine_bad(&dest, &e)),
        }
        finish_check(&dest, &task).await?;
        task.status = TaskStatus::Complete;
        task.downloaded = n;
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
                let etag_ok = v
                    .get("etag")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
                    == probe.etag
                    || probe.etag.is_none();
                if v.get("url").and_then(|u| u.as_str()) == Some(task.url.as_str())
                    && v.get("total").and_then(|t| t.as_u64()) == Some(total)
                    && etag_ok
                {
                    if let Some(arr) = v.get("segments").and_then(|s| s.as_array()) {
                        let mut loaded: Vec<SegmentState> = Vec::new();
                        for s in arr {
                            let (start, end, done) = (
                                s["start"].as_u64().unwrap_or(0),
                                s["end"].as_u64().unwrap_or(0),
                                s["done"].as_u64().unwrap_or(0),
                            );
                            // verify part file size matches done (index = position in saved array)
                            let pf = part_dir.join(format!("part{}", loaded.len()));
                            let actual = std::fs::metadata(&pf).map(|m| m.len()).unwrap_or(0);
                            loaded.push(SegmentState {
                                start,
                                end,
                                done: done.min(actual),
                            });
                        }
                        // Split tails appended out of order: intervals must tile [0,total) exactly, else discard corrupt state.
                        let mut sorted: Vec<&SegmentState> = loaded.iter().collect();
                        sorted.sort_by_key(|s| s.start);
                        let mut expect = 0u64;
                        let mut ok = total > 0;
                        for s in &sorted {
                            if s.start != expect || s.end < s.start || s.done > s.end - s.start + 1
                            {
                                ok = false;
                                break;
                            }
                            expect = s.end + 1;
                        }
                        if ok && expect == total {
                            resumed = loaded.iter().map(|s| s.done).sum();
                            segs = loaded;
                        }
                    }
                }
            }
        }
    }
    if segs.is_empty() {
        let wanted = if task.segments == 0 {
            crate::model::Settings::default().max_connections
        } else {
            task.segments
        };
        segs = plan_segments(total, wanted)
            .into_iter()
            .map(|(start, end)| SegmentState {
                start,
                end,
                done: 0,
            })
            .collect();
    }
    ctrl.downloaded.store(resumed, Ordering::Relaxed);
    task.downloaded = resumed;
    ctrl.total.store(total, Ordering::Relaxed);
    ctrl.parts_are_segments.store(false, Ordering::Relaxed);
    {
        let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
        *pg = segs.iter().map(|x| (x.done, x.end.saturating_add(1).saturating_sub(x.start))).collect();
    }
    // Work-stealing pool: fixed worker count (= initial segment count). Idle
    // worker splits largest remaining in-flight segment, steals its tail.
    let etag: Arc<str> = probe.etag.clone().unwrap_or_default().into();
    let segs = Arc::new(tokio::sync::Mutex::new(segs));
    let active = Arc::new(tokio::sync::Mutex::new(
        std::collections::HashSet::<usize>::new(),
    ));
    let mut handles = Vec::new();
    let count = segs.lock().await.len();
    for _ in 0..count.max(1) {
        let (client, task_c, ctrl, segs, active, part_dir, etag) = (
            client.clone(),
            task.clone(),
            ctrl.clone(),
            segs.clone(),
            active.clone(),
            part_dir.clone(),
            etag.clone(),
        );
        handles.push(tokio::spawn(async move {
            loop {
                let Some(i) =
                    claim_or_split(&segs, &active, &part_dir, &task_c.url, &etag, total, &ctrl)
                        .await
                else {
                    return Ok::<(), String>(());
                };
                let r =
                    run_segment(i, &segs, &client, &task_c, &part_dir, &etag, total, &ctrl).await;
                active.lock().await.remove(&i);
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err("cancelled".to_string());
                }
                r?;
            }
        }));
    }
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if ctrl.cancelled.load(Ordering::Relaxed) || e == "cancelled" => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(e.to_string()),
        }
    }
    if ctrl.cancelled.load(Ordering::Relaxed) {
        task.status = TaskStatus::Paused;
        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
        return Ok(task);
    }
    // Assemble in byte order: split tails appended (index != byte order).
    let order: Vec<usize> = {
        let s = segs.lock().await;
        let mut v: Vec<usize> = (0..s.len()).collect();
        v.sort_by_key(|&i| s[i].start);
        v
    };
    let tmp = task.dest_dir.join(format!(".assemble-{}", task.filename));
    {
        let mut out = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        for i in order {
            let meta_len = tokio::fs::metadata(part_dir.join(format!("part{i}")))
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            // Part may hold discarded in-flight bytes past a split point; copy only full segment length clamped to file size.
            let want = {
                let s = segs.lock().await;
                let sg = &s[i];
                (sg.end - sg.start + 1).min(meta_len)
            };
            let f = tokio::fs::File::open(part_dir.join(format!("part{i}")))
                .await
                .map_err(|e| e.to_string())?;
            tokio::io::copy(&mut f.take(want), &mut out)
                .await
                .map_err(|e| e.to_string())?;
        }
        out.flush().await.map_err(|e| e.to_string())?;
    }
    let assembled = tokio::fs::metadata(&tmp)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if assembled != total {
        return Err(format!(
            "size mismatch: assembled {assembled} != total {total}"
        ));
    }
    let dest = dedup_dest(&dest, Some(total));
    if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
        if name != task.filename {
            task.filename = name;
        }
    }
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| e.to_string())?;
    let _ = tokio::fs::remove_dir_all(&part_dir).await;
    match validate_output(&dest, &task.filename) {
        Ok(_) => {}
        Err(ValidateFail::TsInMp4) => {
            let final_path = ensure_mp4_or_ts(&dest).await?;
            if let Some(name) = final_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
            {
                task.filename = name;
            }
            finish_check(&final_path, &task).await?;
            task.status = TaskStatus::Complete;
            task.downloaded = total;
            return Ok(task);
        }
        Err(e) => return Err(quarantine_bad(&dest, &e)),
    }
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete;
    task.downloaded = total;
    Ok(task)
}

/// Claim work: prefer unclaimed unfinished segment; else split largest
/// remaining active segment (tail half >= MIN_SPLIT) and claim new tail.
/// None when all segments done.
async fn claim_or_split(
    segs: &Arc<tokio::sync::Mutex<Vec<SegmentState>>>,
    active: &Arc<tokio::sync::Mutex<std::collections::HashSet<usize>>>,
    part_dir: &Path,
    url: &str,
    etag: &Arc<str>,
    total: u64,
    ctrl: &Controls,
) -> Option<usize> {
    let mut s = segs.lock().await;
    let mut act = active.lock().await;
    let remaining = |sg: &SegmentState| (sg.end + 1).saturating_sub(sg.start + sg.done);
    for (i, sg) in s.iter().enumerate() {
        if remaining(sg) > 0 && !act.contains(&i) {
            act.insert(i);
            return Some(i);
        }
    }
    let mut victim: Option<(usize, u64)> = None;
    for (i, sg) in s.iter().enumerate() {
        if act.contains(&i) {
            if let Some(np) = split_point(sg.start, sg.end, sg.done) {
                let tail = sg.end - np;
                if victim.is_none_or(|(_, b)| tail > b) {
                    victim = Some((i, tail));
                }
            }
        }
    }
    let (vi, _) = victim?;
    let old_end = s[vi].end;
    let np = split_point(s[vi].start, old_end, s[vi].done)?;
    s[vi].end = np;
    let new_idx = s.len();
    s.push(SegmentState {
        start: np + 1,
        end: old_end,
        done: 0,
    });
    act.insert(new_idx);
    let snapshot: Vec<SegmentState> = s
        .iter()
        .map(|x| SegmentState {
            start: x.start,
            end: x.end,
            done: x.done,
        })
        .collect();
    {
        let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
        *pg = snapshot
            .iter()
            .map(|x| (x.done, x.end.saturating_add(1).saturating_sub(x.start)))
            .collect();
    }
    drop(act);
    drop(s);
    let et_opt = if etag.is_empty() {
        None
    } else {
        Some(etag.to_string())
    };
    save_state(part_dir, url, &et_opt, total, &snapshot);
    Some(new_idx)
}

/// Download one claimed segment index until its (possibly shrunk) end.
/// On split-shrink: only bytes up to current end counted/committed; tail
/// bytes in part file discarded at assemble, re-downloaded by thief.
#[allow(clippy::too_many_arguments)]
async fn run_segment(
    i: usize,
    segs: &Arc<tokio::sync::Mutex<Vec<SegmentState>>>,
    client: &reqwest::Client,
    task: &DownloadTask,
    part_dir: &Path,
    etag: &Arc<str>,
    total: u64,
    ctrl: &Arc<Controls>,
) -> Result<(), String> {
    let mut attempt = 0;
    loop {
        let (start, end, done) = {
            let s = segs.lock().await;
            let sg = &s[i];
            (sg.start, sg.end, sg.done)
        };
        if done > end - start {
            return Ok(());
        }
        let range_start = start + done;
        if range_start > end {
            return Ok(());
        }
        match download_range(
            client,
            task,
            range_start,
            end,
            part_dir.join(format!("part{i}")).as_path(),
            ctrl,
            done,
            Some(i),
        )
        .await
        {
            Ok(written) => {
                let keep = {
                    let s = segs.lock().await;
                    let sg = &s[i];
                    (sg.end + 1).saturating_sub(start + done).min(written)
                };
                {
                    let mut s = segs.lock().await;
                    s[i].done = (s[i].done + keep).min(s[i].end + 1 - s[i].start);
                }
                let snapshot = {
                    let s = segs.lock().await;
                    s.iter()
                        .map(|x| SegmentState {
                            start: x.start,
                            end: x.end,
                            done: x.done,
                        })
                        .collect::<Vec<_>>()
                };
                {
                    let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                    *pg = snapshot
                        .iter()
                        .map(|x| (x.done, x.end.saturating_add(1).saturating_sub(x.start)))
                        .collect();
                }
                let et_opt = if etag.is_empty() {
                    None
                } else {
                    Some(etag.to_string())
                };
                save_state(part_dir, &task.url, &et_opt, total, &snapshot);
                if keep < written {
                    continue;
                }
                return Ok(());
            }
            Err(e) => {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err(e);
                }
                attempt += 1;
                if attempt > 5 {
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    500 * (1 << attempt.min(4)),
                ))
                .await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_range(
    client: &reqwest::Client,
    task: &DownloadTask,
    start: u64,
    end: u64,
    dest: &Path,
    ctrl: &Controls,
    prior: u64,
    part_index: Option<usize>,
) -> Result<u64, String> {
    let resp = client
        .get(&task.url)
        .headers(headers_for(task))
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 206 {
        return Err(format!("expected 206 got {}", resp.status()));
    }
    // Truncate to caller-known done: drops bytes from interrupted attempt.
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dest)
        .await
        .map_err(|e| e.to_string())?;
    file.set_len(prior).await.map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(prior))
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream();
    let mut n: u64 = 0;
    let mut err: Option<String> = None;
    loop {
        let chunk = match next_chunk(&mut stream, ctrl).await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        match chunk {
            Ok(chunk) => {
                throttle(ctrl, chunk.len() as u64).await;
                match file.write_all(&chunk).await {
                    Ok(()) => {
                        n += chunk.len() as u64;
                        ctrl.downloaded
                            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        if let Some(pi) = part_index {
                            let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(part) = pg.get_mut(pi) {
                                part.0 = (prior + n).min(part.1);
                            }
                        }
                    }
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
            Err(e) => {
                err = Some(e.to_string());
                break;
            }
        }
    }
    if err.is_none() {
        if let Err(e) = file.flush().await {
            err = Some(e.to_string());
        }
    }
    // Live-counted bytes roll back on failure: retry truncates part to `prior` and recounts.
    if let Some(e) = err {
        ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
        if let Some(pi) = part_index {
            if let Some(part) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).get_mut(pi) {
                part.0 = prior.min(part.1);
            }
        }
        return Err(e);
    }
    let expected = end - start + 1;
    if n != expected {
        return Err(format!("short range: got {n} expected {expected}"));
    }
    Ok(n)
}

async fn finish_check(dest: &Path, task: &DownloadTask) -> Result<(), String> {
    if let Some(want) = &task.sha256 {
        let mut f = tokio::fs::File::open(dest)
            .await
            .map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let n = f.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(want) {
            // Checksum auto-repair: quarantine corrupt output as `<name>.bad`
            // (overwrite prior .bad) so retry re-downloads instead of
            // resuming/verifying same corrupt bytes. Error still propagates:
            // app retry policy requeues, parts already cleaned by caller.
            let bad = PathBuf::from(format!("{}.bad", dest.display()));
            let _ = tokio::fs::remove_file(&bad).await;
            let _ = tokio::fs::rename(dest, &bad).await;
            return Err(format!(
                "sha256 mismatch: got {got} want {want} (quarantined as {})",
                bad.display()
            ));
        }
    }
    Ok(())
}

pub type MediaTracks = Vec<(String, Vec<(Option<String>, String)>)>;
/// Download list of media segments (DASH/HLS) and concatenate. Separate tracks muxed via ffmpeg if available.
/// Prior-run segment lengths recorded in `tracks.json` (beside `seg-*.bin`
/// caches). Legacy snapshots stored only `(kind, count)` per track; those
/// parse to counts-only and yield no lengths (distrust cache, refetch).
/// Fresh snapshots append per-segment on-disk lengths as a third tuple
/// element, keyed `seg-{kind}-{i}`.
type SegLenSnapshot = std::collections::BTreeMap<String, u64>;

fn seg_len_key(kind: &str, i: usize) -> String {
    format!("seg-{kind}-{i}")
}

/// Load recorded segment length, if snapshot has one for this segment.
/// Missing entry / missing / unparsable snapshot = no recorded length.
fn recorded_seg_len(snapshot: &SegLenSnapshot, kind: &str, i: usize) -> Option<u64> {
    snapshot.get(&seg_len_key(kind, i)).copied()
}

/// Cache trustworthy only when it exists, is non-empty, and (when snapshot
/// recorded a length) matches it exactly. Length-less legacy cache = poison
/// risk (partial write from killed run), so caller refetches.
fn cache_trusted(cache: &Path, recorded: Option<u64>) -> bool {
    let Ok(md) = std::fs::metadata(cache) else {
        return false;
    };
    let len = md.len();
    if len == 0 {
        return false;
    }
    match recorded {
        Some(want) => len == want,
        None => false,
    }
}

/// Serialize tracks.json: `{"tracks": [[kind, count]], "seg_lens":
/// [[seg_key, len]]}`. `parse_seg_snapshot` also accepts legacy bare
/// `[[kind, count]]` (no lengths -> caches distrusted).
fn snapshot_json(tracks: &MediaTracks, lens: &SegLenSnapshot) -> String {
    let tracks_v: Vec<serde_json::Value> = tracks
        .iter()
        .map(|(k, s)| serde_json::json!([k, s.len()]))
        .collect();
    let lens_v: Vec<serde_json::Value> = lens
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();
    serde_json::json!({"tracks": tracks_v, "seg_lens": lens_v}).to_string()
}

/// Parse tracks.json in either form: legacy bare array `[[kind, count]]`
/// (no lengths) or object `{"tracks": [...], "seg_lens": [[key, len]]}`.
fn parse_seg_snapshot(text: &str) -> SegLenSnapshot {
    let mut out = SegLenSnapshot::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let Some(arr) = v.get("seg_lens").and_then(|x| x.as_array()) else {
        return out;
    };
    for e in arr {
        if let (Some(k), Some(n)) = (
            e.get(0).and_then(|x| x.as_str()),
            e.get(1).and_then(|x| x.as_u64()),
        ) {
            out.insert(k.to_string(), n);
        }
    }
    out
}

pub async fn download_media(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<Controls>,
    tracks: MediaTracks,
) -> Result<DownloadTask, String> {
    // tracks: (kind, [(init, segment_url)...]) — one segment list per track, init repeated per entry ignored except first
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let mut task = task;
    let work = task
        .dest_dir
        .join(format!(".downloadcraft-media-{}", task.id));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    // Manifest snapshot BEFORE fetch: kill-resume reuses seg caches below.
    // Record prior completed segment lengths first (cache-size validation);
    // snapshot rewritten with fresh lengths after each segment completes.
    let mut known_lens: SegLenSnapshot = std::fs::read_to_string(work.join("tracks.json"))
        .map(|t| parse_seg_snapshot(&t))
        .unwrap_or_default();
    let _ = std::fs::write(
        work.join("tracks.json"),
        snapshot_json(&tracks, &known_lens),
    );
    let mut track_files: Vec<PathBuf> = Vec::new();
    let mut subtitle_files: Vec<PathBuf> = Vec::new();
    // Segment sizes unknown upfront: progress in bytes, task.total stays None.
    let mut done_bytes: u64 = 0;
    task.total = None;
    task.downloaded = 0;
    ctrl.downloaded.store(0, Ordering::Relaxed);
    {
        let total_segs: u64 = tracks.iter().map(|(_, sg)| sg.len() as u64).sum();
        ctrl.parts_are_segments.store(true, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(0, total_segs)];
    }
    for (kind, segs) in tracks.iter() {
        let out = work.join(format!("track-{kind}.bin"));
        {
            let mut file = tokio::fs::File::create(&out)
                .await
                .map_err(|e| e.to_string())?;
            let mut seen_init: Option<String> = None;
            for (i, (init, seg_url)) in segs.iter().enumerate() {
                wait_not_paused(&ctrl).await?;
                // Kill-resume: cached segment file from prior run is replayed, not refetched.
                let cache = work.join(format!("seg-{kind}-{i}.bin"));
                let seg_result = async {
                    if let Some(init) = init {
                        if seen_init.as_deref() != Some(init.as_str()) {
                            seen_init = Some(init.clone());
                            done_bytes +=
                                fetch_into_inner(&client, &task, init, &mut file, &ctrl).await?;
                        }
                    }
                    // Replay only size-validated cache: missing/empty/length-
                    // mismatch (partial write from killed run) refetches instead
                    // of poisoning output. No recorded length = untrusted.
                    let recorded = recorded_seg_len(&known_lens, kind, i);
                    if !cache_trusted(&cache, recorded) {
                        let mut cf = tokio::fs::File::create(&cache)
                            .await
                            .map_err(|e| e.to_string())?;
                        // Streaming fetch counts live progress; undo that count here —
                        // append below counts segment exactly once (fetch or replay).
                        let n = fetch_into_inner(&client, &task, seg_url, &mut cf, &ctrl).await?;
                        ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
                        cf.flush().await.map_err(|e| e.to_string())?;
                        drop(cf);
                        // Record fresh length for next kill-resume validation.
                        if n > 0 {
                            known_lens.insert(seg_len_key(kind, i), n);
                            let _ = std::fs::write(
                                work.join("tracks.json"),
                                snapshot_json(&tracks, &known_lens),
                            );
                        }
                    }
                    let bytes = tokio::fs::read(&cache).await.map_err(|e| e.to_string())?;
                    if bytes.is_empty() {
                        return Err(format!("empty segment fetched: {seg_url}"));
                    }
                    done_bytes += bytes.len() as u64;
                    ctrl.downloaded
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    file.write_all(&bytes).await.map_err(|e| e.to_string())?;
                    Ok::<(), String>(())
                }
                .await;
                if seg_result.is_ok() {
                    let mut pg = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(first) = pg.first_mut() {
                        first.0 += 1;
                    }
                }
                if let Err(e) = seg_result {
                    if ctrl.cancelled.load(Ordering::Relaxed) || e == "cancelled" {
                        task.status = TaskStatus::Paused;
                        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
                        return Ok(task);
                    }
                    return Err(e);
                }
                task.downloaded = done_bytes;
                task.total = None;
            }
            file.flush().await.map_err(|e| e.to_string())?;
        }
        if kind == "text" {
            subtitle_files.push(out);
        } else {
            track_files.push(out);
        }
    }
    // Subtitle tracks land as sidecar next to final file (ffmpeg stream mapping for
    // arbitrary subtitle segment bytes is unreliable; sidecar is what players pick up).
    let incoming = track_files
        .first()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    let dest = dedup_dest(&task.dest_dir.join(&task.filename), incoming);
    if let Some(name) = dest.file_name().map(|s| s.to_string_lossy().into_owned()) {
        if name != task.filename {
            task.filename = name;
        }
    }
    let text_url_ext = tracks
        .iter()
        .find(|(k, _)| k == "text")
        .and_then(|(_, s)| s.first())
        .map(|(_, u)| {
            u.rsplit('.')
                .next()
                .unwrap_or("vtt")
                .split(['?', '#'])
                .next()
                .unwrap_or("vtt")
                .to_string()
        });
    for (i, sub) in subtitle_files.iter().enumerate() {
        let stem = task
            .filename
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(&task.filename);
        let ext = text_url_ext.clone().unwrap_or_else(|| "vtt".into());
        let suffix = if subtitle_files.len() > 1 {
            format!(".{i}")
        } else {
            String::new()
        };
        let _ = tokio::fs::copy(sub, task.dest_dir.join(format!("{stem}{suffix}.{ext}"))).await;
    }
    if track_files.is_empty() && !subtitle_files.is_empty() {
        // Subtitle-only download: sidecar IS the product.
        tokio::fs::copy(&subtitle_files[0], &dest)
            .await
            .map_err(|e| e.to_string())?;
    } else if track_files.len() == 1 {
        // Single track: raw concat renamed dest may be TS bytes under .mp4
        // name (unplayable). Remux to mp4 or rename .ts before Complete.
        match validate_output(&track_files[0], &task.filename) {
            Err(ValidateFail::TsInMp4) => {
                // Remux track file in place; if ffmpeg absent it becomes .ts.
                let fixed = ensure_mp4_or_ts(&track_files[0]).await?;
                let is_ts = fixed.extension().and_then(|e| e.to_str()) == Some("ts");
                let target = if is_ts {
                    dest.with_extension("ts")
                } else {
                    dest.clone()
                };
                tokio::fs::rename(&fixed, &target)
                    .await
                    .map_err(|e| e.to_string())?;
                if let Some(name) = target.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    task.filename = name;
                }
                let _ = tokio::fs::remove_dir_all(&work).await;
                finish_check(&target, &task).await?;
                task.status = TaskStatus::Complete;
                task.downloaded = done_bytes;
                task.total = None;
                return Ok(task);
            }
            Err(e) => {
                // Assembled segments: empty/manifest hard-fail; generic magic
                // mismatch tolerated (fMP4 concat not standalone container,
                // per-segment manifest guard already ran).
                let hard =
                    e.to_string().contains("empty") || e.to_string().contains("playlist/HTML");
                if hard {
                    let _ = tokio::fs::remove_dir_all(&work).await;
                    task.status = TaskStatus::Error;
                    task.error = format!("invalid output: {e}");
                    return Ok(task);
                }
                tokio::fs::rename(&track_files[0], &dest)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(_) => {
                tokio::fs::rename(&track_files[0], &dest)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    } else {
        // Try ffmpeg mux; fallback: keep first track and note error (muxer missing handled by caller docs)
        match mux_tracks(&track_files, &dest).await {
            Ok(()) => {}
            Err(e) => {
                // Fallback: rename video/first track so file exists, record error.
                // TS bytes must not stay under .mp4 name: rename dest to .ts.
                tokio::fs::copy(&track_files[0], &dest)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = tokio::fs::remove_dir_all(&work).await;
                if matches!(
                    validate_output(&dest, &task.filename),
                    Err(ValidateFail::TsInMp4)
                ) {
                    let ts_dest = dest.with_extension("ts");
                    if tokio::fs::rename(&dest, &ts_dest).await.is_ok() {
                        if let Some(name) = ts_dest
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                        {
                            task.filename = name;
                        }
                    }
                }
                task.status = TaskStatus::Error;
                task.error = format!("mux failed ({e}); kept first track only");
                task.downloaded = done_bytes;
                task.total = None;
                return Ok(task);
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    // Final gate: muxed/multi-track output must not be manifest/empty/wrong magic.
    // Subtitle-only dest is text sidecar copy: skip media magic there.
    if track_files.is_empty() && !subtitle_files.is_empty() {
        // subtitle-only: only non-empty check applies
        if matches!(validate_output(&dest, "x.bin"), Err(ValidateFail::Msg(_))) {
            task.status = TaskStatus::Error;
            task.error = "invalid output: subtitle file empty/missing".into();
            return Ok(task);
        }
    } else {
        match validate_output(&dest, &task.filename) {
            Ok(_) => {}
            Err(ValidateFail::TsInMp4) => {
                let fixed = ensure_mp4_or_ts(&dest).await?;
                if let Some(name) = fixed.file_name().map(|s| s.to_string_lossy().into_owned()) {
                    task.filename = name;
                }
                let final_dest = task.dest_dir.join(&task.filename);
                finish_check(&final_dest, &task).await?;
                task.status = TaskStatus::Complete;
                task.downloaded = done_bytes;
                task.total = None;
                return Ok(task);
            }
            Err(e) => {
                // See single-track note: only empty/manifest hard-fail for assembled media.
                let hard =
                    e.to_string().contains("empty") || e.to_string().contains("playlist/HTML");
                if hard {
                    task.status = TaskStatus::Error;
                    task.error = format!("invalid output: {e}");
                    return Ok(task);
                }
            }
        }
    }
    finish_check(&dest, &task).await?;
    task.status = TaskStatus::Complete;
    task.downloaded = done_bytes;
    task.total = None;
    Ok(task)
}

async fn fetch_into_inner(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    file: &mut tokio::fs::File,
    ctrl: &Controls,
) -> Result<u64, String> {
    let mut attempt = 0;
    let start = file
        .seek(std::io::SeekFrom::Current(0))
        .await
        .map_err(|e| e.to_string())?;
    loop {
        match fetch_once_counted_impl(client, task, url, file, ctrl).await {
            Ok(n) => return Ok(n),
            Err(e) => {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    return Err(e);
                }
                attempt += 1;
                if attempt > 4 {
                    return Err(e);
                }
                // Roll back partial bytes so retry does not duplicate/count twice.
                file.set_len(start).await.map_err(|x| x.to_string())?;
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|x| x.to_string())?;
                tokio::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
            }
        }
    }
}
async fn fetch_once_counted_impl(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    file: &mut tokio::fs::File,
    ctrl: &Controls,
) -> Result<u64, String> {
    let req = client.get(url).headers(headers_for(task));
    // byte-range segment stored in headers via special marker? HLS BYTERANGE passed through task headers Range by caller when needed.
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    let mut stream = resp.bytes_stream();
    let mut n = 0u64;
    // First-chunk guard: manifest/HTML bytes must never become media segments.
    let mut first = true;
    loop {
        let Some(chunk) = (match next_chunk(&mut stream, ctrl).await {
            Ok(v) => v,
            Err(e) => {
                ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
                return Err(e);
            }
        }) else {
            break;
        };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
                return Err(e.to_string());
            }
        };
        if first {
            first = false;
            if crate::stream::looks_like_manifest_bytes_pub(&chunk) {
                return Err(format!("segment is manifest/HTML, not media: {url}"));
            }
        }
        throttle(ctrl, chunk.len() as u64).await;
        if let Err(e) = file.write_all(&chunk).await {
            ctrl.downloaded.fetch_sub(n, Ordering::Relaxed);
            return Err(e.to_string());
        }
        n += chunk.len() as u64;
        ctrl.downloaded
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    Ok(n)
}

/// Fetch segment honoring optional byte range (HLS EXT-X-BYTERANGE).
pub async fn fetch_bytes(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    range: Option<(u64, Option<u64>)>,
    dest: &Path,
) -> Result<(), String> {
    fetch_bytes_counted(client, task, url, range, dest)
        .await
        .map(|_| ())
}
pub async fn fetch_bytes_counted(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
    range: Option<(u64, Option<u64>)>,
    dest: &Path,
) -> Result<u64, String> {
    let mut req = client.get(url).headers(headers_for(task));
    if let Some((len, off)) = range {
        let start = off.unwrap_or(0);
        // len==0 (hostile/garbled HLS byterange) underflows start+len-1: debug panic, release wrap.
        let end = start
            .checked_add(len)
            .and_then(|e| e.checked_sub(1))
            .ok_or("invalid byte range length 0")?;
        req = req.header("Range", format!("bytes={start}-{end}"));
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if crate::stream::looks_like_manifest_bytes_pub(&bytes) {
        return Err(format!("segment is manifest/HTML, not media: {url}"));
    }
    let n = bytes.len() as u64;
    tokio::fs::write(dest, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    Ok(n)
}

/// Output validation failure. `TsInMp4`: dest name is mp4-family but bytes
/// are MPEG-TS (first byte 0x47) — caller remuxes or renames to .ts.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateFail {
    Msg(String),
    TsInMp4,
    Empty,
    Playlist,
}
impl std::fmt::Display for ValidateFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidateFail::Msg(m) => write!(f, "{m}"),
            ValidateFail::TsInMp4 => write!(f, "output is TS, not mp4"),
            ValidateFail::Empty => write!(f, "output empty/missing"),
            ValidateFail::Playlist => write!(f, "output is playlist/HTML, not media"),
        }
    }
}

/// TS stream: 188-byte packets each start 0x47. Single leading 0x47 is not
/// enough: 'G' (GIF, text) also equals 0x47. Require a full packet plus a
/// sync byte at the next packet boundary when present.
pub fn is_ts(bytes: &[u8]) -> bool {
    bytes.len() >= 188
        && bytes[0] == 0x47
        && (bytes.len() == 188 || bytes[188] == 0x47)
        && (bytes.len() < 377 || bytes[376] == 0x47)
}

fn is_mp4_ext(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "mov" | "m4a")
}

const MEDIA_EXTS: &[&str] = &[
    "mp4", "m4a", "m4v", "mov", "mkv", "webm", "mp3", "ts", "m4s", "opus", "ogg", "flac", "wav",
    "aac",
];

fn is_media_ext(ext: &str) -> bool {
    MEDIA_EXTS.contains(&ext)
}

fn ext_of_str(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Validate finished output. Strict checks (empty, playlist/HTML body,
/// magic) ONLY for media-family dest (mp4,m4a,mov,mkv,webm,mp3,ts,m4s,
/// opus,ogg,flac,wav,aac) by dest or context filename extension. Generic
/// downloads always pass (Ok(len), missing file still errors) — legit
/// 0-byte/.html/legacy-.mov/ADTS-.aac files must not false-reject.
/// Pure: never renames. Returns file size.
pub fn validate_output(dest: &Path, filename_for_error_context: &str) -> Result<u64, ValidateFail> {
    validate_output_impl(dest, filename_for_error_context, false)
}

/// `strict=true`: force media checks regardless of extension. Post-mux
/// gate uses this — ffmpeg output under any name must be valid media,
/// any validate error there is a hard failure (corrupt output).
pub fn validate_output_strict(
    dest: &Path,
    filename_for_error_context: &str,
    strict: bool,
) -> Result<u64, ValidateFail> {
    if strict {
        validate_output_impl(dest, filename_for_error_context, true)
    } else {
        validate_output(dest, filename_for_error_context)
    }
}

fn validate_output_impl(
    dest: &Path,
    filename_for_error_context: &str,
    force_media: bool,
) -> Result<u64, ValidateFail> {
    let msg = |m: &str| ValidateFail::Msg(format!("{m} ({filename_for_error_context})"));
    // First 64KB is plenty for magic; whole-file read would be wasteful on
    // multi-GB outputs, size comes from metadata.
    use std::io::Read as _;
    let size = std::fs::metadata(dest).map_err(|_| ValidateFail::Empty)?.len();
    let ctx_ext = ext_of_str(filename_for_error_context);
    let dest_ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !force_media && !is_media_ext(&ctx_ext) && !is_media_ext(&dest_ext) {
        return Ok(size);
    }
    if size == 0 {
        return Err(ValidateFail::Empty);
    }
    let mut head = vec![0u8; 64 * 1024];
    let n = {
        let mut f = std::fs::File::open(dest)
            .map_err(|e| ValidateFail::Msg(format!("output empty/missing: {e}")))?;
        f.read(&mut head)
            .map_err(|e| ValidateFail::Msg(format!("output read: {e}")))?
    };
    head.truncate(n);
    let head = &head[..];
    let text = {
        let t = String::from_utf8_lossy(head);
        let t = t.trim_start_matches('\u{feff}');
        t.trim_start().to_string()
    };
    if text.starts_with("#EXTM3U")
        || text.starts_with("<MPD")
        || text.to_ascii_lowercase().starts_with("<metalink")
        || text.to_ascii_lowercase().starts_with("<!doctype html")
        || text.to_ascii_lowercase().starts_with("<html")
    {
        return Err(msg("output is playlist/HTML, not media"));
    }
    // Extension from filename context first: media track temp files are
    // `.bin` on disk but must validate against final dest extension.
    let ext = Path::new(filename_for_error_context)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| {
            dest.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
        });
    // TS-in-mp4 checked before generic magic: TS bytes are valid media,
    // just wrong container name — caller decides remux vs .ts rename.
    if is_mp4_ext(&ext) && is_ts(head) {
        return Err(ValidateFail::TsInMp4);
    }
    let ok = match ext.as_str() {
        "mp4" | "m4v" | "mov" | "m4a" => head.len() >= 8 && &head[4..8] == b"ftyp",
        "webm" | "mkv" => head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        "mp3" => head.starts_with(b"ID3") || (head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0),
        "ts" => is_ts(head),
        _ => true,
    };
    if !ok {
        return Err(msg("output magic bytes do not match extension"));
    }
    Ok(size)
}

/// Remux TS-in-mp4 dest to real mp4 via ffmpeg; on failure/absent ffmpeg,
/// rename dest to .ts (honest extension). Returns final path. Only called
/// after validate_output returned TsInMp4 (or dest known TS).
pub async fn ensure_mp4_or_ts(dest: &Path) -> Result<PathBuf, String> {
    let tmp = dest.with_extension("remux.mp4");
    match mux_tracks(&[dest.to_path_buf()], &tmp).await {
        Ok(()) => {
            std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
            Ok(dest.to_path_buf())
        }
        Err(_) => {
            let ts = dest.with_extension("ts");
            std::fs::rename(dest, &ts).map_err(|e| e.to_string())?;
            Ok(ts)
        }
    }
}

/// Quarantine bad output as `<name>.bad` (overwrite prior) then report Err.
pub fn quarantine_bad(dest: &Path, why: &ValidateFail) -> String {
    let bad = PathBuf::from(format!("{}.bad", dest.display()));
    let _ = std::fs::remove_file(&bad);
    let _ = std::fs::rename(dest, &bad);
    format!("invalid output: {why} (quarantined as {})", bad.display())
}

pub async fn mux_tracks(tracks: &[PathBuf], dest: &Path) -> Result<(), String> {
    // ffmpeg sidecar: PATH lookup (bundled ffmpeg next to exe preferred)
    let mut candidates = vec![PathBuf::from("ffmpeg")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            candidates.insert(
                0,
                d.join(if cfg!(windows) {
                    "ffmpeg.exe"
                } else {
                    "ffmpeg"
                }),
            );
        }
    }
    let mut last_err = String::new();
    for cand in &candidates {
        let mut cmd = tokio::process::Command::new(cand);
        crate::platform::no_window_tokio(&mut cmd);
        cmd.arg("-y");
        for t in tracks {
            cmd.args(["-i"]).arg(t);
        }
        cmd.args(["-c", "copy"]);
        cmd.arg(dest);
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        match cmd.output().await {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => {
                last_err = String::from_utf8_lossy(&o.stderr)
                    .chars()
                    .take(300)
                    .collect()
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tmp_file(tag: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wc-validate-{}-{}", std::process::id(), tag));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
    #[test]
    fn validate_output_magic_table() {
        let mut mp4 = vec![0u8; 4];
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0u8; 16]);
        let p = tmp_file("mp4", "a.mp4", &mp4);
        assert!(validate_output(&p, "a.mp4").is_ok(), "valid ftyp mp4 ok");
        let p = tmp_file("m3u", "b.mp4", b"#EXTM3U\n#EXT-X-VERSION:3\n");
        assert!(
            matches!(validate_output(&p, "b.mp4"), Err(ValidateFail::Msg(_))),
            "playlist in mp4 err"
        );
        let mut ts = vec![0x47u8];
        ts.extend_from_slice(&[0u8; 187]);
        let p = tmp_file("ts", "c.mp4", &ts);
        assert_eq!(
            validate_output(&p, "c.mp4"),
            Err(ValidateFail::TsInMp4),
            "TS in mp4 sentinel"
        );
        let p = tmp_file("empty", "d.mp4", b"");
        assert!(
            matches!(validate_output(&p, "d.mp4"), Err(ValidateFail::Msg(_))),
            "empty err"
        );
        let p = tmp_file("webm", "e.webm", &[0x1A, 0x45, 0xDF, 0xA3, 1, 2]);
        assert!(validate_output(&p, "e.webm").is_ok(), "webm magic ok");
        let p = tmp_file("mp3", "f.mp3", b"ID3xxxx");
        assert!(validate_output(&p, "f.mp3").is_ok(), "mp3 ID3 ok");
        let p = tmp_file("badmp4", "g.mp4", b"not-media-at-all......");
        assert!(
            matches!(validate_output(&p, "g.mp4"), Err(ValidateFail::Msg(_))),
            "bad mp4 magic err"
        );
        let p = tmp_file("html", "h.mp4", b"<html><body>x</body></html>");
        assert!(
            matches!(validate_output(&p, "h.mp4"), Err(ValidateFail::Msg(_))),
            "HTML err"
        );
    }
    #[test]
    fn seg_snapshot_and_cache_trust() {
        // Legacy [[kind,count]] snapshot: parses, yields no lengths.
        let legacy = parse_seg_snapshot(r#"[["video",2],["audio",1]]"#);
        assert!(legacy.is_empty());
        assert_eq!(recorded_seg_len(&legacy, "video", 0), None);
        // Fresh object snapshot round-trips per-segment lengths.
        let tracks: MediaTracks = vec![
            ("video".into(), vec![(None, "s0".into()), (None, "s1".into())]),
            ("audio".into(), vec![(None, "a0".into())]),
        ];
        let mut lens = SegLenSnapshot::new();
        lens.insert(seg_len_key("video", 0), 100);
        lens.insert(seg_len_key("video", 1), 50);
        let back = parse_seg_snapshot(&snapshot_json(&tracks, &lens));
        assert_eq!(recorded_seg_len(&back, "video", 0), Some(100));
        assert_eq!(recorded_seg_len(&back, "video", 1), Some(50));
        assert_eq!(recorded_seg_len(&back, "audio", 0), None);
        // Garbage snapshot = no lengths, never panics.
        assert!(parse_seg_snapshot("not json").is_empty());
        // Trust: exact recorded len ok; mismatch / empty / missing /
        // no-recorded-length all untrusted.
        let dir = std::env::temp_dir().join(format!("wc-segtrust-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let good = dir.join("seg-video-0.bin");
        std::fs::write(&good, vec![7u8; 100]).unwrap();
        assert!(cache_trusted(&good, Some(100)));
        assert!(!cache_trusted(&good, Some(99)), "partial cache rejected");
        assert!(!cache_trusted(&good, None), "legacy cache untrusted");
        let empty = dir.join("seg-video-1.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(!cache_trusted(&empty, Some(0)));
        assert!(!cache_trusted(&empty, None));
        assert!(!cache_trusted(&dir.join("nope.bin"), Some(10)));
    }
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
    #[test]
    fn split_point_halves_remaining() {
        let kb = 1024u64;
        assert_eq!(split_point(0, 1024 * kb - 1, 0), Some(512 * kb - 1));
        assert_eq!(
            split_point(0, 512 * kb - 2, 0),
            None,
            "remaining < 512KB never splits"
        );
        assert_eq!(
            split_point(0, 600 * kb - 1, 200 * kb),
            None,
            "done counts toward remaining"
        );
        assert_eq!(
            split_point(100, 100 + 1024 * kb - 1, 512 * kb),
            Some(100 + 768 * kb - 1)
        );
        assert_eq!(split_point(0, 10, 20), None);
        // both halves >= MIN_SPLIT at exact threshold
        assert_eq!(split_point(0, 2 * MIN_SPLIT - 1, 0), Some(MIN_SPLIT - 1));
    }
    #[test]
    fn cd_rfc5987_utf8() {
        assert_eq!(
            parse_content_disposition(
                "attachment; filename*=UTF-8''%E2%82%AC%20rates.pdf"
            )
            .as_deref(),
            Some("€ rates.pdf")
        );
    }
    #[test]
    fn cd_star_wins_over_plain() {
        assert_eq!(
            parse_content_disposition(
                "attachment; filename=\"plain.zip\"; filename*=UTF-8''star%20name.zip"
            )
            .as_deref(),
            Some("star name.zip")
        );
    }
    #[test]
    fn cd_plain_quoted_and_unquoted() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"my file.zip\"").as_deref(),
            Some("my file.zip")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=report.pdf").as_deref(),
            Some("report.pdf")
        );
    }
    #[test]
    fn cd_sanitizes_traversal_and_controls() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"../../evil.exe\"").as_deref(),
            Some("evil.exe")
        );
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''..%2F..%2Fevil.exe")
                .as_deref(),
            Some("evil.exe")
        );
        assert_eq!(parse_content_disposition("attachment"), None);
        assert_eq!(parse_content_disposition("attachment; filename=\"\""), None);
    }
    #[test]
    fn cd_latin1_star() {
        // 0xE9 = é in ISO-8859-1
        assert_eq!(
            parse_content_disposition("attachment; filename*=ISO-8859-1''caf%E9.txt")
                .as_deref(),
            Some("café.txt")
        );
    }
    #[test]
    fn adopt_only_generic_names() {
        let cd = Some("attachment; filename=\"real-name.zip\"");
        assert_eq!(
            adopt_server_filename("download", cd, None).as_deref(),
            Some("real-name.zip")
        );
        assert_eq!(
            adopt_server_filename("", cd, None).as_deref(),
            Some("real-name.zip")
        );
        assert_eq!(
            adopt_server_filename("index", cd, None).as_deref(),
            Some("real-name.zip")
        );
        // User-renamed specific name never overridden.
        assert_eq!(adopt_server_filename("my-rename.zip", cd, None), None);
        assert_eq!(adopt_server_filename("holiday.mp4", cd, None), None);
        // Generic without extension + no CD: MIME fallback.
        assert_eq!(
            adopt_server_filename("download", None, Some("video/mp4")).as_deref(),
            Some("download.mp4")
        );
        assert_eq!(
            adopt_server_filename("download.bin", None, Some("video/mp4")),
            None,
            "has extension already"
        );
        assert_eq!(adopt_server_filename("download", None, None), None);
    }
    #[test]
    fn generic_detection() {
        assert!(is_generic_filename(""));
        assert!(is_generic_filename("download"));
        assert!(is_generic_filename("download.bin"));
        assert!(is_generic_filename("index.html"));
        assert!(is_generic_filename("noext"));
        assert!(!is_generic_filename("report.pdf"));
    }
    #[test]
    fn dedup_picks_suffix_on_size_conflict() {
        let dir = std::env::temp_dir().join(format!("wc-dedup-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join("file.zip");
        std::fs::write(&dest, b"12345").unwrap(); // 5 bytes
        assert_eq!(dedup_dest(&dest, Some(5)), dest, "same size keeps dest");
        assert_eq!(
            dedup_dest(&dest, Some(99)),
            dir.join("file (1).zip"),
            "different size dedups"
        );
        std::fs::write(dir.join("file (1).zip"), b"x").unwrap();
        assert_eq!(dedup_dest(&dest, Some(99)), dir.join("file (2).zip"));
        assert_eq!(
            dedup_dest(&dir.join("absent.zip"), Some(1)),
            dir.join("absent.zip")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
