//! Local filesystem backend (dual-pane local side; root = account.endpoint or root).
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};
use std::path::PathBuf;

pub struct LocalDrive {
    pub root: PathBuf,
}

impl LocalDrive {
    pub fn new(acc: DriveAccount) -> Self {
        let base = if !acc.root.is_empty() {
            acc.root.clone()
        } else if !acc.endpoint.is_empty() {
            acc.endpoint.clone()
        } else {
            "/".into()
        };
        Self {
            root: PathBuf::from(base),
        }
    }
    fn resolve(&self, path: &str) -> PathBuf {
        if path.is_empty() || path == "/" {
            return self.root.clone();
        }
        // Absolute local path passthrough (pane nav stores absolute ids).
        let p = PathBuf::from(path);
        if p.is_absolute() {
            return p;
        }
        self.root.join(path)
    }
}

#[async_trait::async_trait]
impl DriveBackend for LocalDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Local
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let dir = self.resolve(path);
        super::list_local_dir(&dir)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        tokio::fs::create_dir_all(self.resolve(path))
            .await
            .map_err(|e| e.to_string())
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let p = self.resolve(&entry.id);
        if entry.is_dir {
            tokio::fs::remove_dir_all(p)
                .await
                .map_err(|e| e.to_string())
        } else {
            tokio::fs::remove_file(p).await.map_err(|e| e.to_string())
        }
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let src = self.resolve(&entry.id);
        let dst = src
            .parent()
            .map(|p| p.join(new_name))
            .ok_or("no parent")?;
        tokio::fs::rename(src, dst).await.map_err(|e| e.to_string())
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        _parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        prog.set_total(entry.size);
        let src = self.resolve(&entry.id);
        let dst = dest.to_path_buf();
        let prog = prog.clone();
        let size = entry.size;
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
            prog.add(size);
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn upload(
        &self,
        local: &std::path::Path,
        dest_dir: &str,
        name: &str,
        prog: &Progress,
    ) -> Result<RemoteEntry, String> {
        let dir = self.resolve(dest_dir);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let dst = dir.join(name);
        let size = super::http_util::file_len(local);
        prog.set_total(size);
        let src = local.to_path_buf();
        let dst2 = dst.clone();
        tokio::task::spawn_blocking(move || std::fs::copy(&src, &dst2))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        prog.add(size);
        Ok(RemoteEntry {
            id: dst.to_string_lossy().into_owned(),
            name: name.to_string(),
            path: super::join_path(dest_dir, name),
            is_dir: false,
            size,
            modified: 0,
            mime: String::new(),
        })
    }
}
