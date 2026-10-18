// BitTorrent via librqbit. Entry: magnet:?xt=urn:btih:..., http(s) URL ending
// .torrent (bytes fetched with app reqwest client), or local .torrent path.
// One shared Session for process (per-task Sessions would clash on listen
// port); persistence = config_dir/torrent-session (JSON) so restarts resume.
// ponytail: all files downloaded (no only_files selection); add file picker when UI needs it.
use crate::engine::{Controls, wait_not_paused};
use crate::model::{DownloadTask, Settings, TaskStatus};
use librqbit::{
    AddTorrent, AddTorrentOptions, ListenerOptions, Magnet, ManagedTorrent, Session,
    SessionOptions, SessionPersistenceConfig, TorrentStatsState,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

static SESSION: tokio::sync::OnceCell<Arc<Session>> = tokio::sync::OnceCell::const_new();

pub fn is_magnet(url: &str) -> bool {
    url.trim_start()
        .to_ascii_lowercase()
        .starts_with("magnet:?")
}

/// magnet:, http(s) .torrent, file:// .torrent, or existing local .torrent path.
pub fn is_torrent_task(task: &DownloadTask) -> bool {
    let url = task.url.trim();
    if is_magnet(url) {
        return true;
    }
    let lower = url.to_ascii_lowercase();
    if (lower.starts_with("http://") || lower.starts_with("https://"))
        && lower
            .split(['?', '#'])
            .next()
            .unwrap_or(&lower)
            .ends_with(".torrent")
    {
        return true;
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    std::path::Path::new(path).is_file() && path.to_ascii_lowercase().ends_with(".torrent")
}

async fn shared_session(settings: &Settings) -> Result<Arc<Session>, String> {
    SESSION
        .get_or_try_init(|| async {
            let dir = crate::platform::config_dir().join("torrent-session");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let opts = SessionOptions {
                // persistence one-liner: JSON store in config dir; fastresume skips re-hash on restart.
                persistence: Some(SessionPersistenceConfig::Json { folder: Some(dir) }),
                fastresume: true,
                listen: (settings.torrent_listen_port != 0).then(|| ListenerOptions {
                    listen_addr: ([0, 0, 0, 0], settings.torrent_listen_port).into(),
                    ..Default::default()
                }),
                ..Default::default()
            };
            Session::new_with_opts(settings.download_dir.clone(), opts)
                .await
                .map_err(|e| e.to_string())
        })
        .await
        .cloned()
}

fn publish(ctrl: &Controls, handle: &ManagedTorrent) -> librqbit::TorrentStats {
    let s = handle.stats();
    if s.total_bytes > 0 {
        ctrl.total.store(s.total_bytes, Ordering::Relaxed);
    }
    ctrl.downloaded.store(s.progress_bytes, Ordering::Relaxed);
    ctrl.parts_are_segments.store(false, Ordering::Relaxed);
    // Single (done,total) part: GUI strip + rate work; per-file parts need metadata file lengths — add if UI wants file-level view.
    if !s.file_progress.is_empty() {
        let total = s.total_bytes.max(1);
        *ctrl.parts.lock().unwrap() = vec![(s.progress_bytes.min(total), total)];
    }
    s
}

pub async fn download_torrent(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
    settings: &Settings,
) -> Result<DownloadTask, String> {
    let url = task.url.trim().to_string();
    let add = if is_magnet(&url) {
        // Validate (btih hex/base32) before touching session/network.
        Magnet::parse(&url).map_err(|e| format!("bad magnet link: {e}"))?;
        AddTorrent::from_url(url.clone())
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let mut req = client.get(&url);
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
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        AddTorrent::from_bytes(bytes)
    } else {
        let path = url.strip_prefix("file://").unwrap_or(&url);
        let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
        AddTorrent::from_bytes(bytes)
    };

    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let session = shared_session(settings).await?;
    let handle = session
        .add_torrent(
            add,
            Some(AddTorrentOptions {
                // overwrite: files from a deleted/paused session entry must re-check + resume, not error.
                overwrite: true,
                output_folder: Some(task.dest_dir.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| e.to_string())?
        .into_handle()
        .ok_or("torrent add returned no handle")?;
    let id = librqbit::api::TorrentIdOrHash::Id(handle.id());

    if let Some(name) = handle.name() {
        if !name.is_empty() {
            task.filename = crate::model::sanitize_filename(&name);
        }
    }
    task.status = TaskStatus::Downloading;

    // Control watcher: separate spawn survives app aborting worker future
    // (Stop/Remove) — torrent must not keep running in shared session.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (session, handle, ctrl, done) =
            (session.clone(), handle.clone(), ctrl.clone(), done.clone());
        tokio::spawn(async move {
            let mut was_paused = false;
            while !done.load(Ordering::Relaxed) {
                if ctrl.cancelled.load(Ordering::Relaxed) {
                    // Cancel semantics like Stop in other protocols: drop session
                    // entry, keep downloaded files on disk for later re-add/resume.
                    if session.get(id).is_some_and(|h| Arc::ptr_eq(&h, &handle)) {
                        let _ = session.delete(id, false).await;
                    }
                    return;
                }
                let paused = ctrl.paused.load(Ordering::Relaxed);
                if paused != was_paused {
                    was_paused = paused;
                    let _ = if paused {
                        session.pause(&handle).await
                    } else {
                        session.clone().unpause(&handle).await
                    };
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
    }

    let mut result: Result<(), String> = Ok(());
    loop {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            break;
        }
        // Honour pause flag even before watcher lands (belt + braces).
        if ctrl.paused.load(Ordering::Relaxed) && wait_not_paused(&ctrl).await.is_err() {
            break;
        }
        let s = publish(&ctrl, &handle);
        task.downloaded = s.progress_bytes;
        if s.total_bytes > 0 {
            task.total = Some(s.total_bytes);
        }
        if let Some(e) = &s.error {
            result = Err(e.clone());
            break;
        }
        if matches!(s.state, TorrentStatsState::Error) {
            result = Err("torrent error".into());
            break;
        }
        if s.finished {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    done.store(true, Ordering::Relaxed);
    // Cancel (flag) semantics like Stop in other protocols: Paused, data kept.
    // Watcher deletes session entry (delete_files=false).
    if ctrl.cancelled.load(Ordering::Relaxed) {
        task.status = TaskStatus::Paused;
        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
        return Ok(task);
    }
    result?;
    let s = publish(&ctrl, &handle);
    task.downloaded = s.progress_bytes;
    task.total = Some(s.total_bytes);
    // ponytail: v1 stops seeding on complete unless seed_after_complete; persistent seeding across app restarts needs session-restore ownership in app.rs — add when seeding is a feature.
    if !settings.torrent_seed_after_complete {
        let _ = session.pause(&handle).await;
    }
    task.status = TaskStatus::Complete;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_btih_hex_and_base32_same_hash() {
        // infohash bytes 0x00..0x13
        let bytes: Vec<u8> = (0u8..20).collect();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let m_hex = Magnet::parse(&format!("magnet:?xt=urn:btih:{hex}&dn=Test")).unwrap();
        assert_eq!(m_hex.name.as_deref(), Some("Test"));
        // base32 (RFC4648, no pad) of same 20 bytes, computed inline.
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut b32 = String::new();
        for c in bytes.chunks(5) {
            let n = c.iter().fold(0u64, |a, &b| (a << 8) | b as u64) << (8 * (5 - c.len()));
            for i in 0..8 {
                b32.push(T[(n >> (35 - 5 * i)) as usize & 31] as char);
            }
        }
        let m_b32 = Magnet::parse(&format!("magnet:?xt=urn:btih:{b32}")).unwrap();
        assert_eq!(m_hex.as_id20(), m_b32.as_id20());
        assert!(m_hex.as_id20().is_some());
    }

    #[test]
    fn magnet_rejects_bad() {
        assert!(Magnet::parse("magnet:?xt=urn:btih:xyz").is_err());
        assert!(Magnet::parse("magnet:?dn=nohash").is_err());
        assert!(Magnet::parse("https://example.com/x.torrent").is_err());
    }

    #[test]
    fn dispatch_detection() {
        let mut t = DownloadTask {
            url: "magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862".into(),
            ..Default::default()
        };
        assert!(is_torrent_task(&t));
        t.url = "https://example.com/file.TORRENT?x=1".into();
        assert!(is_torrent_task(&t));
        t.url = "https://example.com/file.zip".into();
        assert!(!is_torrent_task(&t));
    }
}
