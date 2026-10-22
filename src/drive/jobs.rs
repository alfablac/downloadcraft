//! Drive transfer jobs: recursive copy/move between any two backends
//! (local<->cloud, cloud<->cloud). Files run `concurrency` at a time
//! (rclone --transfers), each file download uses `streams` ranged
//! segments where backend supports (rclone --multi-thread-streams).
use super::{DriveBackend, Progress, RemoteEntry};
use futures_util::StreamExt;
use std::sync::Arc;

/// Normalize local OS path separators to drive '/' for rel computation.
fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum JobKind {
    Upload,
    Download,
    Copy,
    Move,
    Sync,
    Delete,
}

/// Expand one source entry into file jobs: recursive walk.
pub async fn collect_files(
    backend: &Arc<dyn DriveBackend>,
    entry: &RemoteEntry,
) -> Result<Vec<RemoteEntry>, String> {
    if !entry.is_dir {
        return Ok(vec![entry.clone()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![entry.clone()];
    while let Some(d) = stack.pop() {
        for e in backend.list(&d.path).await? {
            if e.is_dir {
                stack.push(e);
            } else {
                out.push(e);
            }
        }
    }
    Ok(out)
}

/// Temp dir for cross-backend staging (config cache/drive-tmp).
pub fn staging_path(name: &str) -> std::path::PathBuf {
    let d = crate::platform::cache_dir().join("drive-tmp");
    let _ = std::fs::create_dir_all(&d);
    d.join(format!("{}-{}", std::process::id(), name))
}

/// Create dst_dir segments that don't exist yet. Each segment checked
/// via list(parent) first — Google Drive create is NOT idempotent (new
/// folder object every call), a bare mkdir loop duplicates folders.
pub async fn ensure_dir(dst_be: &Arc<dyn DriveBackend>, dst_dir: &str) {
    let d = norm(dst_dir).trim_matches('/').to_string();
    if d.is_empty() {
        return;
    }
    let mut cur = String::new();
    for seg in d.split('/') {
        let parent = cur.clone();
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(seg);
        let exists = dst_be
            .list(&parent)
            .await
            .map(|v| {
                v.iter()
                    .any(|e| e.is_dir && e.name.eq_ignore_ascii_case(seg))
            })
            .unwrap_or(false);
        if !exists {
            let _ = dst_be.mkdir(&cur).await;
        }
    }
}

/// Copy one file src->dst backends into dst_dir keeping name.
/// Caller must ensure dst_dir exists (run_job dedupes ensure_dir).
/// Progress: staging download NOT counted (local copy to temp would
/// fake instant progress); only destination upload counts, via child
/// Progress that forwards deltas into shared job counters. Backend
/// set_total() then only touches child totals, never job total.
pub async fn copy_file(
    src_be: &Arc<dyn DriveBackend>,
    entry: &RemoteEntry,
    dst_be: &Arc<dyn DriveBackend>,
    dst_dir: &str,
    streams: u32,
    prog: &Progress,
) -> Result<(), String> {
    prog.check()?;
    // Server-side: destination provider copies from source directly.
    // Crypt peers excluded (ids are logical, bytes encrypted).
    if !src_be.crypt_wrapped()
        && !dst_be.crypt_wrapped()
        && dst_be.copy_from(src_be, entry, dst_dir, prog).await.is_ok()
    {
        prog.add(entry.size);
        return Ok(());
    }
    let tmp = staging_path(&format!(
        "{}-{}",
        crate::model::new_id(),
        entry.name.replace(['/', '\\'], "_")
    ));
    // Staging sink: own counters, shared cancel/pause.
    let sink = Progress {
        done: Default::default(),
        total: Default::default(),
        cancelled: prog.cancelled.clone(),
        paused: prog.paused.clone(),
    };
    let r = src_be.download(entry, &tmp, streams.max(1), &sink).await;
    if let Err(e) = r {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    // Upload child: forward progress into job `done` via delta poll task.
    let child = Progress {
        done: Default::default(),
        total: Default::default(),
        cancelled: prog.cancelled.clone(),
        paused: prog.paused.clone(),
    };
    let relay = child.clone();
    let job_prog = prog.clone();
    let forwarded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let fwd2 = forwarded.clone();
    let relay_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let d = relay.done.load(std::sync::atomic::Ordering::Relaxed);
            let last = fwd2.load(std::sync::atomic::Ordering::Relaxed);
            if d > last {
                job_prog.add(d - last);
                fwd2.store(d, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    let up = dst_be.upload(&tmp, dst_dir, &entry.name, &child).await;
    relay_task.abort();
    // Flush remainder not yet relayed (exact, no double count).
    let d = child.done.load(std::sync::atomic::Ordering::Relaxed);
    let fwd = forwarded.load(std::sync::atomic::Ordering::Relaxed);
    if d > fwd {
        prog.add(d - fwd);
    }
    let _ = tokio::fs::remove_file(&tmp).await;
    up?;
    Ok(())
}

/// Delete partial destination file after error/cancel so half files
/// don't look complete. Best effort.
pub async fn remove_dest(dst_be: &Arc<dyn DriveBackend>, dst_dir: &str, name: &str) {
    let e = RemoteEntry {
        id: super::join_path(dst_dir, name),
        name: name.to_string(),
        path: super::join_path(dst_dir, name),
        is_dir: false,
        size: 0,
        modified: 0,
        mime: String::new(),
    };
    let _ = dst_be.delete(&e).await;
}

/// Run one job over entries. `concurrency` files at once, `streams`
/// segments per file. Returns summary; failed files counted, first error kept.
#[allow(clippy::too_many_arguments)]
pub async fn run_job(
    kind: JobKind,
    src_be: &Arc<dyn DriveBackend>,
    entries: &[RemoteEntry],
    dst_be: &Arc<dyn DriveBackend>,
    dst_dir: &str,
    local_dest: Option<&std::path::Path>,
    streams: u32,
    concurrency: usize,
    prog: &Progress,
) -> Result<String, String> {
    let mut files: Vec<(RemoteEntry, String)> = Vec::new();
    let mut total: u64 = 0;
    for e in entries {
        if e.is_dir {
            let sub = collect_files(src_be, e).await?;
            for f in &sub {
                let rel = norm(&f.path)
                    .strip_prefix(&norm(&e.path))
                    .unwrap_or("")
                    .trim_matches('/')
                    .to_string();
                let parent_rel = super::parent_path(&rel);
                let base = super::join_path(dst_dir, &norm(&e.name));
                let d = if parent_rel.is_empty() {
                    base
                } else {
                    super::join_path(&base, &parent_rel)
                };
                files.push((f.clone(), d));
                total += f.size;
            }
            if sub.is_empty() {
                let _ = dst_be.mkdir(&super::join_path(dst_dir, &e.name)).await;
            }
        } else {
            files.push((e.clone(), dst_dir.to_string()));
            total += e.size;
        }
    }
    prog.set_total(total);
    // Create every destination dir once, sequentially, shallow-first.
    // Per-file ensure_dir duplicated folders on Google Drive.
    if matches!(
        kind,
        JobKind::Upload | JobKind::Copy | JobKind::Sync | JobKind::Move
    ) {
        let mut dirs: Vec<String> = files.iter().map(|(_, d)| d.clone()).collect();
        dirs.sort();
        dirs.dedup();
        dirs.sort_by_key(|d| d.matches('/').count());
        for d in &dirs {
            ensure_dir(dst_be, d).await;
        }
    }
    let conc = concurrency.clamp(1, 16);
    let results: Vec<Result<(), String>> =
        futures_util::stream::iter(files.into_iter().map(|(f, d)| {
            let src_be = src_be.clone();
            let dst_be = dst_be.clone();
            let prog = prog.clone();
            let local_dest = local_dest.map(|p| p.to_path_buf());
            async move {
                prog.check()?;
                match kind {
                    JobKind::Download => {
                        let dest = local_dest.ok_or("download needs local dest")?.join(&f.name);
                        let r = src_be.download(&f, &dest, streams.max(1), &prog).await;
                        if r.is_err() {
                            let _ = tokio::fs::remove_file(&dest).await;
                            let _ =
                                tokio::fs::remove_file(dest.with_extension("wc-crypt-tmp")).await;
                        }
                        r
                    }
                    JobKind::Delete => src_be.delete(&f).await,
                    JobKind::Upload | JobKind::Copy | JobKind::Sync => {
                        copy_file(&src_be, &f, &dst_be, &d, streams, &prog).await
                    }
                    JobKind::Move => {
                        copy_file(&src_be, &f, &dst_be, &d, streams, &prog).await?;
                        src_be.delete(&f).await
                    }
                }
            }
        }))
        .buffer_unordered(conc)
        .collect()
        .await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let _ = ok;
    let first_err = results.into_iter().find_map(|r| r.err());
    match first_err {
        Some(e) if e == "cancelled" => Err(e),
        Some(e) => Err(format!("{e} ({ok} file(s) ok)")),
        None => Ok(format!("{ok} file(s), {}", super::fmt_bytes(total))),
    }
}
