use crate::engine;
use crate::hls;
use crate::model::*;
use crate::mpd::*;
use std::sync::Arc;

enum Manifest {
    Dash,
    Hls,
    Metalink,
    File,
}

fn is_metalink_url(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".meta4") || path.ends_with(".metalink")
}

/// `.nzb` URL/local path, `nzb://` wrapper, or explicit `x-nzb` header marker.
fn is_nzb_url(task: &DownloadTask) -> bool {
    if task.headers.contains_key("x-nzb") {
        return true;
    }
    let url = task.url.trim();
    if url.starts_with("nzb://") {
        return true;
    }
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://"))
        && std::path::Path::new(url).is_file()
    {
        return url.to_ascii_lowercase().ends_with(".nzb");
    }
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase()
        .ends_with(".nzb")
}

/// Download protocol, derived from task URL/headers. Label source for list UI:
/// mirrors dispatch order in `download_url_with_settings`.
/// ponytail: derived, not persisted — metalink primary-mirror rewrite and
/// Content-Type-only streams lose exact kind after run; persist field if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtoKind {
    Torrent,
    Ftp,
    Ftps,
    Sftp,
    Nntp,
    Metalink,
    Dash,
    Hls,
    YtDlp,
    #[default]
    Http,
    File,
}

impl ProtoKind {
    pub fn label(self) -> &'static str {
        match self {
            ProtoKind::Torrent => "TORRENT",
            ProtoKind::Ftp => "FTP",
            ProtoKind::Ftps => "FTPS",
            ProtoKind::Sftp => "SFTP",
            ProtoKind::Nntp => "NNTP",
            ProtoKind::Metalink => "METALINK",
            ProtoKind::Dash => "DASH",
            ProtoKind::Hls => "HLS",
            ProtoKind::YtDlp => "YT-DLP",
            ProtoKind::Http => "HTTP",
            ProtoKind::File => "FILE",
        }
    }
    pub fn full_name(self) -> &'static str {
        match self {
            ProtoKind::Torrent => "BitTorrent",
            ProtoKind::Ftp => "FTP",
            ProtoKind::Ftps => "FTPS",
            ProtoKind::Sftp => "SFTP/SSH",
            ProtoKind::Nntp => "NNTP (NZB)",
            ProtoKind::Metalink => "Metalink",
            ProtoKind::Dash => "DASH stream",
            ProtoKind::Hls => "HLS stream",
            ProtoKind::YtDlp => "Video page (yt-dlp)",
            ProtoKind::Http => "HTTP/HTTPS",
            ProtoKind::File => "Local file",
        }
    }
}

pub fn proto_kind(task: &DownloadTask) -> ProtoKind {
    let lower = task.url.trim().to_ascii_lowercase();
    if crate::torrent::is_torrent_task(task) {
        return ProtoKind::Torrent;
    }
    if lower.starts_with("ftps://") || lower.starts_with("ftpsi://") {
        return ProtoKind::Ftps;
    }
    if lower.starts_with("ftp://") {
        return ProtoKind::Ftp;
    }
    if lower.starts_with("sftp://") || lower.starts_with("ssh://") {
        return ProtoKind::Sftp;
    }
    if is_nzb_url(task) {
        return ProtoKind::Nntp;
    }
    if is_metalink_url(&task.url) {
        return ProtoKind::Metalink;
    }
    if lower.contains(".mpd") {
        return ProtoKind::Dash;
    }
    if lower.contains(".m3u8") {
        return ProtoKind::Hls;
    }
    if task.headers.contains_key("x-ytdlp") || crate::ytdlp::is_video_page(&task.url) {
        return ProtoKind::YtDlp;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return ProtoKind::Http;
    }
    ProtoKind::File
}

/// Probe Content-Type only (headers, body dropped). On any error/non-success -> File.
/// ponytail: header-only probe misses manifests served as octet-stream/text; body-peek + reuse would need passing fetched text into download_dash/download_hls — add when such servers matter.
async fn probe_manifest(client: &reqwest::Client, task: &DownloadTask) -> Manifest {
    let mut req = client.get(&task.url);
    if !task.referrer.is_empty() {
        req = req.header("Referer", &task.referrer);
    }
    if !task.cookies.is_empty() {
        req = req.header("Cookie", &task.cookies);
    }
    for (k, v) in &task.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let Ok(resp) = req.send().await else {
        return Manifest::File;
    };
    if !resp.status().is_success() {
        return Manifest::File;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ct = ct.split(';').next().unwrap_or("").trim();
    // IDM MIME map (REA round 6): video/vnd.mpeg.dash.mpd, application/vnd.apple.mpegurl.
    if ct.contains("application/metalink+xml") {
        Manifest::Metalink
    } else if ct.contains("dash") || ct.contains("mpd") {
        Manifest::Dash
    } else if ct.contains("mpegurl") || ct.contains("m3u") {
        Manifest::Hls
    } else {
        Manifest::File
    }
}

/// Resolve URL to download task(s): URL .mpd/.m3u8 hint first, else probe Content-Type, else plain HTTP.
pub async fn download_url(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    download_url_with_settings(client, task, ctrl, &Settings::default()).await
}

/// Same dispatch with live app `Settings` (NNTP server config lives there).
pub async fn download_url_with_settings(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<engine::Controls>,
    settings: &Settings,
) -> Result<DownloadTask, String> {
    let lower = task.url.to_ascii_lowercase();
    // BitTorrent before HTTP probe: magnet has no HTTP form, .torrent probe would download metadata as file.
    if crate::torrent::is_torrent_task(&task) {
        return crate::torrent::download_torrent(client, task, ctrl, settings).await;
    }
    if lower.starts_with("ftp://")
        || lower.starts_with("ftps://")
        || lower.starts_with("ftpsi://") {
        return crate::ftp::download_ftp(task, ctrl).await;
    }
    if lower.starts_with("sftp://") || lower.starts_with("ssh://") {
        return crate::sftp::download_sftp(task, ctrl).await;
    }
    if is_nzb_url(&task) {
        let mut task = task;
        // nzb://<real-url> wrapper: strip scheme so fetch sees the NZB URL/path.
        if let Some(rest) = task.url.trim().strip_prefix("nzb://") {
            task.url = rest.to_string();
        }
        let cfg = crate::nntp::NntpConfig::from_settings(settings, &task);
        return crate::nntp::download_nzb(client, task, ctrl, &cfg).await;
    }
    if is_metalink_url(&task.url) {
        return download_metalink(client, task, ctrl).await;
    }
    // Explicit yt-dlp choice (x-ytdlp header from dialog) beats manifest dispatch:
    // without this, manifest URLs never reach yt-dlp. Explicit choice fails loud
    // (no silent native fallback: user picked engine deliberately).
    if task.headers.contains_key("x-ytdlp") {
        return crate::ytdlp::download_with_ytdlp(task, ctrl).await;
    }
    if lower.contains(".mpd") {
        return download_dash(client, task, ctrl, false).await;
    }
    if lower.contains(".m3u8") {
        return download_hls(client, task, ctrl).await;
    }
    // Video-page URLs (YouTube etc): yt-dlp extracts formats; native probe would
    // download HTML page. Fall through to native flow on any yt-dlp failure.
    if crate::ytdlp::is_video_page(&task.url) {
        // No native fallback: probe would download page HTML as fake file.
        return crate::ytdlp::download_with_ytdlp(task, ctrl)
            .await
            .map_err(|e| format!("yt-dlp failed: {e}"));
    }
    match probe_manifest(&client, &task).await {
        Manifest::Metalink => download_metalink(client, task, ctrl).await,
        Manifest::Dash => download_dash(client, task, ctrl, false).await,
        Manifest::Hls => download_hls(client, task, ctrl).await,
        Manifest::File => engine::download_http(client, task, ctrl).await,
    }
}

/// Fetch + parse .meta4/.metalink, take first <file>: primary url[0], rest
/// are mirrors (existing engine probe failover), sha256 feeds finish_check.
/// ponytail: first file only; multi-file .metalink would need task fan-out.
async fn download_metalink(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    // No silent fallback: bad fetch/parse/empty-url-list must error the task.
    let xml = fetch_text(&client, &task, &task.url)
        .await
        .map_err(|e| format!("metalink fetch failed: {e}"))?;
    let files = crate::metalink::parse_metalink(&xml)?;
    let f = files
        .into_iter()
        .next()
        .ok_or("metalink has no <file> entries")?;
    let mut urls = f.urls.into_iter();
    let Some(primary) = urls.next() else {
        return Err(format!("metalink file '{}' has no <url>", f.name));
    };
    task.url = primary;
    task.mirrors = urls.collect();
    if !f.name.is_empty() {
        task.filename = sanitize_filename(&f.name);
    }
    // Hash wins; keep caller-supplied sha256 when manifest has none.
    if f.sha256.is_some() {
        task.sha256 = f.sha256;
    }
    engine::download_http(client, task, ctrl).await
}

async fn fetch_text(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
) -> Result<String, String> {
    let mut req = client.get(url);
    if !task.referrer.is_empty() {
        req = req.header("Referer", &task.referrer);
    }
    if !task.cookies.is_empty() {
        req = req.header("Cookie", &task.cookies);
    }
    for (k, v) in &task.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

/// DASH download. If once=true, single snapshot pass (used for dynamic refusal fallback test).
pub async fn download_dash(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<engine::Controls>,
    _once: bool,
) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let xml = fetch_text(&client, &task, &task.url).await?;
    if is_protected(&xml) {
        task.status = TaskStatus::Protected;
        task.error = "Protected content (DRM) is not supported".into();
        return Ok(task);
    }
    let mut parsed = parse_mpd(&xml, &base)?;
    if parsed.mpd_type == "dynamic" {
        // Live DASH: poll until stop condition (cancel) or END via repeated fetch collecting new segments.
        // ponytail: IDM live behavior unverified without live fixture against IDM; clone polls minimumUpdatePeriod and accumulates unique segments until cancelled.
        let mut seen: engine::MediaTracks = Vec::new();
        let poll = parsed.minimum_update_period_secs.unwrap_or(2.0).max(1.0);
        // first snapshot
        append_reps(&parsed, &mut seen, &task);
        loop {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(poll)).await;
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let xml2 = fetch_text(&client, &task, &task.url).await?;
            if is_protected(&xml2) {
                task.status = TaskStatus::Protected;
                task.error = "Protected content (DRM) is not supported".into();
                return Ok(task);
            }
            parsed = parse_mpd(&xml2, &base)?;
            let before: usize = seen.iter().map(|(_, s)| s.len()).sum();
            append_reps(&parsed, &mut seen, &task);
            let after: usize = seen.iter().map(|(_, s)| s.len()).sum();
            if after == before {
                // No new segments for one poll cycle: treat as ended snapshot and assemble what we have.
                break;
            }
        }
        if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            let mut t = task;
            t.status = TaskStatus::Paused;
            return Ok(t);
        }
        if track_is_empty(&seen) {
            return Err("live MPD produced no segments".into());
        }
        return engine::download_media(client, task, ctrl, seen).await;
    }
    let mut tracks = Vec::new();
    append_reps(&parsed, &mut tracks, &task);
    if tracks.iter().all(|(_, s)| s.is_empty()) {
        return Err("MPD produced no segment URLs".into());
    }
    engine::download_media(client, task, ctrl, tracks).await
}

fn track_is_empty(t: &engine::MediaTracks) -> bool {
    t.iter().all(|(_, s)| s.is_empty())
}

fn append_reps(parsed: &Mpd, tracks: &mut engine::MediaTracks, task: &DownloadTask) {
    // Select representation: task.headers x-rep-id override else highest bandwidth video + highest audio + first subtitle per spec default.
    let want_rep = task.headers.get("x-rep-id").cloned();
    // Group by content kind, merged in Period order: parse_mpd already concatenates periods as separate sets in order;
    // for multi-period same-kind sets, append segments in order into same track.
    for set in &parsed.sets {
        let kind = if set.mime_type.starts_with("video") || set.content_type == "video" {
            "video"
        } else if set.mime_type.starts_with("audio") || set.content_type == "audio" {
            "audio"
        } else if set.mime_type.contains("text") || set.content_type == "text" {
            "text"
        } else if set.mime_type.contains('/') {
            set.mime_type.split('/').next().unwrap_or("media")
        } else {
            "media"
        };
        let reps = &set.representations;
        if reps.is_empty() {
            continue;
        }
        let chosen = if let Some(w) = &want_rep {
            reps.iter().find(|r| &r.id == w).unwrap_or(&reps[0])
        } else {
            reps.iter().max_by_key(|r| r.bandwidth).unwrap_or(&reps[0])
        };
        // SegmentBase: single file holds init+index+media. Emit (None, file) — engine
        // fetching init_url too would download whole file twice (mpd.rs keeps init_url=None there).
        let segs: Vec<(Option<String>, String)> = if chosen.segment_base {
            chosen
                .segment_urls
                .iter()
                .map(|u| (None, u.clone()))
                .collect()
        } else {
            chosen
                .segment_urls
                .iter()
                .map(|u| (chosen.init_url.clone(), u.clone()))
                .collect()
        };
        if let Some(existing) = tracks.iter_mut().find(|(k, _)| k == kind) {
            for s in segs {
                if !existing.1.contains(&s) {
                    existing.1.push(s);
                }
            }
        } else {
            tracks.push((kind.to_string(), segs));
        }
    }
}

pub async fn download_hls(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let text = fetch_text(&client, &task, &task.url).await?;
    if let Some(m) = hls::key_method(&text) {
        if m != "AES-128" && m != "NONE" {
            task.status = TaskStatus::Protected;
            task.error = format!("Encrypted HLS METHOD={m} is not supported");
            return Ok(task);
        }
    }
    if text.contains("#EXT-X-STREAM-INF") {
        let master = hls::parse_master(&text, &base);
        if master.variants.is_empty() {
            return Err("HLS master has no variants".into());
        }
        // variant selection: x-variant-index header or highest bandwidth
        let idx: usize = task
            .headers
            .get("x-variant-index")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                master
                    .variants
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, v)| v.bandwidth)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let variant = &master.variants[idx.min(master.variants.len() - 1)];
        let mut t2 = task.clone();
        t2.url = variant.uri.clone();
        // AUDIO rendition in same group; DEFAULT=YES preferred.
        let audio_uri = master
            .renditions
            .iter()
            .find(|r| {
                r.kind == "AUDIO"
                    && r.default
                    && !variant.audio_group.is_empty()
                    && r.group_id == variant.audio_group
            })
            .and_then(|r| r.uri.clone())
            .or_else(|| {
                master
                    .renditions
                    .iter()
                    .find(|r| {
                        r.kind == "AUDIO"
                            && (variant.audio_group.is_empty() || r.group_id == variant.audio_group)
                    })
                    .and_then(|r| r.uri.clone())
            });
        let video_task = download_hls_media(client.clone(), t2.clone(), ctrl.clone()).await?;
        let Some(au) = audio_uri.filter(|u| u != &variant.uri) else {
            return Ok(video_task);
        };
        // Video failed/protected: keep its status, skip audio.
        if video_task.status != TaskStatus::Complete {
            return Ok(video_task);
        }
        // Audio to hidden temp file, then mux into final dest. Mirror DASH: mux fail -> Error, keep video.
        let mut at = task.clone();
        at.url = au.clone();
        at.filename = format!(".{}-audio", task.filename);
        match download_hls_media(client.clone(), at, ctrl.clone()).await {
            Err(e) => {
                let mut out = video_task;
                out.status = TaskStatus::Error;
                out.error = format!("audio download failed: {e}");
                return Ok(out);
            }
            Ok(at2) if at2.status != TaskStatus::Complete => {
                let mut out = video_task;
                out.status = TaskStatus::Error;
                out.error = format!("audio download failed: {}", at2.error);
                return Ok(out);
            }
            Ok(_) => {}
        }
        let audio_file = task.dest_dir.join(format!(".{}-audio", task.filename));
        let video_file = task.dest_dir.join(&task.filename);
        // ffmpeg cannot mux in-place (input == output) and picks muxer by extension:
        // temp name must keep real extension or it cannot choose output format.
        let mux_out = task.dest_dir.join(format!(".mux-{}", task.filename));
        let result = engine::mux_tracks(&[video_file.clone(), audio_file.clone()], &mux_out)
            .await
            .and_then(|()| std::fs::rename(&mux_out, &video_file).map_err(|e| e.to_string()));
        let _ = std::fs::remove_file(&mux_out);
        let _ = std::fs::remove_file(&audio_file);
        match result {
            Ok(()) => return Ok(video_task),
            Err(e) => {
                let mut out = video_task;
                out.status = TaskStatus::Error;
                out.error = format!("audio mux failed: {e}");
                return Ok(out);
            }
        }
    }
    download_hls_media(client, task, ctrl).await
}

async fn fetch_key(
    client: &reqwest::Client,
    task: &DownloadTask,
    url: &str,
) -> Result<[u8; 16], String> {
    let mut req = client.get(url);
    if !task.referrer.is_empty() {
        req = req.header("Referer", &task.referrer);
    }
    if !task.cookies.is_empty() {
        req = req.header("Cookie", &task.cookies);
    }
    for (k, v) in &task.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for key {url}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    <[u8; 16]>::try_from(bytes.as_ref())
        .map_err(|_| format!("AES-128 key must be 16 bytes, got {}", bytes.len()))
}

async fn download_hls_media(
    client: reqwest::Client,
    task: DownloadTask,
    ctrl: Arc<engine::Controls>,
) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let text = fetch_text(&client, &task, &task.url).await?;
    if let Some(m) = hls::key_method(&text) {
        if m != "AES-128" && m != "NONE" {
            let mut t = task.clone();
            t.status = TaskStatus::Protected;
            t.error = format!("Encrypted HLS METHOD={m} is not supported");
            return Ok(t);
        }
    }
    let media = hls::resolve_implicit_ranges(hls::parse_media(&text, &base));
    if media.segments.is_empty() {
        return Err("HLS media playlist has no segments".into());
    }
    // Live HLS: if no ENDLIST, poll until ENDLIST appears or cancelled
    let mut media = media;
    if !media.end_list {
        let mut seen = media.segments.clone();
        let mut seen_keys = media.key_infos.clone();
        let mut seen_ranges = media.byte_ranges.clone();
        loop {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(
                media.target_duration.max(1.0) / 2.0,
            ))
            .await;
            let t2 = fetch_text(&client, &task, &task.url).await?;
            let m2 = hls::resolve_implicit_ranges(hls::parse_media(&t2, &base));
            for (i, s) in m2.segments.iter().enumerate() {
                if !seen.contains(s) {
                    seen.push(s.clone());
                    seen_keys.push(m2.key_infos.get(i).cloned().unwrap_or(None));
                    seen_ranges.push(m2.byte_ranges.get(i).copied().flatten());
                }
            }
            if m2.end_list {
                media = m2;
                break;
            }
        }
        if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            let mut t = task.clone();
            t.status = TaskStatus::Paused;
            return Ok(t);
        }
        if !media.end_list {
            media.segments = seen;
            media.key_infos = seen_keys;
            media.byte_ranges = seen_ranges;
        }
    }
    // Encrypted playlists: any non-AES-128 method refused; AES-128 handled below.
    // EXT-X-MAP init segment is clear (never decrypted).
    let encrypted = media.key_infos.iter().any(|k| k.is_some());
    if encrypted {
        for (i, ki) in media.key_infos.iter().enumerate() {
            if let Some(ki) = ki {
                if ki.method != "AES-128" {
                    let mut t = task.clone();
                    t.status = TaskStatus::Protected;
                    t.error = format!("Encrypted HLS METHOD={} is not supported", ki.method);
                    return Ok(t);
                }
                if ki.uri.is_none() {
                    return Err(format!("EXT-X-KEY AES-128 segment {i} missing URI"));
                }
            }
        }
    }
    // BYTERANGE or encrypted segments need per-segment fetch to temp then concat
    // (plain path streams raw bytes via download_media; decrypt needs whole segment).
    let has_ranges =
        media.byte_ranges.iter().any(|r| r.is_some()) || media.map_byte_range.is_some();
    if has_ranges || encrypted {
        let work = task
            .dest_dir
            .join(format!(".downloadcraft-hls-{}", task.id));
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        ctrl.parts_are_segments
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(0, media.segments.len() as u64)];
        let mut parts: Vec<std::path::PathBuf> = Vec::new();
        if let Some(init) = &media.init_uri {
            let p = work.join("init.bin");
            let n =
                engine::fetch_bytes_counted(&client, &task, init, media.map_byte_range, &p).await?;
            ctrl.downloaded
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            parts.push(p);
        }
        // Key cache by URI: keys may rotate mid-playlist.
        let mut key_cache: std::collections::HashMap<String, [u8; 16]> =
            std::collections::HashMap::new();
        for (i, seg) in media.segments.iter().enumerate() {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let mut t = task.clone();
                t.status = TaskStatus::Paused;
                return Ok(t);
            }
            let p = work.join(format!("{i}.seg"));
            let n = engine::fetch_bytes_counted(
                &client,
                &task,
                seg,
                media.byte_ranges.get(i).copied().flatten(),
                &p,
            )
            .await?;
            ctrl.downloaded
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            if let Some(part) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).first_mut() {
                part.0 = (i + 1) as u64;
            }
            if let Some(Some(ki)) = media.key_infos.get(i) {
                let key_uri = ki.uri.as_deref().unwrap();
                let key = match key_cache.get(key_uri) {
                    Some(k) => *k,
                    None => {
                        let k = fetch_key(&client, &task, key_uri).await?;
                        key_cache.insert(key_uri.to_string(), k);
                        k
                    }
                };
                let iv = ki
                    .iv
                    .unwrap_or_else(|| hls::iv_for_sequence(media.media_sequence + i as u64));
                let raw = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
                let plain = hls::decrypt_segment(&key, &iv, &raw)?;
                tokio::fs::write(&p, &plain)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            parts.push(p);
        }
        let dest = task.dest_dir.join(&task.filename);
        let mut total_bytes: u64 = 0;
        {
            let mut out = tokio::fs::File::create(&dest)
                .await
                .map_err(|e| e.to_string())?;
            use tokio::io::AsyncWriteExt;
            for p in &parts {
                let b = tokio::fs::read(p).await.map_err(|e| e.to_string())?;
                total_bytes += b.len() as u64;
                out.write_all(&b).await.map_err(|e| e.to_string())?;
            }
            out.flush().await.map_err(|e| e.to_string())?;
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
        let mut t = task;
        t.status = TaskStatus::Complete;
        t.downloaded = total_bytes;
        t.total = Some(total_bytes);
        ctrl.downloaded
            .store(total_bytes, std::sync::atomic::Ordering::Relaxed);
        return Ok(t);
    }
    let segs: Vec<(Option<String>, String)> = media
        .segments
        .iter()
        .map(|u| (media.init_uri.clone(), u.clone()))
        .collect();
    engine::download_media(client, task, ctrl, vec![("video".into(), segs)]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DownloadTask;

    fn kind_of(url: &str) -> ProtoKind {
        proto_kind(&DownloadTask {
            url: url.to_string(),
            ..Default::default()
        })
    }

    #[test]
    fn proto_kind_labels() {
        assert_eq!(
            kind_of("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"),
            ProtoKind::Torrent
        );
        assert_eq!(kind_of("https://x.org/a.torrent"), ProtoKind::Torrent);
        assert_eq!(kind_of("ftp://x.org/a.zip"), ProtoKind::Ftp);
        assert_eq!(kind_of("ftps://x.org/a.zip"), ProtoKind::Ftps);
        assert_eq!(kind_of("ftpsi://x.org/a.zip"), ProtoKind::Ftps);
        assert_eq!(kind_of("sftp://x.org/a.zip"), ProtoKind::Sftp);
        assert_eq!(kind_of("ssh://x.org/a.zip"), ProtoKind::Sftp);
        assert_eq!(kind_of("nzb://x.org/a.nzb"), ProtoKind::Nntp);
        assert_eq!(kind_of("https://x.org/a.meta4"), ProtoKind::Metalink);
        assert_eq!(kind_of("https://x.org/m.mpd?token=1"), ProtoKind::Dash);
        assert_eq!(kind_of("https://x.org/m.m3u8"), ProtoKind::Hls);
        assert_eq!(kind_of("https://x.org/a.zip"), ProtoKind::Http);
        assert_eq!(kind_of("file.zip"), ProtoKind::File);
        assert_eq!(ProtoKind::Torrent.label(), "TORRENT");
    }
}
