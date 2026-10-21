//! FTP/FTPS drive backend via ftp.rs DriveFtpConn seam (fresh control
//! connection per operation; single-stream transfer — no parallel FTP).
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};

pub struct FtpDrive {
    acc: DriveAccount,
}

impl FtpDrive {
    pub fn new(acc: DriveAccount) -> Self {
        Self { acc }
    }
    async fn conn(&self) -> Result<crate::ftp::DriveFtpConn, String> {
        // explicit_tls only from impossible-to-encode flag: endpoint prefix.
        let host = self
            .acc
            .endpoint
            .trim()
            .trim_start_matches("ftp://")
            .trim_start_matches("ftps://")
            .to_string();
        let explicit = self.acc.endpoint.starts_with("ftps://");
        crate::ftp::drive_connect(
            &host,
            self.acc.port,
            &self.acc.username,
            &self.acc.secret,
            explicit,
        )
        .await
    }
    fn full(&self, path: &str) -> String {
        let root = self.acc.root.trim_matches('/');
        let p = path.trim_matches('/');
        if root.is_empty() {
            p.to_string()
        } else if p.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{p}")
        }
    }
}

#[async_trait::async_trait]
impl DriveBackend for FtpDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Ftp
    }
    async fn about(&self) -> Result<String, String> {
        Ok(format!("ftp://{}", self.acc.endpoint))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let mut c = self.conn().await?;
        let rows = c.list(&self.full(path)).await;
        c.quit().await;
        let rows = rows?;
        let mut out: Vec<RemoteEntry> = rows
            .into_iter()
            .map(|(name, is_dir, size, modified)| RemoteEntry {
                id: super::join_path(path, &name),
                path: super::join_path(path, &name),
                name,
                is_dir,
                size,
                modified,
                mime: String::new(),
            })
            .collect();
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        let mut c = self.conn().await?;
        let r = c
            .simple(&format!("MKD {}", self.full(path)), &[257, 250])
            .await;
        c.quit().await;
        r
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let mut c = self.conn().await?;
        let wire = if entry.is_dir {
            format!("RMD {}", self.full(&entry.path))
        } else {
            format!("DELE {}", self.full(&entry.path))
        };
        let r = c.simple(&wire, &[250, 200]).await;
        c.quit().await;
        r
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let mut c = self.conn().await?;
        let src = self.full(&entry.path);
        let dst = self.full(&super::join_path(
            &super::parent_path(&entry.path),
            new_name,
        ));
        let r = async {
            c.simple(&format!("RNFR {src}"), &[350]).await?;
            c.simple(&format!("RNTO {dst}"), &[250, 200]).await
        }
        .await;
        c.quit().await;
        r
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        _parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        let mut c = self.conn().await?;
        let remote = self.full(&entry.path);
        let r = c.retr_to_file(&remote, dest, prog, entry.size).await;
        c.quit().await;
        r
    }
    async fn upload(
        &self,
        local: &std::path::Path,
        dest_dir: &str,
        name: &str,
        prog: &Progress,
    ) -> Result<RemoteEntry, String> {
        let mut c = self.conn().await?;
        let remote = self.full(&super::join_path(dest_dir, name));
        let r = c.stor_file(local, &remote, prog).await;
        c.quit().await;
        r?;
        Ok(RemoteEntry {
            id: super::join_path(dest_dir, name),
            path: super::join_path(dest_dir, name),
            name: name.into(),
            is_dir: false,
            size: super::http_util::file_len(local),
            modified: 0,
            mime: String::new(),
        })
    }
}
