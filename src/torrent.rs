// BitTorrent via librqbit. Entry: magnet:?xt=urn:btih:..., http(s) URL ending
// .torrent (bytes fetched with app reqwest client), or local .torrent path.
// One shared Session for process (per-task Sessions would clash on listen
// port); persistence = config_dir/torrent-session (JSON) so restarts resume.
// File selection: task.torrent_files (empty = all) -> AddTorrentOptions.only_files.
use crate::engine::{Controls, wait_not_paused};
use crate::model::{DownloadTask, Settings, TaskStatus};
use librqbit::{
    AddTorrent, AddTorrentOptions, ListenerOptions, Magnet, Session,
    SessionOptions, SessionPersistenceConfig, TorrentStatsState,
};

/// librqbit handle type (lib.rs re-exports ManagedTorrent, not the Arc alias).
type ManagedTorrentHandle = std::sync::Arc<librqbit::ManagedTorrent>;
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

/// Torrent-picker detection for Add dialog (string only, no fs/network).
pub fn is_torrent_url(url: &str) -> bool {
    is_torrent_task(&DownloadTask {
        url: url.trim().to_string(),
        ..Default::default()
    })
}

/// Char-safe truncation for picker labels (full path stays on hover).
pub fn trunc_path(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

/// One file inside torrent (index = librqbit file id, used in only_files).
#[derive(Clone, Debug, PartialEq)]
pub struct TorrentFile {
    pub index: usize,
    pub path: String,
    pub len: u64,
}

/// Validate picker selection: empty/out-of-range -> Err.
/// (Caller stores empty vec = all files; never pass empty selection to librqbit.)
pub fn validate_selection(sel: &[usize], file_count: usize) -> Result<Vec<usize>, String> {
    if sel.is_empty() {
        return Err("select at least one file".into());
    }
    if sel.iter().any(|&i| i >= file_count) {
        return Err("file index out of range".into());
    }
    let mut v: Vec<usize> = sel.to_vec();
    v.sort_unstable();
    v.dedup();
    Ok(v)
}

/// Build librqbit add source from task URL/bytes (magnet / http .torrent / local).
/// Extra headers (task.headers) only honoured on http fetch (download path).
async fn build_add(
    client: &reqwest::Client,
    task: &DownloadTask,
) -> Result<AddTorrent<'static>, String> {
    let url = task.url.trim();
    if is_magnet(url) {
        Magnet::parse(url).map_err(|e| format!("bad magnet link: {e}"))?;
        Ok(AddTorrent::from_url(url.to_string()))
    } else if url.starts_with("http://") || url.starts_with("https://") {
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
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        Ok(AddTorrent::from_bytes(bytes))
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
        Ok(AddTorrent::from_bytes(bytes))
    }
}

fn files_from_info(
    info: &librqbit::ValidatedTorrentMetaV1Info<librqbit::ByteBufOwned>,
) -> Vec<TorrentFile> {
    info.iter_file_details()
        .enumerate()
        .map(|(index, d)| TorrentFile {
            index,
            path: d.filename.to_pathbuf().to_string_lossy().into_owned(),
            len: d.len,
        })
        .collect()
}

/// List files without starting download (list_only resolves magnet metadata).
/// Timeout guards dead magnets (librqbit itself waits forever).
pub async fn list_torrent_files(
    client: reqwest::Client,
    task: &DownloadTask,
    settings: &Settings,
) -> Result<Vec<TorrentFile>, String> {
    let session = shared_session(settings).await?;
    // Whole probe (fetch + metadata) bounded, not just add_torrent.
    let add = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        build_add(&client, task),
    )
    .await
    .map_err(|_| "timed out fetching torrent metadata (120s)".to_string())??;
    let fut = session.add_torrent(
        add,
        Some(AddTorrentOptions {
            list_only: true,
            ..Default::default()
        }),
    );
    let resp = tokio::time::timeout(std::time::Duration::from_secs(120), fut)
        .await
        .map_err(|_| "timed out fetching torrent metadata (120s)".to_string())?
        .map_err(|e| format!("add torrent: {e:#}"))?;
    match resp {
        librqbit::AddTorrentResponse::ListOnly(l) => Ok(files_from_info(&l.info)),
        // Duplicated download already running: metadata reachable via handle
        // (list_only early-return is skipped once torrent is managed).
        other => match other.into_handle() {
            Some(h) => h
                .with_metadata(|m| {
                    m.file_infos
                        .iter()
                        .enumerate()
                        .map(|(index, f)| TorrentFile {
                            index,
                            path: f.relative_filename.to_string_lossy().into_owned(),
                            len: f.len,
                        })
                        .collect::<Vec<_>>()
                })
                .map_err(|e| format!("torrent metadata: {e:#}")),
            None => Err("unexpected torrent add response".into()),
        },
    }
}

/// Normalize selection against torrent file count -> concrete set for
/// librqbit (which instant-finishes on empty). Empty/out-of-range-clamped-to-
/// nothing selections become the full set.
fn normalize_selection(
    selected: &[usize],
    count: usize,
) -> std::collections::HashSet<usize> {
    let set: std::collections::HashSet<usize> =
        selected.iter().copied().filter(|i| *i < count).collect();
    if set.is_empty() {
        (0..count).collect()
    } else {
        set
    }
}

/// Apply selection live: no init wait, retry-friendly (poll loop tolerates
/// Initializing-state bail by keeping request pending).
async fn update_selection(
    session: &Arc<Session>,
    handle: &ManagedTorrentHandle,
    selected: &[usize],
) -> Result<(), String> {
    let count = handle
        .with_metadata(|m| m.file_infos.len())
        .map_err(|e| format!("torrent metadata: {e:#}"))?;
    let set = normalize_selection(selected, count);
    session
        .update_only_files(handle, &set)
        .await
        .map_err(|e| format!("torrent select files: {e:#}"))
}

/// Re-apply persisted selection on an already-managed handle: empty = all files
/// (explicit full set clears a prior subset). Bounded wait for initialization.
async fn apply_selection(
    session: &Arc<Session>,
    handle: &ManagedTorrentHandle,
    selected: &[usize],
) -> Result<(), String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        handle.wait_until_initialized(),
    )
    .await
    .map_err(|_| "timed out initializing torrent (120s)".to_string())?
    .map_err(|e| format!("torrent init: {e:#}"))?;
    update_selection(session, handle, selected).await
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

/// Placeholder names seeded before metadata known (model::filename_from_url
/// fallback / magnet without dn=); only these may adopt handle.name().
fn is_placeholder_name(name: &str) -> bool {
    matches!(name.trim(), "" | "download.bin" | "torrent" | "magnet_")
}

fn publish(
    ctrl: &Controls,
    handle: &ManagedTorrentHandle,
    selected: &[usize],
) -> librqbit::TorrentStats {
    let s = handle.stats();
    // With selection, librqbit total_bytes = whole torrent in Initializing but
    // selected bytes once Live. Override with selected sum so bar never shrinks.
    // Option distinguishes "metadata not yet known" from "selected sums to 0".
    let selected_set: std::collections::HashSet<usize> = selected.iter().copied().collect();
    let selected_total: Option<u64> = if selected.is_empty() {
        None
    } else {
        handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| selected_set.contains(i))
                    .map(|(_, f)| f.len)
                    .sum::<u64>()
            })
            .ok()
    };
    let show_total = selected_total.unwrap_or(s.total_bytes);
    if show_total > 0 {
        ctrl.total.store(show_total, Ordering::Relaxed);
    } else if selected_total == Some(0) {
        // Zero-byte-only selection: clear any earlier whole-torrent total.
        ctrl.total.store(0, Ordering::Relaxed);
    }
    // Unselected files can hold shared pieces; cap at shown total.
    ctrl.downloaded
        .store(s.progress_bytes.min(show_total), Ordering::Relaxed);
    ctrl.parts_are_segments.store(false, Ordering::Relaxed);
    // Per-file parts when metadata gives file lengths; else single whole-torrent part.
    // All files published (unselected too) so progress UI can re-select them;
    // part_files stays index-aligned with parts.
    if !s.file_progress.is_empty() {
        let (lens, names): (Vec<u64>, Vec<String>) = handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .map(|f| (f.len, f.relative_filename.to_string_lossy().into_owned()))
                    .unzip()
            })
            .unwrap_or_default();
        let set_parts = |v: Vec<(u64, u64)>| {
            // Poison-tolerant: worker panic elsewhere must not cascade into GUI thread.
            *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = v;
        };
        if lens.len() == s.file_progress.len() && lens.len() > 1 {
            set_parts(
                s.file_progress
                    .iter()
                    .copied()
                    .zip(lens.iter().copied())
                    .collect(),
            );
            *ctrl.part_files.lock().unwrap_or_else(|e| e.into_inner()) =
                names.into_iter().enumerate().collect();
        } else {
            let total = show_total.max(1);
            set_parts(vec![(s.progress_bytes.min(total), total)]);
            *ctrl.part_files.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        }
    }
    // Swarm snapshot for Info dialog. Cheap stats only; trackers/hash static per handle.
    {
        let peer = s.live.as_ref().map(|l| &l.snapshot.peer_stats);
        let snap = crate::engine::TorrentSnap {
            trackers: handle.shared().trackers.iter().map(|u| u.to_string()).collect(),
            peers_live: peer.map(|p| p.live).unwrap_or(0),
            peers_connecting: peer.map(|p| p.connecting).unwrap_or(0),
            peers_seen: peer.map(|p| p.seen).unwrap_or(0),
            uploaded: s.uploaded_bytes,
            pieces_done: s
                .live
                .as_ref()
                .map(|l| l.snapshot.downloaded_and_checked_pieces)
                .unwrap_or(0),
            pieces_total: handle
                .with_metadata(|m| m.lengths().total_pieces())
                .unwrap_or(0),
            info_hash: handle.info_hash().as_string(),
            live: s.live.is_some(),
        };
        *ctrl.torrent.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
    }
    s
}

pub async fn download_torrent(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
    settings: &Settings,
) -> Result<DownloadTask, String> {
    // Validate magnet (btih hex/base32) before touching session/network.
    let add = build_add(&client, &task).await?;

    std::fs::create_dir_all(&task.dest_dir).map_err(|e| e.to_string())?;
    let session = shared_session(settings).await?;
    // librqbit 9.0.1: per-torrent limits only via AddTorrentOptions.ratelimits
    // (librqbit::limits::LimitsConfig, bytes/s); no runtime per-torrent setter
    // (TorrentStateLive.ratelimits private, Session.ratelimits is global-only).
    // ponytail: add-time only; live limit change needs delete+re-add — add when librqbit exposes setter.
    // Mirror engine::throttle convention: kbps * 1024 bytes/s, 0 = unlimited.
    let bps = std::num::NonZeroU32::new(
        u32::try_from(ctrl.limit_kbps.load(Ordering::Relaxed).saturating_mul(1024))
            .unwrap_or(u32::MAX),
    );
    // Added path has no metadata yet to clamp against; out-of-range in librqbit
    // bails whole add, so pre-filter cannot happen here. Stale state indices are
    // sanitized on resume via apply_selection; fresh adds come from validated picker.
    let only_files = if task.torrent_files.is_empty() {
        None
    } else {
        Some(task.torrent_files.clone())
    };
    let added = session
        .add_torrent(
            add,
            Some(AddTorrentOptions {
                // overwrite: files from a deleted/paused session entry must re-check + resume, not error.
                overwrite: true,
                output_folder: Some(task.dest_dir.to_string_lossy().into_owned()),
                only_files,
                ratelimits: librqbit::limits::LimitsConfig {
                    upload_bps: bps,
                    download_bps: bps,
                },
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    // AlreadyManaged ignores AddTorrentOptions: re-apply persisted selection below.
    let was_managed = matches!(added, librqbit::AddTorrentResponse::AlreadyManaged(..));
    let handle = added
        .into_handle()
        .ok_or("torrent add returned no handle")?;
    let id = librqbit::api::TorrentIdOrHash::Id(handle.id());
    if was_managed {
        // Handle pre-existed (sibling task / restored session): never delete it
        // on error; just fail this task. Restored handle may be paused: unpause.
        apply_selection(&session, &handle, &task.torrent_files).await?;
        let _ = session.unpause(&handle).await;
    }

    // Only placeholders adopt metadata name (here + poll loop); user names kept.
    let mut name_pending = is_placeholder_name(&task.filename);
    if name_pending {
        if let Some(name) = handle.name().filter(|n| !n.trim().is_empty()) {
            task.filename = crate::model::sanitize_filename(&name);
            name_pending = false;
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
        // Metadata may arrive after add (magnet): adopt name once.
        if name_pending {
            if let Some(name) = handle.name().filter(|n| !n.trim().is_empty()) {
                task.filename = crate::model::sanitize_filename(&name);
                name_pending = false;
            }
        }
        // Live reselection from progress dialog: apply + fold into worker task
        // (worker return overwrites state, so this is source of truth while running).
        // Errors (still Initializing / metadata) keep request pending for next tick.
        {
            let want = ctrl
                .desired_files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(want) = want {
                if update_selection(&session, &handle, &want).await.is_ok() {
                    task.torrent_files = want.clone();
                    let mut d = ctrl.desired_files.lock().unwrap_or_else(|e| e.into_inner());
                    // Clear only this request; newer toggle during await stays pending.
                    if *d == Some(want) {
                        *d = None;
                    }
                }
            }
        }
        let s = publish(&ctrl, &handle, &task.torrent_files);
        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
        let shown_total = ctrl.total.load(Ordering::Relaxed);
        if shown_total > 0 {
            task.total = Some(shown_total);
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
    let _s = publish(&ctrl, &handle, &task.torrent_files);
    task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
    // Zero-byte/error torrents: Some(0) made dialog show 0/1 parts + "1/1 done" nonsense.
    let shown_total = ctrl.total.load(Ordering::Relaxed);
    if shown_total > 0 {
        task.total = Some(shown_total);
    }
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
    fn normalize_selection_rules() {
        use std::collections::HashSet;
        // Empty -> all (librqbit instant-finishes on empty set).
        assert_eq!(normalize_selection(&[], 3), (0..3).collect::<HashSet<_>>());
        // Out-of-range clamped; nothing survives -> all.
        assert_eq!(normalize_selection(&[9], 3), (0..3).collect::<HashSet<_>>());
        assert_eq!(
            normalize_selection(&[2, 9], 3),
            HashSet::from([2])
        );
        assert_eq!(
            normalize_selection(&[1, 1], 3),
            HashSet::from([1])
        );
    }

    #[test]
    fn validate_selection_rules() {
        assert!(validate_selection(&[], 3).is_err());
        assert!(validate_selection(&[3], 3).is_err());
        assert_eq!(validate_selection(&[2, 0, 2, 1], 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(validate_selection(&[1], 3).unwrap(), vec![1]);
    }

    #[test]
    fn files_from_bencode_fixture() {
        // Hand-built multi-file .torrent: root name "root", 3 files.
        fn bstr(s: &[u8]) -> Vec<u8> {
            let mut v = format!("{}:", s.len()).into_bytes();
            v.extend_from_slice(s);
            v
        }
        fn file(path: &str, len: i64) -> Vec<u8> {
            let mut pathlist = b"l".to_vec();
            for part in path.split('/') {
                pathlist.extend(bstr(part.as_bytes()));
            }
            pathlist.push(b'e');
            let mut d = b"d6:lengthi".to_vec();
            d.extend(len.to_string().into_bytes());
            d.extend(b"e4:path".iter());
            d.extend(pathlist);
            d.push(b'e');
            d
        }
        // Keys sorted: files < name < piece length < pieces.
        let mut info = b"d5:filesl".to_vec();
        info.extend(file("a.txt", 10));
        info.extend(file("sub/b.mkv", 2000));
        info.extend(file("c.nfo", 30));
        info.extend(b"e4:name4:root12:piece lengthi16384e6:pieces0:e".iter());
        let mut torrent = b"d8:announce0:4:info".to_vec();
        torrent.extend(info);
        torrent.push(b'e');
        use librqbit::CloneToOwned;
        let meta = librqbit::torrent_from_bytes(&torrent).expect("fixture parses");
        let vt = meta
            .info
            .data
            .clone_to_owned(None)
            .validate()
            .expect("fixture validates");
        let files = files_from_info(&vt);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].index, 0);
        assert_eq!(files[0].len, 10);
        assert_eq!(files[1].len, 2000);
        assert_eq!(files[2].len, 30);
        assert!(
            files[1].path.replace('\\', "/").ends_with("sub/b.mkv"),
            "{}",
            files[1].path
        );
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
