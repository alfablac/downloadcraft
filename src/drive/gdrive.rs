//! Google Drive v3 REST. Auth: OAuth bearer (own client_id). Root id
//! "root" or account.root shared-drive id. Resumable chunked upload,
//! export formats not handled (Google Docs download as-is errors noted).
use super::http_util;
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};

pub struct GDrive {
    acc: std::sync::Mutex<DriveAccount>,
    client: reqwest::Client,
}

/// Parse pasted Shared Drive value: bare id, drive URL, folder URL.
/// Pure (unit tested). "root"/"" -> My Drive.
pub fn parse_drive_id(pasted: &str) -> String {
    let s = pasted.trim();
    if s.is_empty() || s == "root" {
        return String::new();
    }
    // Full folder marker first: "/drive/u/0/folders/<id>" must not cut at "u/0".
    for marker in [
        "/drive/folders/",
        "folders/",
        "/drive/u/",
        "?id=",
        "&id=",
        "id=",
    ] {
        if let Some(pos) = s.find(marker) {
            let rest = &s[pos + marker.len()..];
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !id.is_empty() {
                return id;
            }
        }
    }
    if let Some(pos) = s.find("/drives/") {
        let rest = &s[pos + "/drives/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !id.is_empty() {
            return id;
        }
    }
    // Bare id heuristic: no spaces/slashes, len>=10.
    let t = s.trim_matches('/');
    if !t.contains('/') && !t.contains(' ') && t.len() >= 10 {
        return t.to_string();
    }
    s.to_string()
}

/// Parse /drives list JSON (pure, unit tested).
pub fn parse_drives(v: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(a) = v["drives"].as_array() {
        for d in a {
            if let (Some(id), Some(name)) = (d["id"].as_str(), d["name"].as_str()) {
                out.push((id.to_string(), name.to_string()));
            }
        }
    }
    out
}

impl GDrive {
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
    fn team_id(&self) -> String {
        let a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        parse_drive_id(&a.team_drive)
    }
    fn root_folder_raw(&self) -> String {
        let a = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        parse_drive_id(&a.root)
    }
    /// (start folder id, team drive id). Legacy accounts had root =
    /// drive-or-folder conflated: probe files.get(driveId) once to split.
    async fn resolved_root(&self) -> (String, String) {
        let team = self.team_id();
        let folder = self.root_folder_raw();
        if !team.is_empty() {
            let start = if folder.is_empty() || folder == team {
                team.clone()
            } else {
                folder
            };
            return (start, team);
        }
        if folder.is_empty() {
            return ("root".into(), String::new());
        }
        // Legacy probe: folder may be in a Shared Drive.
        let url = format!(
            "https://www.googleapis.com/drive/v3/files/{}?fields=id,driveId&supportsAllDrives=true",
            super::pct(&folder)
        );
        if let Ok(resp) = self.req(reqwest::Method::GET, &url).await {
            if let Ok(resp) = resp.send().await {
                if resp.status().is_success() {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        let drive = v["driveId"].as_str().unwrap_or("").to_string();
                        return (folder, drive);
                    }
                }
            }
        }
        (folder, String::new())
    }
    fn scope_params(team: &str) -> String {
        if team.is_empty() {
            String::new()
        } else {
            format!("&corpora=drive&driveId={}", super::pct(team))
        }
    }
    /// Shared drives user can access: (id, name). Used by account picker.
    pub async fn list_shared_drives(&self) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::new();
        let mut page = String::new();
        loop {
            let mut url = "https://www.googleapis.com/drive/v3/drives?pageSize=100&fields=nextPageToken,drives(id,name)".to_string();
            if !page.is_empty() {
                url.push_str(&format!("&pageToken={}", super::pct(&page)));
            }
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
            out.extend(parse_drives(&v));
            page = v["nextPageToken"].as_str().unwrap_or("").to_string();
            if page.is_empty() {
                break;
            }
        }
        Ok(out)
    }
    async fn req(&self, m: reqwest::Method, url: &str) -> Result<reqwest::RequestBuilder, String> {
        Ok(self.client.request(m, url).bearer_auth(self.token().await?))
    }
    /// Resolve display path -> folder id (walk names). Cache-free walk.
    async fn folder_id(&self, path: &str) -> Result<String, String> {
        let (start, team) = self.resolved_root().await;
        let mut cur = start;
        let p = path.trim_matches('/');
        if p.is_empty() {
            return Ok(cur);
        }
        for seg in p.split('/') {
            let q = format!(
                "name='{}' and '{}' in parents and mimeType='application/vnd.google-apps.folder' and trashed=false",
                seg.replace('\'', "\\'"),
                cur
            );
            let url = format!(
                "https://www.googleapis.com/drive/v3/files?q={}&fields=files(id,name)&supportsAllDrives=true&includeItemsFromAllDrives=true{}",
                super::pct(&q),
                Self::scope_params(&team)
            );
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
            let id = v["files"][0]["id"]
                .as_str()
                .ok_or_else(|| format!("Google Drive folder not found: {seg} in {path}"))?;
            cur = id.to_string();
        }
        Ok(cur)
    }
}

/// Map Drive files.copy failure to user-actionable text. Pure (tested).
/// 404 here almost always means destination identity cannot see source
/// file (different account, file not shared with it): Drive hides
/// inaccessible files as not-found. Shared Drive membership also fits.
fn copy_error_hint(status: u16, file_id: &str, raw: &str) -> String {
    if status == 404 {
        format!(
            "Google Drive 404: file {file_id} not visible to destination account. \
             Server-side copy runs as destination account, so it can only copy files that \
             account can already see. Fix: in Google Drive, Share '{file_id}' (file or its \
             parent Shared Drive) with destination account, then retry. For many files, \
             share parent folder/Shared Drive once, or use normal Copy (download+upload, \
             works across accounts). Raw: {raw}"
        )
    } else {
        raw.to_string()
    }
}

/// files.copy body: keep display name, land in resolved destination folder.
fn copy_body(name: &str, parent_id: &str) -> serde_json::Value {
    serde_json::json!({"name": name, "parents": [parent_id]})
}

#[async_trait::async_trait]
impl DriveBackend for GDrive {
    fn kind(&self) -> DriveKind {
        DriveKind::GoogleDrive
    }
    async fn about(&self) -> Result<String, String> {
        let url = "https://www.googleapis.com/drive/v3/about?fields=storageQuota,user";
        let resp = self
            .req(reqwest::Method::GET, url)
            .await?
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let email = v["user"]["emailAddress"].as_str().unwrap_or("");
        let limit: u64 = v["storageQuota"]["limit"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let usage: u64 = v["storageQuota"]["usage"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Ok(format!(
            "{email} · {} / {}",
            super::fmt_bytes(usage),
            if limit > 0 {
                super::fmt_bytes(limit)
            } else {
                "unlimited".into()
            }
        ))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let parent = self.folder_id(path).await?;
        let mut out = Vec::new();
        let mut page_token = String::new();
        loop {
            let (_, team) = self.resolved_root().await;
            let q = format!("'{parent}' in parents and trashed=false");
            let mut url = format!(
                "https://www.googleapis.com/drive/v3/files?q={}&fields=nextPageToken,files(id,name,mimeType,size,modifiedTime)&pageSize=1000&supportsAllDrives=true&includeItemsFromAllDrives=true&orderBy=name{}",
                super::pct(&q),
                Self::scope_params(&team)
            );
            if !page_token.is_empty() {
                url.push_str(&format!("&pageToken={}", super::pct(&page_token)));
            }
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
            if let Some(files) = v["files"].as_array() {
                for f in files {
                    let is_dir =
                        f["mimeType"].as_str() == Some("application/vnd.google-apps.folder");
                    let name = f["name"].as_str().unwrap_or("").to_string();
                    out.push(RemoteEntry {
                        id: f["id"].as_str().unwrap_or("").into(),
                        name: name.clone(),
                        path: super::join_path(path, &name),
                        is_dir,
                        size: f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                        modified: super::parse_time(f["modifiedTime"].as_str().unwrap_or("")),
                        mime: f["mimeType"].as_str().unwrap_or("").into(),
                    });
                }
            }
            page_token = v["nextPageToken"].as_str().unwrap_or("").to_string();
            if page_token.is_empty() {
                break;
            }
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        // Idempotent: Drive create always mints new folder otherwise.
        if self.folder_id(path).await.is_ok() {
            return Ok(());
        }
        let (parent_path, name) = (super::parent_path(path), super::base_name(path));
        let parent = self.folder_id(&parent_path).await?;
        let body = serde_json::json!({"name": name, "mimeType": "application/vnd.google-apps.folder", "parents": [parent]});
        let resp = self
            .req(
                reqwest::Method::POST,
                "https://www.googleapis.com/drive/v3/files?supportsAllDrives=true",
            )
            .await?
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        // Trash (recoverable) rather than permanent delete.
        let url = format!(
            "https://www.googleapis.com/drive/v3/files/{}?supportsAllDrives=true",
            super::pct(&entry.id)
        );
        let resp = self
            .req(reqwest::Method::PATCH, &url)
            .await?
            .json(&serde_json::json!({"trashed": true}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        let url = format!(
            "https://www.googleapis.com/drive/v3/files/{}?supportsAllDrives=true",
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
        if entry.mime.starts_with("application/vnd.google-apps.") && !entry.is_dir {
            return Err(
                "Google Docs/Sheets files need export; download from drive.google.com".into(),
            );
        }
        let url = format!(
            "https://www.googleapis.com/drive/v3/files/{}?alt=media&supportsAllDrives=true",
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
        let parent = self.folder_id(dest_dir).await?;
        let size = http_util::file_len(local);
        prog.set_total(size);
        // Init resumable session.
        let init = self.req(reqwest::Method::POST, "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&supportsAllDrives=true").await?
            .header("X-Upload-Content-Length", size)
            .json(&serde_json::json!({"name": name, "parents":[parent]}))
            .send().await.map_err(|e| e.to_string())?;
        if !init.status().is_success() {
            return Err(http_util::err_text(init).await);
        }
        let session = init
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or("Drive upload: no session URL")?
            .to_string();
        const CHUNK: u64 = 8 * 1024 * 1024; // multiple of 256KiB required
        let token = self.token().await?;
        let client = self.client.clone();
        let session2 = session.clone();
        http_util::upload_bytes_chunked(
            local,
            CHUNK as usize,
            move |off, bytes| {
                let client = client.clone();
                let session = session2.clone();
                let token = token.clone();
                let end = off + bytes.len() as u64 - 1;
                Box::pin(async move {
                    let resp = client
                        .put(&session)
                        .bearer_auth(&token)
                        .header(
                            "Content-Range",
                            if bytes.is_empty() {
                                format!("bytes */{off}")
                            } else {
                                format!("bytes {off}-{end}/*")
                            },
                        )
                        .header("Content-Length", bytes.len())
                        .body(bytes)
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let st = resp.status().as_u16();
                    if st == 308 || st == 200 || st == 201 {
                        Ok(())
                    } else {
                        Err(http_util::err_text(resp).await)
                    }
                })
            },
            prog,
        )
        .await?;
        // Final id lookup by name in parent (session response body lost in chunk closure).
        let entries = self.list(dest_dir).await.unwrap_or_default();
        entries
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| "Drive upload done but file not listed".to_string())
    }
    /// Server-side copy body: keep name, land in destination folder id.
    async fn copy_from(
        &self,
        src: &std::sync::Arc<dyn DriveBackend>,
        src_entry: &RemoteEntry,
        dst_dir: &str,
        _prog: &Progress,
    ) -> Result<(), String> {
        if src.kind() != DriveKind::GoogleDrive {
            return Err("cross-provider copy: staging".into());
        }
        if src_entry.id.is_empty() {
            return Err("source file has no Drive id".into());
        }
        let parent = self.folder_id(dst_dir).await?;
        let body = copy_body(&src_entry.name, &parent);
        let url = format!(
            "https://www.googleapis.com/drive/v3/files/{}/copy?supportsAllDrives=true",
            super::pct(&src_entry.id)
        );
        let resp = self
            .req(reqwest::Method::POST, &url)
            .await?
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status().as_u16();
            Err(copy_error_hint(status, &src_entry.id, &http_util::err_text(resp).await))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drive_id_parse() {
        assert_eq!(parse_drive_id(""), "");
        assert_eq!(parse_drive_id("root"), "");
        assert_eq!(parse_drive_id("0AExampleDriveId123"), "0AExampleDriveId123");
        assert_eq!(
            parse_drive_id("https://drive.google.com/drive/folders/0AFolderId456?usp=sharing"),
            "0AFolderId456"
        );
        assert_eq!(
            parse_drive_id("https://drive.google.com/drive/u/0/folders/0AFolderId789"),
            "0AFolderId789"
        );
    }
    #[test]
    fn drives_parse() {
        let v = serde_json::json!({"drives":[{"id":"0A1","name":"Team"}]});
        assert_eq!(
            parse_drives(&v),
            vec![("0A1".to_string(), "Team".to_string())]
        );
    }
    #[test]
    fn copy_body_keeps_name_and_sets_parent() {
        let b = copy_body("movie.mkv", "0AFolder");
        assert_eq!(b["name"], "movie.mkv");
        assert_eq!(b["parents"], serde_json::json!(["0AFolder"]));
    }
    #[test]
    fn copy_error_hint_404_guides_sharing() {
        let h = copy_error_hint(404, "1LKYYrExample", "HTTP 404: File not found");
        assert!(h.contains("not visible to destination account"));
        assert!(h.contains("Share"));
        assert!(h.contains("normal Copy"));
        assert!(h.contains("1LKYYrExample"));
        let raw = copy_error_hint(403, "id", "HTTP 403: forbidden");
        assert_eq!(raw, "HTTP 403: forbidden");
    }
}
