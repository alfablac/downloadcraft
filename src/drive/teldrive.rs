//! Teldrive backend: user-hosted tgdrive/teldrive server REST /v1.
//! Auth: pasted API key (X-Api-Key) or bearer token in account.secret
//! ("api_key:..." prefix distinguishes; bare = bearer). Resumable
//! session uploads (create -> PUT parts -> complete), ranged downloads.
use super::http_util;
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};

pub struct Teldrive {
    acc: DriveAccount,
    client: reqwest::Client,
}

impl Teldrive {
    pub fn new(acc: DriveAccount) -> Self {
        Self {
            client: http_util::client(),
            acc,
        }
    }
    fn base(&self) -> String {
        self.acc.endpoint.trim().trim_end_matches('/').to_string()
    }
    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base(), path);
        let mut r = self.client.request(m, &url);
        let s = self.acc.secret.trim();
        if let Some(k) = s.strip_prefix("api_key:") {
            r = r.header("X-Api-Key", k.trim());
        } else if !s.is_empty() {
            r = r.bearer_auth(s);
        }
        // Username field may hold pasted access token alternatively.
        r
    }
    fn folder_path(path: &str) -> String {
        let p = path.trim_matches('/');
        if p.is_empty() {
            "/".into()
        } else {
            format!("/{p}")
        }
    }
}

#[async_trait::async_trait]
impl DriveBackend for Teldrive {
    fn kind(&self) -> DriveKind {
        DriveKind::Teldrive
    }
    async fn about(&self) -> Result<String, String> {
        let resp = self.req(reqwest::Method::GET, "/v1/me").send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let name = v["name"].as_str().or_else(|| v["username"].as_str()).unwrap_or("teldrive");
        Ok(format!("teldrive · {name} · {}", self.base()))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let p = Self::folder_path(path);
        let url_path = format!("/v1/files?path={}&scope=folder&limit=500", super::pct(&p));
        let resp = self.req(reqwest::Method::GET, &url_path).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let items = v["items"].as_array().or_else(|| v["files"].as_array()).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for f in items {
            let is_dir = f["type"].as_str() == Some("folder") || f["isDir"].as_bool() == Some(true);
            let name = f["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            out.push(RemoteEntry {
                id: f["id"].as_str().unwrap_or("").into(),
                name: name.clone(),
                path: super::join_path(path, &name),
                is_dir,
                size: f["size"].as_u64().unwrap_or(0),
                modified: super::parse_time(
                    f["updatedAt"].as_str().or_else(|| f["createdAt"].as_str()).unwrap_or(""),
                ),
                mime: f["mimeType"].as_str().unwrap_or("").into(),
            });
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        let parent = super::parent_path(path);
        let name = super::base_name(path);
        let resp = self.req(reqwest::Method::POST, "/v1/folders")
            .json(&serde_json::json!({"name": name, "path": Self::folder_path(&parent)}))
            .send().await.map_err(|e| e.to_string())?;
        if resp.status().is_success() { Ok(()) } else { Err(http_util::err_text(resp).await) }
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let resp = self.req(reqwest::Method::DELETE, &format!("/v1/files/{}", super::pct(&entry.id)))
            .send().await.map_err(|e| e.to_string())?;
        if resp.status().is_success() || resp.status().as_u16() == 404 { Ok(()) } else { Err(http_util::err_text(resp).await) }
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let resp = self.req(reqwest::Method::PATCH, &format!("/v1/files/{}", super::pct(&entry.id)))
            .json(&serde_json::json!({"name": new_name}))
            .send().await.map_err(|e| e.to_string())?;
        if resp.status().is_success() { Ok(()) } else { Err(http_util::err_text(resp).await) }
    }
    async fn download(&self, entry: &RemoteEntry, dest: &std::path::Path, parallel: u32, prog: &Progress) -> Result<(), String> {
        let path = format!("/v1/files/{}/content/{}?download=1", super::pct(&entry.id), super::pct(&entry.name));
        let base = self.base();
        let secret = self.acc.secret.clone();
        let client = self.client.clone();
        let mk = move || {
            let mut r = client.get(format!("{base}{path}"));
            let s = secret.trim();
            if let Some(k) = s.strip_prefix("api_key:") {
                r = r.header("X-Api-Key", k.trim());
            } else if !s.is_empty() {
                r = r.bearer_auth(s);
            }
            r
        };
        http_util::download_url(&self.client, &mk, dest, parallel, prog).await
    }
    async fn upload(&self, local: &std::path::Path, dest_dir: &str, name: &str, prog: &Progress) -> Result<RemoteEntry, String> {
        const PART: u64 = 512 * 1024 * 1024;
        let size = http_util::file_len(local);
        prog.set_total(size);
        // Resolve dest folder id via list parent chain: teldrive create
        // upload with path hint (v1 accepts fileName + path fields).
        let resp = self.req(reqwest::Method::POST, "/v1/uploads")
            .json(&serde_json::json!({
                "fileName": name,
                "path": Self::folder_path(dest_dir),
                "totalParts": size.div_ceil(PART).max(1),
                "totalSize": size
            }))
            .send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let upload_id = v["id"].as_str().or_else(|| v["uploadId"].as_str()).ok_or("teldrive: no upload id")?.to_string();
        let data = tokio::fs::read(local).await.map_err(|e| e.to_string())?;
        // ponytail: RAM read for parts; part size 512MiB default per teldrive.
        let mut off = 0usize;
        let mut part_no = 1u64;
        if data.is_empty() {
            let resp = self.req(reqwest::Method::PUT, &format!("/v1/uploads/{upload_id}/parts/1"))
                .body(Vec::<u8>::new()).send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
        }
        while off < data.len() {
            prog.wait().await;
            prog.check()?;
            let end = (off + PART as usize).min(data.len());
            let resp = self.req(reqwest::Method::PUT, &format!("/v1/uploads/{upload_id}/parts/{part_no}"))
                .body(data[off..end].to_vec())
                .send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
            prog.add((end - off) as u64);
            off = end;
            part_no += 1;
        }
        let resp = self.req(reqwest::Method::POST, &format!("/v1/uploads/{upload_id}/complete"))
            .json(&serde_json::json!({}))
            .send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        Ok(RemoteEntry {
            id: String::new(),
            name: name.into(),
            path: super::join_path(dest_dir, name),
            is_dir: false,
            size,
            modified: 0,
            mime: String::new(),
        })
    }
}
