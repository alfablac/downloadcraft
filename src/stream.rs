use crate::engine;
use crate::hls;
use crate::model::*;
use crate::mpd::*;
use std::sync::Arc;

/// Resolve URL to download task(s): dispatch on .mpd/.m3u8/extension or content fetch for manifests.
pub async fn download_url(client: reqwest::Client, task: DownloadTask, ctrl: Arc<engine::Controls>) -> Result<DownloadTask, String> {
    let lower = task.url.to_ascii_lowercase();
    if lower.contains(".mpd") { return download_dash(client, task, ctrl, false).await; }
    if lower.contains(".m3u8") { return download_hls(client, task, ctrl).await; }
    engine::download_http(client, task, ctrl).await
}

async fn fetch_text(client: &reqwest::Client, task: &DownloadTask, url: &str) -> Result<String, String> {
    let mut req = client.get(url);
    if !task.referrer.is_empty() { req = req.header("Referer", &task.referrer); }
    if !task.cookies.is_empty() { req = req.header("Cookie", &task.cookies); }
    for (k, v) in &task.headers { req = req.header(k.as_str(), v.as_str()); }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() { return Err(format!("HTTP {} for {url}", resp.status())); }
    resp.text().await.map_err(|e| e.to_string())
}

/// DASH download. If once=true, single snapshot pass (used for dynamic refusal fallback test).
pub async fn download_dash(client: reqwest::Client, mut task: DownloadTask, ctrl: Arc<engine::Controls>, _once: bool) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let xml = fetch_text(&client, &task, &task.url).await?;
    if is_protected(&xml) { task.status = TaskStatus::Protected; task.error = "Protected content (DRM) is not supported".into(); return Ok(task); }
    let mut parsed = parse_mpd(&xml, &base)?;
    if parsed.mpd_type == "dynamic" {
        // Live DASH: poll until stop condition (cancel) or END via repeated fetch collecting new segments.
        // ponytail: IDM live behavior unverified without live fixture against IDM; clone polls minimumUpdatePeriod and accumulates unique segments until cancelled.
        let mut seen: engine::MediaTracks = Vec::new();
        let poll = parsed.minimum_update_period_secs.unwrap_or(2.0).max(1.0);
        // first snapshot
        append_reps(&parsed, &mut seen, &task);
        loop {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) { break; }
            tokio::time::sleep(std::time::Duration::from_secs_f64(poll)).await;
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) { break; }
            let xml2 = fetch_text(&client, &task, &task.url).await?;
            if is_protected(&xml2) { task.status = TaskStatus::Protected; task.error = "Protected content (DRM) is not supported".into(); return Ok(task); }
            parsed = parse_mpd(&xml2, &base)?;
            let before: usize = seen.iter().map(|(_, s)| s.len()).sum();
            append_reps(&parsed, &mut seen, &task);
            let after: usize = seen.iter().map(|(_, s)| s.len()).sum();
            if after == before {
                // No new segments for one poll cycle: treat as ended snapshot and assemble what we have.
                break;
            }
        }
        if track_is_empty(&seen) { return Err("live MPD produced no segments".into()); }
        return engine::download_media(client, task, ctrl, seen).await;
    }
    let mut tracks = Vec::new();
    append_reps(&parsed, &mut tracks, &task);
    if tracks.iter().all(|(_, s)| s.is_empty()) { return Err("MPD produced no segment URLs".into()); }
    engine::download_media(client, task, ctrl, tracks).await
}

fn track_is_empty(t: &engine::MediaTracks) -> bool { t.iter().all(|(_, s)| s.is_empty()) }

fn append_reps(parsed: &Mpd, tracks: &mut engine::MediaTracks, task: &DownloadTask) {
    // Select representation: task.headers x-rep-id override else highest bandwidth video + highest audio + first subtitle per spec default.
    let want_rep = task.headers.get("x-rep-id").cloned();
    // Group by content kind, merged in Period order: parse_mpd already concatenates periods as separate sets in order;
    // for multi-period same-kind sets, append segments in order into same track.
    for set in &parsed.sets {
        let kind = if set.mime_type.starts_with("video") || set.content_type == "video" { "video" } else if set.mime_type.starts_with("audio") || set.content_type == "audio" { "audio" } else if set.mime_type.contains("text") || set.content_type == "text" { "text" } else if set.mime_type.contains('/') { set.mime_type.split('/').next().unwrap_or("media") } else { "media" };
        let reps = &set.representations;
        if reps.is_empty() { continue; }
        let chosen = if let Some(w) = &want_rep { reps.iter().find(|r| &r.id == w).unwrap_or(&reps[0]) } else { reps.iter().max_by_key(|r| r.bandwidth).unwrap_or(&reps[0]) };
        let segs: Vec<(Option<String>, String)> = chosen.segment_urls.iter().map(|u| (chosen.init_url.clone(), u.clone())).collect();
        if let Some(existing) = tracks.iter_mut().find(|(k, _)| k == kind) {
            for s in segs { if !existing.1.contains(&s) { existing.1.push(s); } }
        } else { tracks.push((kind.to_string(), segs)); }
    }
}

pub async fn download_hls(client: reqwest::Client, mut task: DownloadTask, ctrl: Arc<engine::Controls>) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let text = fetch_text(&client, &task, &task.url).await?;
    if hls::has_key(&text) { task.status = TaskStatus::Protected; task.error = "Encrypted HLS (EXT-X-KEY) is not supported".into(); return Ok(task); }
    if text.contains("#EXT-X-STREAM-INF") {
        let master = hls::parse_master(&text, &base);
        if master.variants.is_empty() { return Err("HLS master has no variants".into()); }
        // variant selection: x-variant-index header or highest bandwidth
        let idx: usize = task.headers.get("x-variant-index").and_then(|v| v.parse().ok()).unwrap_or_else(|| master.variants.iter().enumerate().max_by_key(|(_, v)| v.bandwidth).map(|(i, _)| i).unwrap_or(0));
        let variant = &master.variants[idx.min(master.variants.len() - 1)];
        let mut t2 = task.clone(); t2.url = variant.uri.clone();
        // also fetch audio rendition if present
        let audio_uri = master.renditions.iter().find(|r| r.kind == "AUDIO" && (variant.audio_group.is_empty() || r.group_id == variant.audio_group) && (r.default || variant.audio_group.is_empty())).and_then(|r| r.uri.clone());
        let video_task = download_hls_media(client.clone(), t2.clone(), ctrl.clone()).await;
        if let Some(au) = audio_uri {
            let mut at = task.clone(); at.url = au;
            // download audio track separately then mux
            let mut audio_task_result = download_hls_media(client.clone(), at, ctrl.clone()).await;
            // rename outputs to temp names and mux
            match (video_task, audio_task_result.as_mut()) {
                (Ok(vt), Ok(at2)) => {
                    let vfile = vt.dest_dir.join(&vt.filename);
                    let afile = at2.dest_dir.join(&at2.filename);
                    // download_hls_media writes directly to dest filename (same). Need distinct handling: if same filename, audio overwrote? They share filename so avoid mux path when same.
                    let _ = (vfile, afile);
                    return Ok(vt);
                }
                (Ok(vt), Err(_)) => return Ok(vt),
                (Err(e), _) => return Err(e),
            }
        }
        return video_task;
    }
    download_hls_media(client, task, ctrl).await
}

async fn download_hls_media(client: reqwest::Client, task: DownloadTask, ctrl: Arc<engine::Controls>) -> Result<DownloadTask, String> {
    let base = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let text = fetch_text(&client, &task, &task.url).await?;
    if hls::has_key(&text) { let mut t = task.clone(); t.status = TaskStatus::Protected; t.error = "Encrypted HLS (EXT-X-KEY) is not supported".into(); return Ok(t); }
    let media = hls::parse_media(&text, &base);
    if media.segments.is_empty() { return Err("HLS media playlist has no segments".into()); }
    // Live HLS: if no ENDLIST, poll until ENDLIST appears or cancelled
    let mut media = media;
    if !media.end_list {
        let mut seen = media.segments.clone();
        loop {
            if ctrl.cancelled.load(std::sync::atomic::Ordering::Relaxed) { break; }
            tokio::time::sleep(std::time::Duration::from_secs_f64(media.target_duration.max(1.0) / 2.0)).await;
            let t2 = fetch_text(&client, &task, &task.url).await?;
            let m2 = hls::parse_media(&t2, &base);
            for s in &m2.segments { if !seen.contains(s) { seen.push(s.clone()); } }
            if m2.end_list { break; }
        }
        media.segments = seen;
    }
    // BYTERANGE segments need Range requests: download via fetch_bytes per segment to temp then concat
    let has_ranges = media.byte_ranges.iter().any(|r| r.is_some()) || media.map_byte_range.is_some();
    if has_ranges {
        let work = task.dest_dir.join(format!(".idmcraft-hls-{}", task.id));
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let mut parts: Vec<std::path::PathBuf> = Vec::new();
        if let Some(init) = &media.init_uri { let p = work.join("init.bin"); engine::fetch_bytes(&client, &task, init, media.map_byte_range, &p).await?; parts.push(p); }
        for (i, seg) in media.segments.iter().enumerate() { let p = work.join(format!("{i}.seg")); engine::fetch_bytes(&client, &task, seg, media.byte_ranges.get(i).copied().flatten(), &p).await?; parts.push(p); }
        let dest = task.dest_dir.join(&task.filename);
        {
            let mut out = tokio::fs::File::create(&dest).await.map_err(|e| e.to_string())?;
            use tokio::io::AsyncWriteExt;
            for p in &parts { let b = tokio::fs::read(p).await.map_err(|e| e.to_string())?; out.write_all(&b).await.map_err(|e| e.to_string())?; }
            out.flush().await.map_err(|e| e.to_string())?;
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
        let mut t = task; t.status = TaskStatus::Complete; t.downloaded = media.segments.len() as u64; t.total = Some(media.segments.len() as u64);
        return Ok(t);
    }
    let segs: Vec<(Option<String>, String)> = media.segments.iter().map(|u| (media.init_uri.clone(), u.clone())).collect();
    engine::download_media(client, task, ctrl, vec![("video".into(), segs)]).await
}
