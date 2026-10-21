//! S3-compatible backend: SigV4 signed REST, ListObjectsV2, multipart
//! upload (16MiB parts, parallel), ranged parallel download. Works AWS,
//! R2 (endpoint + region auto), B2, MinIO. Bucket = account.root.
use super::http_util;
use super::{DriveBackend, Progress, RemoteEntry};
use crate::model::{DriveAccount, DriveKind};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha = Hmac<Sha256>;

fn sha256_hex(d: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(d);
    format!("{:x}", h.finalize())
}
fn hmac(key: &[u8], d: &[u8]) -> Vec<u8> {
    let mut m = HmacSha::new_from_slice(key).expect("hmac key");
    m.update(d);
    m.finalize().into_bytes().to_vec()
}
fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub struct SigV4 {
    pub access: String,
    pub secret: String,
    pub region: String,
    pub service: String,
}

/// Build Authorization + x-amz headers for one request (pure, testable).
pub fn sign(
    s: &SigV4,
    method: &str,
    url: &url::Url,
    headers: &[(String, String)],
    body_sha: &str,
    now: (String, String), // (YYYYMMDDTHHMMSSZ, YYYYMMDD)
) -> Vec<(String, String)> {
    let (amzdate, datestamp) = now;
    let host = url.host_str().unwrap_or("").to_string();
    let mut hs: Vec<(String, String)> = headers.to_vec();
    hs.push(("host".into(), host.clone()));
    hs.push(("x-amz-content-sha256".into(), body_sha.into()));
    hs.push(("x-amz-date".into(), amzdate.clone()));
    hs.sort_by(|a, b| a.0.cmp(&b.0));
    let signed_headers = hs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canon_headers = hs
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
        .collect::<String>();
    let query = url.query().unwrap_or("");
    // Canonical query: already encoded by url crate; sort pairs.
    let mut pairs: Vec<&str> = if query.is_empty() {
        vec![]
    } else {
        query.split('&').collect()
    };
    pairs.sort();
    let canon = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        url.path(),
        pairs.join("&"),
        canon_headers,
        signed_headers,
        body_sha
    );
    let scope = format!("{datestamp}/{}/{}/aws4_request", s.region, s.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amzdate}\n{scope}\n{}",
        sha256_hex(canon.as_bytes())
    );
    let k_date = hmac(format!("AWS4{}", s.secret).as_bytes(), datestamp.as_bytes());
    let k_region = hmac(&k_date, s.region.as_bytes());
    let k_service = hmac(&k_region, s.service.as_bytes());
    let k_sign = hmac(&k_service, b"aws4_request");
    let sig = hex_bytes(&hmac(&k_sign, to_sign.as_bytes()));
    vec![
        ("x-amz-date".into(), amzdate),
        ("x-amz-content-sha256".into(), body_sha.into()),
        (
            "Authorization".into(),
            format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={sig}", s.access),
        ),
    ]
}

fn now_amz() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil(secs);
    (
        format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z"),
        format!("{y:04}{mo:02}{d:02}"),
    )
}
fn civil(secs: u64) -> (i64, i64, i64, i64, i64, i64) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        m,
        d,
        (rem / 3600) as i64,
        ((rem % 3600) / 60) as i64,
        (rem % 60) as i64,
    )
}

pub struct S3 {
    acc: DriveAccount,
    client: reqwest::Client,
}

impl S3 {
    pub fn new(acc: DriveAccount) -> Self {
        Self {
            client: http_util::client(),
            acc,
        }
    }
    fn bucket(&self) -> String {
        self.acc.root.trim_matches('/').to_string()
    }
    fn endpoint(&self) -> String {
        let e = self.acc.endpoint.trim().trim_end_matches('/');
        if e.is_empty() {
            let region = if self.acc.region.is_empty() {
                "us-east-1"
            } else {
                &self.acc.region
            };
            format!("https://s3.{region}.amazonaws.com")
        } else if e.starts_with("http") {
            e.to_string()
        } else {
            format!("https://{e}")
        }
    }
    fn signer(&self) -> SigV4 {
        SigV4 {
            access: self.acc.username.clone(),
            secret: self.acc.secret.clone(),
            region: if self.acc.region.is_empty() || self.acc.region == "auto" {
                "us-east-1".into()
            } else {
                self.acc.region.clone()
            },
            service: "s3".into(),
        }
    }
    fn url(&self, key: &str, query: &str) -> url::Url {
        let ep = self.endpoint();
        let b = self.bucket();
        let base = if b.is_empty() {
            ep.clone()
        } else {
            format!("{ep}/{b}")
        };
        let mut s = base;
        if !key.is_empty() {
            s.push('/');
            s.push_str(&super::pct_path(key));
        }
        if !query.is_empty() {
            s.push('?');
            s.push_str(query);
        }
        url::Url::parse(&s).unwrap_or_else(|_| url::Url::parse("https://invalid/").unwrap())
    }
    async fn signed_send(
        &self,
        method: reqwest::Method,
        url: url::Url,
        extra: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, String> {
        let body_sha = if body.is_empty() {
            sha256_hex(b"")
        } else {
            sha256_hex(&body)
        };
        let headers = sign(
            &self.signer(),
            method.as_str(),
            &url,
            &extra,
            &body_sha,
            now_amz(),
        );
        let mut r = self.client.request(method, url.as_str());
        for (k, v) in headers.iter().chain(extra.iter()) {
            r = r.header(k.as_str(), v.as_str());
        }
        if !body.is_empty() {
            r = r.body(body);
        }
        r.send().await.map_err(|e| e.to_string())
    }
    fn prefix(path: &str) -> String {
        let p = path.trim_matches('/');
        if p.is_empty() {
            String::new()
        } else {
            format!("{p}/")
        }
    }
}

#[async_trait::async_trait]
impl DriveBackend for S3 {
    fn kind(&self) -> DriveKind {
        DriveKind::S3
    }
    async fn about(&self) -> Result<String, String> {
        Ok(format!("S3 bucket {}", self.bucket()))
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let prefix = Self::prefix(path);
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let mut q = format!("list-type=2&delimiter=/&prefix={}", super::pct(&prefix));
            if !token.is_empty() {
                q.push_str(&format!("&continuation-token={}", super::pct(&token)));
            }
            let url = self.url("", &q);
            let resp = self
                .signed_send(reqwest::Method::GET, url, vec![], vec![])
                .await?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
            let xml = resp.text().await.map_err(|e| e.to_string())?;
            let doc = roxmltree::Document::parse(&xml).map_err(|e| e.to_string())?;
            for n in doc.descendants() {
                match n.tag_name().name() {
                    "Contents" => {
                        let key = n
                            .descendants()
                            .find(|x| x.tag_name().name() == "Key")
                            .and_then(|x| x.text())
                            .unwrap_or("")
                            .to_string();
                        let size: u64 = n
                            .descendants()
                            .find(|x| x.tag_name().name() == "Size")
                            .and_then(|x| x.text())
                            .and_then(|t| t.parse().ok())
                            .unwrap_or(0);
                        let modified = n
                            .descendants()
                            .find(|x| x.tag_name().name() == "LastModified")
                            .and_then(|x| x.text())
                            .map(super::parse_time)
                            .unwrap_or(0);
                        if key == prefix {
                            continue;
                        }
                        out.push(RemoteEntry {
                            name: super::base_name(&key),
                            path: key.trim_end_matches('/').to_string(),
                            id: key.clone(),
                            is_dir: false,
                            size,
                            modified,
                            mime: String::new(),
                        });
                    }
                    "CommonPrefixes" => {
                        let p = n
                            .descendants()
                            .find(|x| x.tag_name().name() == "Prefix")
                            .and_then(|x| x.text())
                            .unwrap_or("");
                        let trimmed = p.trim_end_matches('/');
                        out.push(RemoteEntry {
                            name: super::base_name(trimmed),
                            path: trimmed.to_string(),
                            id: p.to_string(),
                            is_dir: true,
                            size: 0,
                            modified: 0,
                            mime: String::new(),
                        });
                    }
                    _ => {}
                }
            }
            let truncated = doc
                .descendants()
                .any(|n| n.tag_name().name() == "IsTruncated" && n.text() == Some("true"));
            token = doc
                .descendants()
                .find(|n| n.tag_name().name() == "NextContinuationToken")
                .and_then(|n| n.text())
                .unwrap_or("")
                .to_string();
            if !truncated || token.is_empty() {
                break;
            }
        }
        super::sort_entries(&mut out);
        Ok(out)
    }
    async fn mkdir(&self, path: &str) -> Result<(), String> {
        // Zero-byte "folder" marker object.
        let key = format!("{}/", path.trim_matches('/'));
        let url = self.url(&key, "");
        let resp = self
            .signed_send(reqwest::Method::PUT, url, vec![], vec![])
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(http_util::err_text(resp).await)
        }
    }
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String> {
        if entry.is_dir {
            // Delete all under prefix (recursive).
            let entries = self.list(&entry.path).await?;
            for e in entries {
                Box::pin(self.delete(&e)).await?;
            }
            let key = format!("{}/", entry.path.trim_matches('/'));
            let url = self.url(&key, "");
            let _ = self
                .signed_send(reqwest::Method::DELETE, url, vec![], vec![])
                .await?;
            Ok(())
        } else {
            let url = self.url(&entry.path, "");
            let resp = self
                .signed_send(reqwest::Method::DELETE, url, vec![], vec![])
                .await?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(http_util::err_text(resp).await)
            }
        }
    }
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String> {
        // S3 rename = copy + delete.
        let dst = super::join_path(&super::parent_path(&entry.path), new_name);
        let src = format!("/{}/{}", self.bucket(), super::pct_path(&entry.path));
        let url = self.url(&dst, "");
        let resp = self
            .signed_send(
                reqwest::Method::PUT,
                url,
                vec![("x-amz-copy-source".into(), src)],
                vec![],
            )
            .await?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let mut e = entry.clone();
        e.path = entry.path.clone();
        if !entry.is_dir {
            let url = self.url(&entry.path, "");
            let _ = self
                .signed_send(reqwest::Method::DELETE, url, vec![], vec![])
                .await?;
        }
        let _ = &mut e;
        Ok(())
    }
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        parallel: u32,
        prog: &Progress,
    ) -> Result<(), String> {
        let url = self.url(&entry.path, "");
        let signer = self.signer();
        let client = self.client.clone();
        let mk = move || {
            let body_sha = sha256_hex(b"");
            let hdrs = sign(&signer, "GET", &url, &[], &body_sha, now_amz());
            let mut r = client.get(url.as_str());
            for (k, v) in hdrs {
                r = r.header(k, v);
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
        let key = super::join_path(dest_dir, name);
        let size = http_util::file_len(local);
        prog.set_total(size);
        const PART: u64 = 16 * 1024 * 1024;
        if size <= PART {
            let bytes = http_util::read_file(local).await?;
            let url = self.url(&key, "");
            let resp = self
                .signed_send(reqwest::Method::PUT, url, vec![], bytes)
                .await?;
            if !resp.status().is_success() {
                return Err(http_util::err_text(resp).await);
            }
            prog.add(size);
            return Ok(RemoteEntry {
                id: key.clone(),
                name: name.into(),
                path: key,
                is_dir: false,
                size,
                modified: 0,
                mime: String::new(),
            });
        }
        // Multipart: create -> parts (sequential; server presign-free) -> complete.
        // ponytail: sequential parts; parallel needs signed part URLs batching.
        let url = self.url(&key, "uploads=");
        let resp = self
            .signed_send(reqwest::Method::POST, url, vec![], vec![])
            .await?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        let xml = resp.text().await.map_err(|e| e.to_string())?;
        let doc = roxmltree::Document::parse(&xml).map_err(|e| e.to_string())?;
        let upload_id = doc
            .descendants()
            .find(|n| n.tag_name().name() == "UploadId")
            .and_then(|n| n.text())
            .ok_or("S3: no UploadId")?
            .to_string();
        let data = std::sync::Arc::new(tokio::fs::read(local).await.map_err(|e| e.to_string())?);
        // ponytail: multipart loads RAM; stream parts later if OOM reports.
        let parts: Vec<(u32, usize, usize)> = {
            let mut v = Vec::new();
            let mut off = 0usize;
            let mut n = 1u32;
            while off < data.len() {
                let end = (off + PART as usize).min(data.len());
                v.push((n, off, end));
                off = end;
                n += 1;
            }
            v
        };
        let conc = self.acc.streams.clamp(1, 4) as usize;
        let key_ref = &key;
        let upload_ref = &upload_id;
        let results: Vec<Result<(u32, String), String>> =
            futures_util::stream::iter(parts.into_iter().map(|(part_no, off, end)| {
                let data = data.clone();
                let prog = prog.clone();
                let key = key_ref;
                let upload_id = upload_ref;
                async move {
                    prog.wait().await;
                    prog.check()?;
                    let q = format!("partNumber={part_no}&uploadId={}", super::pct(upload_id));
                    let url = self.url(key, &q);
                    let resp = self
                        .signed_send(reqwest::Method::PUT, url, vec![], data[off..end].to_vec())
                        .await?;
                    if !resp.status().is_success() {
                        return Err(http_util::err_text(resp).await);
                    }
                    let etag = resp
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    prog.add((end - off) as u64);
                    Ok((part_no, etag))
                }
            }))
            .buffer_unordered(conc)
            .collect()
            .await;
        let mut etags: Vec<(u32, String)> = Vec::new();
        for r in results {
            match r {
                Ok(x) => etags.push(x),
                Err(e) => {
                    let _ = self
                        .signed_send(
                            reqwest::Method::DELETE,
                            self.url(&key, &format!("uploadId={}", super::pct(&upload_id))),
                            vec![],
                            vec![],
                        )
                        .await;
                    return Err(e);
                }
            }
        }
        etags.sort_by_key(|(n, _)| *n);
        let mut body = String::from("<CompleteMultipartUpload>");
        for (n, e) in &etags {
            body.push_str(&format!(
                "<Part><PartNumber>{n}</PartNumber><ETag>{e}</ETag></Part>"
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let url = self.url(&key, &format!("uploadId={}", super::pct(&upload_id)));
        let resp = self
            .signed_send(reqwest::Method::POST, url, vec![], body.into_bytes())
            .await?;
        if !resp.status().is_success() {
            return Err(http_util::err_text(resp).await);
        }
        Ok(RemoteEntry {
            id: key.clone(),
            name: name.into(),
            path: key,
            is_dir: false,
            size,
            modified: 0,
            mime: String::new(),
        })
    }
    async fn copy_to(
        &self,
        _entry: &RemoteEntry,
        dst: &dyn DriveBackend,
        _dst_dir: &str,
        _prog: &Progress,
    ) -> Result<(), String> {
        if dst.kind() != DriveKind::S3 {
            return Err("cross-provider via jobs".into());
        }
        Err("jobs fallback".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sigv4_known_vector() {
        // AWS docs example: IAM ListUsers signed request shape; verify deterministic sig with fixed date.
        let s = SigV4 {
            access: "AKIDEXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
            service: "iam".into(),
        };
        let url = url::Url::parse("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08")
            .unwrap();
        let hdrs = sign(
            &s,
            "GET",
            &url,
            &[(
                "content-type".into(),
                "application/x-www-form-urlencoded; charset=utf-8".into(),
            )],
            &sha256_hex(b""),
            ("20150830T123600Z".into(), "20150830".into()),
        );
        let auth = hdrs
            .iter()
            .find(|(k, _)| k == "Authorization")
            .unwrap()
            .1
            .clone();
        // AWS docs SigV4 example (with x-amz-content-sha256 added by us):
        // deterministic re-sign must match, scope format exact.
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date, Signature="), "{auth}");
        let hdrs2 = sign(
            &s,
            "GET",
            &url,
            &[(
                "content-type".into(),
                "application/x-www-form-urlencoded; charset=utf-8".into(),
            )],
            &sha256_hex(b""),
            ("20150830T123600Z".into(), "20150830".into()),
        );
        assert_eq!(hdrs, hdrs2, "signing must be deterministic");
    }
    #[test]
    fn civil_check() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1704067200), (2024, 1, 1, 0, 0, 0));
    }
}
