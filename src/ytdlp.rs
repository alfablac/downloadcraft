//! yt-dlp sidecar: resolve/download binary, list formats, download video pages.
use crate::engine::Controls;
use crate::model::*;
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

fn bin_dir() -> PathBuf {
    crate::platform::config_dir().join("bin")
}

/// (a) sidecar next to exe, (b) config_dir/bin, (c) PATH (bare name if on PATH).
pub fn ytdlp_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let p = bin_dir().join(exe_name());
    if p.is_file() {
        return Some(p);
    }
    // PATH probe: spawn `--version` cheap check via which-style search.
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

pub async fn ensure_ytdlp() -> Result<PathBuf, String> {
    if let Some(p) = ytdlp_path() {
        return Ok(p);
    }
    let dir = bin_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let dest = dir.join(exe_name());
    let url = if cfg!(windows) {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
    } else {
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp"
    };
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("yt-dlp download HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() < 1_000_000 {
        return Err(format!("yt-dlp download too small ({} bytes)", bytes.len()));
    }
    let part = dir.join(format!("{}.part", exe_name()));
    tokio::fs::write(&part, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).await;
    }
    tokio::fs::rename(&part, &dest)
        .await
        .map_err(|e| e.to_string())?;
    Ok(dest)
}

// ponytail: -U is upstream-supported self-updater; do not reimplement release asset swap.
pub fn ytdlp_update(path: &std::path::Path) -> Result<String, String> {
    let out = run_timeout(path, &["-U".to_string()], 120)?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(tail(&s))
    } else {
        Err(tail(&s))
    }
}

fn tail(s: &str) -> String {
    let t: Vec<&str> = s.lines().collect();
    t[t.len().saturating_sub(5)..].join("\n")
}

fn run_timeout(
    path: &std::path::Path,
    args: &[String],
    secs: u64,
) -> Result<std::process::Output, String> {
    let mut child = Command::new(path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => return child.wait_with_output().map_err(|e| e.to_string()),
            None if start.elapsed().as_secs() >= secs => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("yt-dlp timed out after {secs}s"));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct YtFormat {
    pub format_id: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub vcodec: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub filesize: Option<u64>,
    #[serde(default)]
    pub filesize_approx: Option<u64>,
}

impl YtFormat {
    pub fn size(&self) -> Option<u64> {
        self.filesize.or(self.filesize_approx)
    }
    fn has_video(&self) -> bool {
        // vcodec null = progressive format with unknown codec (X http-*): treat as video.
        match self.vcodec.as_deref() {
            None => true,
            Some(c) => c != "none",
        }
    }
    fn has_audio(&self) -> bool {
        self.acodec.as_deref().is_some_and(|c| c != "none")
    }
}

/// Best: max height with both codecs; else max-height video-only (download adds +bestaudio).
pub fn pick_best(formats: &[YtFormat]) -> Option<YtFormat> {
    let real: Vec<&YtFormat> = formats.iter().filter(|f| f.has_video()).collect();
    real.iter()
        .filter(|f| f.has_audio())
        .max_by_key(|f| f.height.unwrap_or(0))
        .or_else(|| real.iter().max_by_key(|f| f.height.unwrap_or(0)))
        .map(|f| (*f).clone())
}

fn base_args(url: &str, task: &DownloadTask) -> Vec<String> {
    let mut a: Vec<String> = vec!["--no-playlist".into(), "--no-warnings".into()];
    if !task.referrer.is_empty() {
        a.push("--referer".into());
        a.push(task.referrer.clone());
    }
    if !task.cookies.is_empty() {
        a.push("--add-header".into());
        a.push(format!("Cookie:{}", task.cookies));
    }
    for (k, v) in &task.headers {
        if k.eq_ignore_ascii_case("x-ytdlp") {
            continue;
        }
        a.push("--add-header".into());
        a.push(format!("{k}:{v}"));
    }
    a.push(url.to_string());
    a
}

#[derive(Deserialize)]
pub struct YtDump {
    #[serde(default)]
    pub formats: Vec<YtFormat>,
    #[serde(default)]
    pub title: String,
}

pub async fn fetch_formats(url: &str, task: &DownloadTask) -> Result<YtDump, String> {
    let path = ytdlp_path().ok_or("yt-dlp not found")?;
    let mut args = vec!["-J".to_string()];
    args.extend(base_args(url, task));
    let out = tokio::task::spawn_blocking(move || {
        let o = Command::new(&path)
            .args(&args)
            .output()
            .map_err(|e| e.to_string())?;
        if !o.status.success() {
            return Err(tail(&String::from_utf8_lossy(&o.stderr)));
        }
        Ok(o.stdout)
    })
    .await
    .map_err(|e| e.to_string())??;
    let dump: YtDump = serde_json::from_slice(&out).map_err(|e| e.to_string())?;
    Ok(dump)
}

fn parse_progress(
    line: &str,
    ctrl: &Controls,
    video_done_base: &Arc<std::sync::atomic::AtomicU64>,
) {
    // Template: download:<downloaded>|<total>|<speed>; total may be "NA".
    let Some(rest) = line.strip_prefix("download:") else {
        return;
    };
    let mut it = rest.split('|');
    let dl: u64 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let tot: Option<u64> = it.next().and_then(|s| s.trim().parse().ok());
    // yt-dlp restarts counter per file (video then audio); keep monotonic total progress.
    let base = video_done_base.load(Ordering::Relaxed);
    ctrl.downloaded
        .store(base.saturating_add(dl), Ordering::Relaxed);
    if let Some(t) = tot {
        let cur = ctrl.total.load(Ordering::Relaxed);
        if base + t > cur {
            ctrl.total.store(base + t, Ordering::Relaxed);
        }
    }
}

/// Download via yt-dlp. On success returns completed task; caller falls back to native on Err.
pub async fn download_with_ytdlp(
    task: DownloadTask,
    ctrl: Arc<Controls>,
) -> Result<DownloadTask, String> {
    let path = ensure_ytdlp().await?;
    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    // Format choice: -J first; fallback selector lets yt-dlp decide.
    // Also adopt video title as filename when seeded name is extensionless ID.
    let dump = fetch_formats(&task.url, &task).await.ok();
    let mut task = task;
    if let Some(d) = &dump {
        if !d.title.trim().is_empty() && !task.filename.contains('.') {
            task.filename = crate::model::sanitize_filename(&format!("{}.mp4", d.title));
        }
    }
    let chosen = dump.and_then(|d| pick_best(&d.formats));
    let fmt_arg = match &chosen {
        Some(f) if f.has_audio() => f.format_id.clone(),
        // Fallbacks mandatory: chosen id can vanish between -J and download (client/format rotation).
        Some(f) => format!("{}+bestaudio/bv*+ba/best", f.format_id),
        None => "bv*+ba/b".to_string(),
    };
    let mut args: Vec<String> = vec![
        "--newline".into(),
        "--progress-template".into(),
        "download:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.speed)s".into(),
        "-f".into(),
        fmt_arg,
        "--merge-output-format".into(),
        "mp4".into(),
        "--concurrent-fragments".into(),
        "4".into(),
    ];
    if let Some(ff) = ffmpeg_location() {
        args.push("--ffmpeg-location".into());
        args.push(ff);
    }
    args.push("-o".into());
    args.push(dest.to_string_lossy().into_owned());
    // URL last (base_args appends it); insert flags before it.
    let mut full = base_args(&task.url, &task);
    let url = full.pop().expect("url");
    full.extend(args);
    full.push(url);

    let ctrl2 = ctrl.clone();
    let started = std::time::SystemTime::now();
    let out = tokio::task::spawn_blocking(move || run_ytdlp_download(&path, &full, &ctrl2))
        .await
        .map_err(|e| e.to_string())?;
    match out {
        Ok(()) => {}
        Err(e) if e == "cancelled" => {
            let mut t = task.clone();
            t.status = TaskStatus::Paused;
            t.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
            return Ok(t);
        }
        Err(e) => return Err(e),
    }
    // yt-dlp may keep source ext despite -o exact path: find dest or newest stem.* sibling.
    // Exclude intermediates (.f<id>., .part/.ytdl) and files predating this run.
    if !dest.is_file() {
        let stem = dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        if let Ok(rd) = std::fs::read_dir(&task.dest_dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if name
                    .rsplit('.')
                    .next()
                    .is_some_and(|ext| ext == "part" || ext == "ytdl")
                {
                    continue;
                }
                if p.file_stem().and_then(|s| s.to_str()) == Some(stem.as_str()) && p != dest {
                    // stem.f401.mp4 = unmerged intermediate, skip; stem.webm = final renamed ext, keep.
                    if name[stem.len()..].starts_with(".f") {
                        continue;
                    }
                    if let Ok(m) = e.metadata().and_then(|mm| mm.modified()) {
                        if m >= started && best.as_ref().is_none_or(|(t, _)| m > *t) {
                            best = Some((m, p));
                        }
                    }
                }
            }
        }
        match best {
            Some((_, p)) => {
                std::fs::rename(&p, &dest).map_err(|e| e.to_string())?;
            }
            None => {
                // yt-dlp merger failed silently in past runs: mux leftover .f<id> tracks ourselves.
                let stem = dest
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let mut tracks: Vec<PathBuf> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&task.dest_dir) {
                    for e in rd.flatten() {
                        let p = e.path();
                        let name = p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string();
                        if name.starts_with(&format!("{stem}.f"))
                            && !name.ends_with(".part")
                            && !name.ends_with(".ytdl")
                        {
                            tracks.push(p);
                        }
                    }
                }
                tracks.sort();
                if tracks.len() >= 2 {
                    crate::engine::mux_tracks(&tracks, &dest)
                        .await
                        .map_err(|e| format!("yt-dlp merge failed; fallback mux failed: {e}"))?;
                    if dest.is_file() {
                        for t in &tracks {
                            let _ = std::fs::remove_file(t);
                        }
                    } else {
                        return Err("yt-dlp finished but output file not found".into());
                    }
                } else {
                    return Err("yt-dlp finished but output file not found".into());
                }
            }
        }
    }
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    ctrl.downloaded.store(size, Ordering::Relaxed);
    if size > 0 {
        ctrl.total.store(size, Ordering::Relaxed);
    }
    let mut t = task;
    t.status = TaskStatus::Complete;
    t.downloaded = size;
    t.total = Some(size);
    Ok(t)
}

fn ffmpeg_location() -> Option<String> {
    // Must be absolute: yt-dlp resolves --ffmpeg-location relative to its own CWD,
    // and bare "ffmpeg" can miss when child CWD differs. PATH search mirrors ytdlp_path.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
        .map(|p| p.to_string_lossy().into_owned())
}

fn run_ytdlp_download(
    path: &std::path::Path,
    args: &[String],
    ctrl: &Arc<Controls>,
) -> Result<(), String> {
    let mut cmd = Command::new(path);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("yt-dlp: no stdout")?;
    let stderr = child.stderr.take().ok_or("yt-dlp: no stderr")?;
    // Capture stderr tail for error surfacing (piped-but-undrained deadlocks).
    let err_tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let err_tail2 = err_tail.clone();
    let _err_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut g = err_tail2.lock().unwrap_or_else(|e| e.into_inner());
            g.push(line);
            if g.len() > 20 {
                g.remove(0);
            }
        }
    });
    let video_base = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ctrl_c = ctrl.clone();
    // Reader thread: progress lines. New Destination folds prior file bytes into base (counters restart per file).
    let base2 = video_base.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if line.contains("[download] Destination:") {
                let cur = ctrl_c.downloaded.load(Ordering::Relaxed);
                if cur > base2.load(Ordering::Relaxed) {
                    base2.store(cur, Ordering::Relaxed);
                }
            }
            if line.starts_with("download:") {
                parse_progress(&line, &ctrl_c, &base2);
            }
        }
    });
    loop {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("cancelled".into());
        }
        if ctrl.paused.load(Ordering::Relaxed) {
            // ponytail: no cross-platform suspend; kill and let caller mark Paused. Resume re-invokes yt-dlp (keeps .part).
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err("cancelled".into());
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let _ = reader.join();
                let _ = _err_thread.join();
                if status.success() {
                    return Ok(());
                }
                let t = err_tail.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
                return Err(if t.trim().is_empty() {
                    "yt-dlp exited with error".into()
                } else {
                    format!("yt-dlp failed: {}", tail(&t))
                });
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

pub fn is_video_page(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let path = u.path().to_ascii_lowercase();
    let h = host.strip_prefix("www.").unwrap_or(&host);
    let h = h.strip_prefix("m.").unwrap_or(h);
    match h {
        "youtube.com" => {
            path.starts_with("/watch") || path.starts_with("/shorts/") || path.starts_with("/live/")
        }
        "youtu.be" => path.len() > 1,
        "music.youtube.com" => path.starts_with("/watch"),
        "vimeo.com" => path.chars().nth(1).is_some_and(|c| c.is_ascii_digit()),
        "dailymotion.com" => path.starts_with("/video/"),
        "dai.ly" => path.len() > 1,
        "twitch.tv" => path.starts_with("/videos/"),
        "tiktok.com" => path.contains("/video/"),
        "instagram.com" => path.starts_with("/reel/") || path.starts_with("/p/"),
        "twitter.com" | "x.com" => path.contains("/status/"),
        "facebook.com" | "fb.watch" => {
            h == "fb.watch"
                || path.contains("/videos/")
                || path.starts_with("/watch")
                || path.starts_with("/reel/")
        }
        "reddit.com" => path.contains("/comments/"),
        "soundcloud.com" => path.matches('/').count() >= 2,
        "bilibili.com" => path.starts_with("/video/"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn video_page_detection() {
        assert!(is_video_page("https://www.youtube.com/watch?v=aqz-KE-bpKQ"));
        assert!(is_video_page("https://youtu.be/aqz-KE-bpKQ"));
        assert!(is_video_page("https://vimeo.com/12345"));
        assert!(is_video_page("https://www.twitch.tv/videos/123"));
        assert!(!is_video_page("https://example.com/file.mp4"));
        assert!(!is_video_page("https://www.youtube.com/feed/subscriptions"));
        assert!(!is_video_page("https://vimeo.com/about"));
        assert!(!is_video_page("not a url"));
    }
    #[test]
    fn pick_best_prefers_muxed_then_height() {
        let fs = vec![
            YtFormat {
                format_id: "a".into(),
                ext: "mp4".into(),
                height: Some(720),
                vcodec: Some("avc".into()),
                acodec: Some("none".into()),
                filesize: None,
                filesize_approx: None,
            },
            YtFormat {
                format_id: "b".into(),
                ext: "mp4".into(),
                height: Some(360),
                vcodec: Some("avc".into()),
                acodec: Some("aac".into()),
                filesize: None,
                filesize_approx: None,
            },
            YtFormat {
                format_id: "sb".into(),
                ext: "mhtml".into(),
                height: None,
                vcodec: Some("none".into()),
                acodec: Some("none".into()),
                filesize: None,
                filesize_approx: None,
            },
        ];
        assert_eq!(pick_best(&fs).unwrap().format_id, "b");
        let fs2 = vec![YtFormat {
            format_id: "a".into(),
            ext: "mp4".into(),
            height: Some(720),
            vcodec: Some("avc".into()),
            acodec: Some("none".into()),
            filesize: None,
            filesize_approx: None,
        }];
        assert_eq!(pick_best(&fs2).unwrap().format_id, "a");
    }
}
