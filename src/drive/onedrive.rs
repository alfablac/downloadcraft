//! OneDrive via Microsoft Graph. Path-based addressing under
//! /me/drive/root. Upload: simple PUT <4MiB, session chunks above.
use super::http_util;
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};

pub struct OneDrive {
    acc: std::sync::Mutex<DriveAccount>,
    client: reqwest::Client,
}

fn item_url(path: &str) -> String {
    let p = path.trim_matches('/');
    if p.is_empty() {
        "https://graph.microsoft.com/v1.0/me/drive/root/children".into()
    } else {
        format!(
            "https://graph.microsoft.com/v1.0/me/drive/root:/{}:/children",
            super::pct_path(p)
        )
    }
}
fn file_url(path: &str) -> String {
    let p = path.trim_matches('/');
    if p.is_empty() {
        "https://graph.microsoft.com/v1.0/me/drive/root".into()
    } else {
        format!(
            "https://graph.microsoft.com/v1.0/me/drive/root:/{}",
            super::pct_path(p)
        )
    }
}

impl OneDrive {
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
    async fn req(&self, m: reqwest::Method, url: &str) -> Result<reqwest::RequestBuilder, String> {
        Ok(self.client.request(m, url).bearer_auth(self.token().await?))
    }
}

#[async_trait::async_trait]
impl DriveBackend for OneDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::OneDrive
    }
    async fn about(&self) -> Result<String, String> {
        let resp = self
            .req(
                reqwest::Method::GET,
                "https://graph.microsoft.com/v1.0/me/drive?$select=quota,owner",
            )
            .await?
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let used = v["quota"]["used"].as_u64().unwrap_or(0);
        let total = v["quota"]["total"].as_u64().unwrap_or(0);
        Ok(format!(
            "{} / {}",
            super::fmt_bytes(used),
            super::fmt_bytes(total)
        ))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let mut out = Vec::new();
        let mut url = item_url(path);
        loop {
            let resp = self
                .req(reqwest::Method::GET, &url)
                .await?
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
            let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
            if let Some(items) = v["value"].as_array() {
                for f in items {
                    let is_dir = f.get("folder").is_some();
                    let name = f["name"].as_str().unwrap_or("").to_string();
                    out.push(RemoteEntry {
                        id: f["id"].as_str().unwrap_or("").into(),
                        name: name.clone(),
                        path: super::join_path(path, &name),
                        is_dir,
                        size: f["size"].as_u64().unwrap_or(0),
                        modified: super::parse_time(
                            f["lastModifiedDateTime"].as_str().unwrap_or(""),
                        ),
                        mime: f["file"]["mimeType"].as_str().unwrap_or("").into(),
                    });
                }
            }
            match v["@odata.nextLink"].as_str() {
                Some(n) if !n.is_empty() => url = n.to_string(),
                _ => break,
            }
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        let (parent, name) = (super::parent_path(path), super::base_name(path));
        let url = item_url(&parent);
        let resp = self.req(reqwest::Method::POST, &url).await?
            .json(&serde_json::json!({"name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail"}))
            .send().await.map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}",
            super::pct(&entry.id)
        );
        let resp = self
            .req(reqwest::Method::DELETE, &url)
            .await?
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() || resp.status().as_u16() == 404 {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}",
            super::pct(&entry.id)
        );
        let resp = self
            .req(reqwest::Method::PATCH, &url)
            .await?
            .json(&serde_json::json!({"name": new_name}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}/content",
            super::pct(&entry.id)
        );
        let token = self.token().await?;
        let client = self.client.clone();
        let mk = move || client.get(&url).bearer_auth(&token);
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
        if size <= 4 * 1024 * 1024 {
            let bytes = http_util::read_file(local).await?;
            let url = format!("{}:/content", file_url(&path));
            let resp = self
                .req(reqwest::Method::PUT, &url)
                .await?
                .body(bytes)
                .send()
                .await
                .map_err(|e| e.to_string())?;
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
                modified: super::parse_time(v["lastModifiedDateTime"].as_str().unwrap_or("")),
                mime: String::new(),
            });
        }
        // Upload session, 5MiB chunks (multiple of 320KiB required).
        let url = format!("{}:/createUploadSession", file_url(&path));
        let resp = self
            .req(reqwest::Method::POST, &url)
            .await?
            .json(&serde_json::json!({"item": {"@microsoft.graph.conflictBehavior": "replace"}}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let session = v["uploadUrl"]
            .as_str()
            .ok_or("OneDrive: no uploadUrl")?
            .to_string();
        const CHUNK: u64 = 5 * 1024 * 1024;
        let client = self.client.clone();
        let session2 = session.clone();
        let total_for_range = size;
        http_util::upload_bytes_chunked(
            local,
            CHUNK as usize,
            move |off, bytes| {
                let client = client.clone();
                let session = session2.clone();
                let end = off + bytes.len() as u64;
                Box::pin(async move {
                    let resp = client
                        .put(&session)
                        .header(
                            "Content-Range",
                            format!(
                                "bytes {}-{}/{}",
                                off,
                                end.saturating_sub(1),
                                total_for_range
                            ),
                        )
                        .header("Content-Length", bytes.len())
                        .body(bytes)
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let st = resp.status().as_u16();
                    if st == 202 || st == 200 || st == 201 {
                        Ok(())
                    } else {
                        Err(http_util::err_text(resp).await)
                    }
                })
            },
            prog,
        )
        .await?;
        let entries = self.list(dest_dir).await.unwrap_or_default();
        entries
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| "OneDrive upload done but not listed".to_string())
    }
}
