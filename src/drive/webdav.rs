//! WebDAV backend: PROPFIND list, GET (ranged parallel), PUT upload,
//! MKCOL, DELETE, MOVE. Basic auth (app passwords for Nextcloud/ownCloud).
use super::http_util;
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};

pub struct WebDav {
    acc: DriveAccount,
    client: reqwest::Client,
}

impl WebDav {
    pub fn new(acc: DriveAccount) -> Self {
        Self {
            client: http_util::client(),
            acc,
        }
    }
    fn base(&self) -> String {
        let e = if !self.acc.root.is_empty() && self.acc.endpoint.is_empty() {
            self.acc.root.clone()
        } else {
            self.acc.endpoint.clone()
        };
        e.trim_end_matches('/').to_string()
    }
    fn url_for(&self, path: &str) -> String {
        let b = self.base();
        let p = path.trim_matches('/');
        if p.is_empty() {
            format!("{b}/")
        } else {
            format!("{b}/{}", super::pct_path(p))
        }
    }
    fn req(&self, m: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut r = self.client.request(m, url);
        if !self.acc.username.is_empty() {
            r = r.basic_auth(&self.acc.username, Some(&self.acc.secret));
        }
        r
    }
}

/// Parse multistatus XML -> entries (pure, tested).
pub fn parse_propfind(xml: &str, base_url: &str, cur_path: &str) -> Vec<RemoteEntry> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let base_path = url::Url::parse(base_url)
        .map(|u| u.path().trim_end_matches('/').to_string())
        .unwrap_or_default();
    let mut out = Vec::new();
    for node in doc.descendants() {
        if node.tag_name().name() != "response" {
            continue;
        }
        let href = node
            .descendants()
            .find(|n| n.tag_name().name() == "href")
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string();
        let decoded = pct_decode(&href);
        // Strip base path prefix -> relative path.
        let rel = decoded
            .strip_prefix(&base_path)
            .unwrap_or(&decoded)
            .trim_matches('/')
            .to_string();
        if rel == cur_path.trim_matches('/') {
            continue; // self
        }
        let is_dir = node
            .descendants()
            .any(|n| n.tag_name().name() == "collection");
        let size: u64 = node
            .descendants()
            .find(|n| n.tag_name().name() == "getcontentlength")
            .and_then(|n| n.text())
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        let modified = node
            .descendants()
            .find(|n| n.tag_name().name() == "getlastmodified")
            .and_then(|n| n.text())
            .map(parse_http_date)
            .unwrap_or(0);
        let mime = node
            .descendants()
            .find(|n| n.tag_name().name() == "getcontenttype")
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string();
        let name = super::base_name(&rel);
        if name.is_empty() {
            continue;
        }
        out.push(RemoteEntry {
            id: rel.clone(),
            name,
            path: rel,
            is_dir,
            size: if is_dir { 0 } else { size },
            modified,
            mime,
        });
    }
    super::sort_entries(&mut out);
    out
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "Mon, 02 Jan 2006 15:04:05 GMT" -> unix secs (tiny parser).
fn parse_http_date(s: &str) -> u64 {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return super::parse_time(s);
    }
    let mon = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return 0,
    };
    let iso = format!("{}-{:02}-{}T{}Z", parts[3], mon, parts[1], parts[4]);
    super::parse_time(&iso)
}

#[async_trait::async_trait]
impl DriveBackend for WebDav {
    fn kind(&self) -> DriveKind {
        DriveKind::WebDav
    }
    async fn about(&self) -> Result<String, String> {
        Ok(format!("WebDAV {}", self.base()))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let url = self.url_for(path);
        let resp = self
            .req(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body("<?xml version=\"1.0\"?><propfind xmlns=\"DAV:\"><prop><displayname/><getcontentlength/><getlastmodified/><getcontenttype/><resourcetype/></prop></propfind>")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() && resp.status().as_u16() != 207 {
            return Err(http_util::err_text(resp).await);
        }
        let xml = resp.text().await.map_err(|e| e.to_string())?;
        Ok(parse_propfind(&xml, &self.base(), path))
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        let url = self.url_for(path);
        let resp = self
            .req(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() || resp.status().as_u16() == 405 {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        let url = self.url_for(&entry.path);
        let resp = self
            .req(reqwest::Method::DELETE, &url)
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
        let src = self.url_for(&entry.path);
        let dst_path = super::join_path(&super::parent_path(&entry.path), new_name);
        let dst = self.url_for(&dst_path);
        let resp = self
            .req(reqwest::Method::from_bytes(b"MOVE").unwrap(), &src)
            .header("Destination", dst)
            .header("Overwrite", "T")
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
        let url = self.url_for(&entry.path);
        let client = self.client.clone();
        let user = self.acc.username.clone();
        let pass = self.acc.secret.clone();
        let mk = move || {
            let mut r = client.get(&url);
            if !user.is_empty() {
                r = r.basic_auth(&user, Some(&pass));
            }
            r
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
        let path = super::join_path(dest_dir, name);
        let url = self.url_for(&path);
        let size = http_util::file_len(local);
        prog.set_total(size);
        // Stream file as body (reqwest file body via tokio file stream).
        let f = tokio::fs::File::open(local)
            .await
            .map_err(|e| e.to_string())?;
        let stream = futures_util::stream::unfold((f, prog.clone()), |(mut f, prog)| async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 256 * 1024];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    prog.add(n as u64);
                    buf.truncate(n);
                    Some((Ok::<_, std::io::Error>(buf), (f, prog)))
                }
                Err(e) => Some((Err(e), (f, prog))),
            }
        });
        let resp = self
            .req(reqwest::Method::PUT, &url)
            .header("Content-Length", size)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        Ok(RemoteEntry {
            id: path.clone(),
            name: name.to_string(),
            path,
            is_dir: false,
            size,
            modified: 0,
            mime: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn propfind_parse() {
        let xml = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
        <d:response><d:href>/remote.php/dav/files/u/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
        <d:response><d:href>/remote.php/dav/files/u/a%20b.txt</d:href><d:propstat><d:prop><d:getcontentlength>123</d:getcontentlength><d:getlastmodified>Mon, 01 Jan 2024 00:00:00 GMT</d:getlastmodified><d:resourcetype/></d:prop></d:propstat></d:response>
        <d:response><d:href>/remote.php/dav/files/u/sub/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
        </d:multistatus>"#;
        let v = parse_propfind(xml, "https://h/remote.php/dav/files/u", "");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "sub");
        assert!(v[0].is_dir);
        assert_eq!(v[1].name, "a b.txt");
        assert_eq!(v[1].size, 123);
        assert_eq!(v[1].modified, 1704067200);
    }
    #[test]
    fn http_date() {
        assert_eq!(parse_http_date("Mon, 01 Jan 2024 00:00:00 GMT"), 1704067200);
    }
}
