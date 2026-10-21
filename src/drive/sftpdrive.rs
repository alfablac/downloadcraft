//! SFTP drive backend over russh-sftp (session per operation; parallel
//! download via concurrent sessions when parallel>1 — each fetches a
//! byte range into preallocated file).
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub struct SftpDrive {
    acc: DriveAccount,
}

impl SftpDrive {
    pub fn new(acc: DriveAccount) -> Self {
        Self { acc }
    }
    async fn session(&self) -> Result<crate::sftp::SftpSession, String> {
        let host = self.acc.endpoint.trim().to_string();
        if host.is_empty() {
            return Err("SFTP host empty".into());
        }
        let port = if self.acc.port == 0 {
            22
        } else {
            self.acc.port
        };
        crate::sftp::connect_session(
            &host,
            port,
            &self.acc.username,
            &self.acc.secret,
            &self.acc.ssh_key_path,
        )
        .await
    }
    fn full(&self, path: &str) -> String {
        let root = self.acc.root.trim_end_matches('/');
        let p = path.trim_matches('/');
        if root.is_empty() {
            format!("/{p}")
        } else if p.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{p}")
        }
    }
}

#[async_trait::async_trait]
impl DriveBackend for SftpDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Sftp
    }
    async fn about(&self) -> Result<String, String> {
        Ok(format!(
            "sftp://{}@{}",
            self.acc.username, self.acc.endpoint
        ))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let s = self.session().await?;
        let dir = self.full(path);
        let rd = s.read_dir(&dir).await.map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for e in rd {
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let md = e.metadata();
            let is_dir = md.is_dir();
            let full = format!("{}/{}", dir.trim_end_matches('/'), name);
            out.push(RemoteEntry {
                id: full.clone(),
                name: name.clone(),
                path: super::join_path(path, &name),
                is_dir,
                size: if is_dir { 0 } else { md.len() },
                modified: md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                mime: String::new(),
            });
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        let s = self.session().await?;
        s.create_dir(self.full(path))
            .await
            .map_err(|e| e.to_string())
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let s = self.session().await?;
        let p = self.full(&entry.path);
        if entry.is_dir {
            // Recursive.
            let mut stack = vec![p.clone()];
            let mut all: Vec<String> = Vec::new();
            while let Some(d) = stack.pop() {
                let rd = s.read_dir(&d).await.map_err(|e| e.to_string())?;
                for e in rd {
                    let name = e.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let fp = format!("{}/{}", d.trim_end_matches('/'), name);
                    all.push(fp.clone());
                    if e.metadata().is_dir() {
                        stack.push(fp);
                    }
                }
            }
            for fp in all.iter().rev() {
                let _ = s.remove_file(fp).await;
                let _ = s.remove_dir(fp).await;
            }
            s.remove_dir(&p).await.map_err(|e| e.to_string())
        } else {
            s.remove_file(&p).await.map_err(|e| e.to_string())
        }
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let s = self.session().await?;
        let src = self.full(&entry.path);
        let dst = self.full(&super::join_path(
            &super::parent_path(&entry.path),
            new_name,
        ));
        s.rename(&src, &dst).await.map_err(|e| e.to_string())
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        prog.set_total(entry.size);
        if parallel <= 1 || entry.size < 1 << 20 {
            let s = self.session().await?;
            let mut rf = s
                .open(self.full(&entry.path))
                .await
                .map_err(|e| e.to_string())?;
            if let Some(p) = dest.parent() {
                tokio::fs::create_dir_all(p)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let mut lf = tokio::fs::File::create(dest)
                .await
                .map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                prog.wait().await;
                prog.check()?;
                let n = rf.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                lf.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
                prog.add(n as u64);
            }
            lf.flush().await.map_err(|e| e.to_string())?;
            return Ok(());
        }
        // Parallel ranges over concurrent sessions (true multithreaded SFTP).
        if let Some(p) = dest.parent() {
            tokio::fs::create_dir_all(p)
                .await
                .map_err(|e| e.to_string())?;
        }
        let f = tokio::fs::File::create(dest)
            .await
            .map_err(|e| e.to_string())?;
        f.set_len(entry.size).await.map_err(|e| e.to_string())?;
        drop(f);
        let nseg = parallel.min(8);
        let chunk = entry.size / nseg as u64;
        let mut futs = Vec::new();
        for i in 0..nseg {
            let start = i as u64 * chunk;
            let end = if i + 1 == nseg {
                entry.size
            } else {
                start + chunk
            };
            let acc = self.acc.clone();
            let remote = self.full(&entry.path);
            let dest = dest.to_path_buf();
            let prog = prog.clone();
            futs.push(async move {
                let drv = SftpDrive { acc };
                let s = drv.session().await?;
                let mut rf = s.open(&remote).await.map_err(|e| e.to_string())?;
                rf.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| e.to_string())?;
                let mut lf = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&dest)
                    .await
                    .map_err(|e| e.to_string())?;
                lf.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; 256 * 1024];
                let mut pos = start;
                while pos < end {
                    prog.wait().await;
                    prog.check()?;
                    let want = ((end - pos) as usize).min(buf.len());
                    let n = rf.read(&mut buf[..want]).await.map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    lf.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
                    prog.add(n as u64);
                    pos += n as u64;
                }
                lf.flush().await.map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            });
        }
        for f in futures_util::future::join_all(futs).await {
            f?;
        }
        Ok(())
    }
    async fn upload(
        &self,
        local: &std::path::Path,
        dest_dir: &str,
        name: &str,
        prog: &Progress,
    ) -> Result<RemoteEntry, String> {
        let size = super::http_util::file_len(local);
        prog.set_total(size);
        let s = self.session().await?;
        let remote = self.full(&super::join_path(dest_dir, name));
        let mut wf = s.create(&remote).await.map_err(|e| e.to_string())?;
        let mut lf = tokio::fs::File::open(local)
            .await
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            prog.wait().await;
            prog.check()?;
            let n = lf.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            wf.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
            prog.add(n as u64);
        }
        wf.flush().await.map_err(|e| e.to_string())?;
        wf.shutdown().await.map_err(|e| e.to_string())?;
        Ok(RemoteEntry {
            id: remote.clone(),
            name: name.into(),
            path: super::join_path(dest_dir, name),
            is_dir: false,
            size,
            modified: 0,
            mime: String::new(),
        })
    }
}
