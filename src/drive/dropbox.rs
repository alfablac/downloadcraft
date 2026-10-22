//! Dropbox API v2. Path-based (root = account.root prefix), cursor
//! listing, chunked upload sessions (8MiB), download via /download.
use super::http_util;
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};

pub struct Dropbox {
    acc: std::sync::Mutex<DriveAccount>,
    client: reqwest::Client,
}

impl Dropbox {
    pub fn new(acc: DriveAccount) -> Self {
        Self {
            client: http_util::client(),
            acc: std::sync::Mutex::new(acc),
        }
    }
    async fn token(&self) -> Result<String, String> {
        let mut acc = self.acc.lock().unwrap_or_else(|e| e.into_inner()).clone();
        super::oauth::ensure_fresh(&mut acc).await?;
        *self.acc.lock().unwrap_or_else(|e| e.into_inner()) = acc.clone();
        Ok(acc.access_token)
    }
    fn full(&self, path: &str) -> String {
        let root = self
            .acc
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .root
            .clone();
        let root = root.trim_matches('/');
        let p = path.trim_matches('/');
        let joined = if root.is_empty() {
            p.to_string()
        } else if p.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{p}")
        };
        if joined.is_empty() {
            String::new()
        } else {
            format!("/{joined}")
        }
    }
    async fn rpc(&self, ep: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let resp = self
            .client
            .post(format!("https://api.dropboxapi.com/2/{ep}"))
            .bearer_auth(self.token().await?)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        resp.json().await.map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl DriveBackend for Dropbox {
    fn kind(&self) -> DriveKind {
        DriveKind::Dropbox
    }
    async fn about(&self) -> Result<String, String> {
        let v = self
            .rpc("users/get_space_usage", serde_json::Value::Null)
            .await?;
        let used = v["used"].as_u64().unwrap_or(0);
        let total = v["allocation"]["allocated"].as_u64().unwrap_or(0);
        Ok(format!(
            "{} / {}",
            super::fmt_bytes(used),
            super::fmt_bytes(total)
        ))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let mut v = self
            .rpc(
                "files/list_folder",
                serde_json::json!({"path": self.full(path), "recursive": false, "limit": 2000}),
            )
            .await?;
        let mut out = Vec::new();
        loop {
            if let Some(entries) = v["entries"].as_array() {
                for f in entries {
                    let tag = f[".tag"].as_str().unwrap_or("");
                    let name = f["name"].as_str().unwrap_or("").to_string();
                    out.push(RemoteEntry {
                        id: f["id"].as_str().unwrap_or("").into(),
                        name: name.clone(),
                        path: super::join_path(path, &name),
                        is_dir: tag == "folder",
                        size: f["size"].as_u64().unwrap_or(0),
                        modified: super::parse_time(f["server_modified"].as_str().unwrap_or("")),
                        mime: String::new(),
                    });
                }
            }
            if v["has_more"].as_bool() == Some(true) {
                let cursor = v["cursor"].as_str().unwrap_or("").to_string();
                v = self
                    .rpc(
                        "files/list_folder/continue",
                        serde_json::json!({"cursor": cursor}),
                    )
                    .await?;
            } else {
                break;
            }
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        self.rpc(
            "files/create_folder_v2",
            serde_json::json!({"path": self.full(path), "autorename": false}),
        )
        .await?;
        Ok(())
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        self.rpc(
            "files/delete_v2",
            serde_json::json!({"path": self.full(&entry.path)}),
        )
        .await?;
        Ok(())
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let to = super::join_path(&super::parent_path(&entry.path), new_name);
        self.rpc("files/move_v2", serde_json::json!({"from_path": self.full(&entry.path), "to_path": self.full(&to), "autorename": false})).await?;
        Ok(())
    }
    /// Stream source: 4h temporary link (self-authenticating).
    /// `/files/download` is POST-only, mpv cannot use it.
    async fn stream_url(&self, entry: &RemoteEntry) -> Result<super::StreamSource, String> {
        let v = self
            .rpc("files/get_temporary_link", serde_json::json!({"path": self.full(&entry.path)}))
            .await?;
        let link = v["link"].as_str().filter(|s| !s.is_empty()).ok_or("Dropbox: no temporary link")?;
        Ok(super::StreamSource { url: link.to_string(), headers: Vec::new() })
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        let token = self.token().await?;
        let client = self.client.clone();
        let arg = serde_json::json!({"path": self.full(&entry.path)}).to_string();
        let mk = move || {
            client
                .post("https://content.dropboxapi.com/2/files/download")
                .bearer_auth(&token)
                .header("Dropbox-API-Arg", &arg)
        };
        http_util::download_url(&self.client, &mk, dest, parallel, prog).await
    }
    async fn upload(
        &self,
        local: &std::path::Path,
        dest_dir: &str,
        name: &str,
        prog: &Progress,
    ) -> Result<RemoteEntry, String> {
        let size = http_util::file_len(local);
        prog.set_total(size);
        let path = super::join_path(dest_dir, name);
        let full = self.full(&path);
        let token = self.token().await?;
        if size <= 8 * 1024 * 1024 {
            let bytes = http_util::read_file(local).await?;
            prog.add(size);
            let resp = self.client.post("https://content.dropboxapi.com/2/files/upload")
                .bearer_auth(&token)
                .header("Dropbox-API-Arg", serde_json::json!({"path": full, "mode": "overwrite", "autorename": false, "mute": true}).to_string())
                .header("Content-Type", "application/octet-stream")
                .body(bytes)
                .send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
            let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            return Ok(RemoteEntry {
                id: v["id"].as_str().unwrap_or("").into(),
                name: name.into(),
                path,
                is_dir: false,
                size,
                modified: super::parse_time(v["server_modified"].as_str().unwrap_or("")),
                mime: String::new(),
            });
        }
        // Session: start with first 8MiB, append_v2, finish.
        let data = tokio::fs::read(local).await.map_err(|e| e.to_string())?;
        // ponytail: session path loads file in RAM; chunked-file streaming
        // adds later if giant Dropbox uploads OOM.
        const CHUNK: usize = 8 * 1024 * 1024;
        let resp = self
            .client
            .post("https://content.dropboxapi.com/2/files/upload_session/start")
            .bearer_auth(&token)
            .header(
                "Dropbox-API-Arg",
                serde_json::json!({"close": false}).to_string(),
            )
            .header("Content-Type", "application/octet-stream")
            .body(data[..CHUNK.min(data.len())].to_vec())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let session = v["session_id"]
            .as_str()
            .ok_or("Dropbox: no session")?
            .to_string();
        prog.add(CHUNK.min(data.len()) as u64);
        let mut off = CHUNK.min(data.len());
        while off < data.len() {
            prog.wait().await;
            prog.check()?;
            let end = (off + CHUNK).min(data.len());
            let last = end == data.len();
            let (ep, arg) = if last {
                (
                    "files/upload_session/finish",
                    serde_json::json!({
                        "cursor": {"session_id": session, "offset": off},
                        "commit": {"path": full, "mode": "overwrite", "autorename": false, "mute": true}
                    }),
                )
            } else {
                (
                    "files/upload_session/append_v2",
                    serde_json::json!({
                        "cursor": {"session_id": session, "offset": off}, "close": false
                    }),
                )
            };
            let resp = self
                .client
                .post(format!("https://content.dropboxapi.com/2/{ep}"))
                .bearer_auth(&token)
                .header("Dropbox-API-Arg", arg.to_string())
                .header("Content-Type", "application/octet-stream")
                .body(data[off..end].to_vec())
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
            prog.add((end - off) as u64);
            off = end;
        }
        Ok(RemoteEntry {
            id: String::new(),
            name: name.into(),
            path,
            is_dir: false,
            size,
            modified: 0,
            mime: String::new(),
        })
    }
}
