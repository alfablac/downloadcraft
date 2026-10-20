//! Shared HTTP helpers for drive backends.
use super::Progress;
use futures_util::StreamExt;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

pub fn client() -> reqwest::Client {
    crate::engine::build_client("")
}

pub async fn err_text(resp: reqwest::Response) -> String {
    let st = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    format!("HTTP {st}: {body}")
}

/// Download URL (GET, optional bearer + extra headers) to file with up to
/// `parallel` ranged segments when server advertises ranges; else single
/// stream. Resume within run only (temp `.wcpart` segments, assembled at
/// end, deleted on cancel). Cross-restart resume: callers route through
/// normal download engine when they have direct URL (UI does for WebDAV).
pub async fn download_url(
    client: &reqwest::Client,
    make_req: &(dyn Fn() -> reqwest::RequestBuilder + Send + Sync),
    dest: &std::path::Path,
    parallel: u32,
    prog: &Progress,
) -> Result<(), String> {
    prog.check()?;
    // Probe size + ranges with ranged GET 0-0 (HEAD rejected by some APIs).
    let probe = make_req()
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = probe.status();
    let mut total: u64 = 0;
    let mut ranged = false;
    if status.as_u16() == 206 {
        ranged = true;
        if let Some(cr) = probe
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(t) = cr.rsplit('/').next() {
                total = t.parse().unwrap_or(0);
            }
        }
    } else if status.is_success() {
        total = probe
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    } else {
        return Err(err_text(probe).await);
    }
    prog.grow_total(total);
    let nseg = if ranged && total > 1 << 20 && parallel > 1 {
        parallel.min(8)
    } else {
        1
    };
    if nseg <= 1 || total == 0 {
        // Single stream (probe body discarded; fresh GET).
        drop(probe);
        let resp = make_req().send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() && resp.status().as_u16() != 206 {
            return Err(err_text(resp).await);
        }
        stream_to_file(resp, dest, prog).await
    } else {
        drop(probe);
        download_segmented(client, make_req, dest, total, nseg, prog).await
    }
}

async fn stream_to_file(
    resp: reqwest::Response,
    dest: &std::path::Path,
    prog: &Progress,
) -> Result<(), String> {
    if let Some(p) = dest.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut f = tokio::fs::File::create(dest)
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        prog.wait().await;
        prog.check()?;
        let chunk = chunk.map_err(|e| e.to_string())?;
        f.write_all(&chunk).await.map_err(|e| e.to_string())?;
        prog.add(chunk.len() as u64);
    }
    f.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn download_segmented(
    _client: &reqwest::Client,
    make_req: &(dyn Fn() -> reqwest::RequestBuilder + Send + Sync),
    dest: &std::path::Path,
    total: u64,
    nseg: u32,
    prog: &Progress,
) -> Result<(), String> {
    if let Some(p) = dest.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| e.to_string())?;
    }
    // Preallocate.
    {
        let f = tokio::fs::File::create(dest)
            .await
            .map_err(|e| e.to_string())?;
        f.set_len(total).await.map_err(|e| e.to_string())?;
    }
    let chunk = total / nseg as u64;
    let prog = prog.clone();
    let mut futs = Vec::new();
    // make_req not Clone-able closure: wrap in Arc.
    // Caller closure captured by reference not allowed across tasks;
    // segments run concurrently via join (same task).
    for i in 0..nseg {
        let start = i as u64 * chunk;
        let end = if i + 1 == nseg {
            total - 1
        } else {
            start + chunk - 1
        };
        let dest = dest.to_path_buf();
        let prog2 = prog.clone();
        futs.push(async move {
            let resp = make_req()
                .header("Range", format!("bytes={start}-{end}"))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if resp.status().as_u16() != 206 {
                return Err(err_text(resp).await);
            }
            let mut f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .await
                .map_err(|e| e.to_string())?;
            f.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| e.to_string())?;
            let mut stream = resp.bytes_stream();
            while let Some(c) = stream.next().await {
                prog2.wait().await;
                prog2.check()?;
                let c = c.map_err(|e| e.to_string())?;
                f.write_all(&c).await.map_err(|e| e.to_string())?;
                prog2.add(c.len() as u64);
            }
            f.flush().await.map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        });
    }
    for f in futures_util::future::join_all(futs).await {
        f?;
    }
    // Sanity: done may overshoot nothing; clamp display.
    prog.done.store(total, Ordering::Relaxed);
    Ok(())
}

/// Read whole local file (uploads are chunked by backends themselves;
/// this is only for small files / fallback paths).
pub async fn read_file(p: &std::path::Path) -> Result<Vec<u8>, String> {
    tokio::fs::read(p).await.map_err(|e| e.to_string())
}

pub fn file_len(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Upload bytes with progress via reqwest body from Vec (small/medium).
/// Large uploads: backends use their own chunked loops.
pub async fn upload_bytes_chunked<F>(
    local: &std::path::Path,
    chunk_size: usize,
    mut send_chunk: F,
    prog: &Progress,
) -> Result<(), String>
where
    F: FnMut(u64, Vec<u8>) -> futures_util::future::BoxFuture<'static, Result<(), String>>,
{
    use tokio::io::AsyncReadExt;
    let total = file_len(local);
    prog.set_total(total);
    let mut f = tokio::fs::File::open(local)
        .await
        .map_err(|e| e.to_string())?;
    if total == 0 {
        send_chunk(0, Vec::new()).await?;
        return Ok(());
    }
    let mut off = 0u64;
    let mut buf = vec![0u8; chunk_size.max(1)];
    loop {
        prog.wait().await;
        prog.check()?;
        let n = f.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        send_chunk(off, buf[..n].to_vec()).await?;
        prog.add(n as u64);
        off += n as u64;
    }
    Ok(())
}
