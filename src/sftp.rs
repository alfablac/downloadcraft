// SFTP download via russh + russh-sftp. Password (task.password) or key file
// (task.ssh_key_path, else ~/.ssh/id_ed25519 then id_rsa). Host key TOFU:
// first connect stores SHA256 fingerprint in config_dir/known_hosts.json
// (<host>:<port> -> fp); mismatch hard-errors, never blind-accepts after.
// ponytail: key passphrase = task.password when key set; no ssh-agent. Add agent when requested.
use crate::engine::Controls;
use crate::model::{DownloadTask, TaskStatus};
use russh::client;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
pub use russh_sftp::client::SftpSession;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub struct SftpUrl {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub path: String,
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

pub fn parse_sftp_url(raw: &str, task_user: &str, task_pass: &str) -> Result<SftpUrl, String> {
    let url = url::Url::parse(raw).map_err(|e| e.to_string())?;
    if url.scheme() != "sftp" && url.scheme() != "ssh" {
        return Err(format!("unsupported scheme: {}", url.scheme()));
    }
    let host = url.host_str().ok_or("SFTP URL missing host")?.to_string();
    let port = url.port().unwrap_or(22);
    let path = pct_decode(url.path());
    if path.is_empty() || path == "/" {
        return Err("SFTP URL missing file path".into());
    }
    let user = if !task_user.is_empty() {
        task_user.to_string()
    } else if !url.username().is_empty() {
        pct_decode(url.username())
    } else {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "anonymous".into())
    };
    let pass = if !task_pass.is_empty() {
        task_pass.to_string()
    } else {
        url.password().map(pct_decode).unwrap_or_default()
    };
    Ok(SftpUrl {
        host,
        port,
        user,
        pass,
        path,
    })
}

fn known_hosts_path() -> PathBuf {
    crate::platform::config_dir().join("known_hosts.json")
}

fn load_known_hosts_at(path: &std::path::Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Ok(true)=known match, Ok(false)=first-seen stored, Err=changed/corrupt-write.
fn verify_or_store_at(path: &std::path::Path, hostport: &str, fp: &str) -> Result<bool, String> {
    let mut map = load_known_hosts_at(path);
    match map.get(hostport) {
        Some(old) if old == fp => Ok(true),
        Some(old) => Err(format!(
            "host key changed for {hostport}: stored {old}, got {fp}"
        )),
        None => {
            map.insert(hostport.to_string(), fp.to_string());
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::write(
                path,
                serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            Ok(false)
        }
    }
}

fn b64_nopad(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

/// OpenSSH SHA256 fingerprint: `SHA256:<base64-nopad>`.
fn fingerprint_of(key: &russh::keys::PublicKeyOrCertificate) -> String {
    let fp = key.public_key().fingerprint(russh::keys::HashAlg::Sha256);
    format!("SHA256:{}", b64_nopad(fp.as_bytes()))
}

struct Handler {
    hostport: String,
    err: Arc<std::sync::Mutex<Option<String>>>,
}
impl client::Handler for Handler {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = fingerprint_of(key);
        match verify_or_store_at(&known_hosts_path(), &self.hostport, &fp) {
            Ok(_) => Ok(true),
            Err(e) => {
                *self.err.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                Ok(false)
            }
        }
    }
}

async fn wait_not_paused(ctrl: &Controls) -> bool {
    while ctrl.paused.load(Ordering::Relaxed) {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    ctrl.cancelled.load(Ordering::Relaxed)
}

fn default_key_path() -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    ["id_ed25519", "id_rsa"]
        .iter()
        .map(|n| home.join(".ssh").join(n))
        .find(|p| p.exists())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SftpAuth {
    /// Explicit key (task.ssh_key_path) or probed default key; password = passphrase.
    Key,
    Password,
    KeyboardInteractive,
    None,
}

/// Auth order: explicit key wins; password-only otherwise. On key failure the
/// caller falls back to password, then keyboard-interactive (same secret).
fn pick_auth(has_key: bool, has_password: bool) -> Vec<SftpAuth> {
    let mut v = Vec::new();
    if has_key {
        v.push(SftpAuth::Key);
    }
    if has_password {
        v.push(SftpAuth::Password);
        v.push(SftpAuth::KeyboardInteractive);
    }
    if v.is_empty() {
        v.push(SftpAuth::None);
    }
    v
}

async fn auth_keyboard_interactive(
    session: &mut client::Handle<Handler>,
    user: &str,
    pass: &str,
) -> Result<bool, String> {
    use russh::client::KeyboardInteractiveAuthResponse as R;
    match session
        .authenticate_keyboard_interactive_start(user.to_string(), None::<String>)
        .await
        .map_err(|e| e.to_string())?
    {
        R::Success => Ok(true),
        R::Failure { .. } => Ok(false),
        R::InfoRequest { prompts, .. } => {
            let answers: Vec<String> = prompts
                .iter()
                .map(|_| {
                    if pass.is_empty() {
                        String::new()
                    } else {
                        pass.to_string()
                    }
                })
                .collect();
            match session
                .authenticate_keyboard_interactive_respond(answers)
                .await
                .map_err(|e| e.to_string())?
            {
                R::Success => Ok(true),
                _ => Ok(false),
            }
        }
    }
}

/// Drive backend seam: authenticated SFTP session (same TOFU + auth order).
pub async fn connect_session(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    key_path: &str,
) -> Result<SftpSession, String> {
    let hostport = format!("{host}:{port}");
    let err: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let handler = Handler {
        hostport: hostport.clone(),
        err: err.clone(),
    };
    let config = Arc::new(client::Config::default());
    let mut session = match client::connect(config, (host, port), handler).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(m) = err.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Err(m);
            }
            return Err(format!("SSH connect {hostport}: {e}"));
        }
    };
    if let Some(m) = err.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(m);
    }
    let kp: Option<PathBuf> = if !key_path.is_empty() {
        Some(PathBuf::from(key_path))
    } else if pass.is_empty() {
        default_key_path()
    } else {
        None
    };
    let mut authed = false;
    for method in pick_auth(kp.is_some(), !pass.is_empty()) {
        match method {
            SftpAuth::Key => {
                let k = kp.as_ref().unwrap();
                let passphrase = if pass.is_empty() { None } else { Some(pass) };
                if let Ok(key) = load_secret_key(k, passphrase) {
                    let hash = session
                        .best_supported_rsa_hash()
                        .await
                        .map_err(|e| e.to_string())?
                        .flatten();
                    if session
                        .authenticate_publickey(
                            user.to_string(),
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                        )
                        .await
                        .map_err(|e| e.to_string())?
                        .success()
                    {
                        authed = true;
                        break;
                    }
                }
            }
            SftpAuth::Password => {
                if session
                    .authenticate_password(user.to_string(), pass.to_string())
                    .await
                    .map_err(|e| e.to_string())?
                    .success()
                {
                    authed = true;
                    break;
                }
            }
            SftpAuth::KeyboardInteractive => {
                if auth_keyboard_interactive(&mut session, user, pass).await? {
                    authed = true;
                    break;
                }
            }
            SftpAuth::None => return Err("SFTP: no password and no SSH key".into()),
        }
    }
    if !authed {
        return Err(format!("SFTP auth failed for {user}@{hostport}"));
    }
    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| e.to_string())?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| e.to_string())?;
    sftp.set_timeout(3600);
    Ok(sftp)
}

/// SFTP download: metadata size, seek-resume from `<filename>.part`, rename on complete.
pub async fn download_sftp(
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
) -> Result<DownloadTask, String> {
    let u = parse_sftp_url(&task.url, &task.username, &task.password)?;
    if task.filename.is_empty() {
        task.filename = crate::model::filename_from_url(&task.url);
    }
    tokio::fs::create_dir_all(&task.dest_dir)
        .await
        .map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part = task.dest_dir.join(format!("{}.part", task.filename));
    let mut offset = tokio::fs::metadata(&part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let hostport = format!("{}:{}", u.host, u.port);
    let err: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let handler = Handler {
        hostport: hostport.clone(),
        err: err.clone(),
    };
    let config = Arc::new(client::Config::default());
    let mut session = match client::connect(config, (u.host.as_str(), u.port), handler).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(m) = err.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Err(m);
            }
            return Err(format!("SSH connect {hostport}: {e}"));
        }
    };
    if let Some(m) = err.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(m);
    }

    // Auth order from pick_auth: explicit/default key first, then password,
    // then keyboard-interactive. Key load/reject falls through instead of
    // hard-failing (wrong passphrase would otherwise never try password).
    let key_path: Option<PathBuf> = if !task.ssh_key_path.is_empty() {
        Some(PathBuf::from(&task.ssh_key_path))
    } else if u.pass.is_empty() {
        default_key_path()
    } else {
        None
    };
    let mut authed = false;
    let mut tried: Vec<&str> = Vec::new();
    for method in pick_auth(key_path.is_some(), !u.pass.is_empty()) {
        match method {
            SftpAuth::Key => {
                let kp = key_path.as_ref().unwrap();
                tried.push("publickey");
                let passphrase = if u.pass.is_empty() {
                    None
                } else {
                    Some(u.pass.as_str())
                };
                let loaded = load_secret_key(kp, passphrase);
                if let Ok(key) = loaded {
                    let hash = session
                        .best_supported_rsa_hash()
                        .await
                        .map_err(|e| e.to_string())?
                        .flatten();
                    if session
                        .authenticate_publickey(
                            u.user.clone(),
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                        )
                        .await
                        .map_err(|e| e.to_string())?
                        .success()
                    {
                        authed = true;
                        break;
                    }
                }
                // fall through to password methods
            }
            SftpAuth::Password => {
                tried.push("password");
                if session
                    .authenticate_password(u.user.clone(), u.pass.clone())
                    .await
                    .map_err(|e| e.to_string())?
                    .success()
                {
                    authed = true;
                    break;
                }
            }
            SftpAuth::KeyboardInteractive => {
                tried.push("keyboard-interactive");
                if auth_keyboard_interactive(&mut session, &u.user, &u.pass).await? {
                    authed = true;
                    break;
                }
            }
            SftpAuth::None => {
                return Err(
                    "SFTP: no password and no SSH key (set task password or ssh_key_path)".into(),
                );
            }
        }
    }
    if !authed {
        return Err(format!(
            "SFTP auth failed for {}@{hostport} (tried: {})",
            u.user,
            tried.join(", ")
        ));
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| e.to_string())?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| e.to_string())?;
    // russh-sftp default per-request timeout 10s kills idle/pause waits; raise ceiling.
    sftp.set_timeout(3600);

    let total: u64 = sftp
        .metadata(&u.path)
        .await
        .map_err(|e| format!("SFTP stat {}: {e}", u.path))?
        .size
        .ok_or("SFTP server gave no file size")?;
    task.total = Some(total);
    ctrl.total.store(total, Ordering::Relaxed);
    ctrl.parts_are_segments.store(false, Ordering::Relaxed);
    if total > 0 && offset == total {
        if dest.exists() {
            tokio::fs::remove_file(&dest)
                .await
                .map_err(|e| e.to_string())?;
        }
        tokio::fs::rename(&part, &dest)
            .await
            .map_err(|e| e.to_string())?;
        ctrl.downloaded.store(total, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(total, total)];
        task.downloaded = total;
        task.status = TaskStatus::Complete;
        return Ok(task);
    }
    if offset > total {
        let _ = tokio::fs::remove_file(&part).await;
        offset = 0;
    }
    ctrl.downloaded.store(offset, Ordering::Relaxed);
    *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(offset, total)];
    task.downloaded = offset;
    task.status = TaskStatus::Downloading;

    let mut remote = sftp.open(&u.path).await.map_err(|e| e.to_string())?;
    if offset > 0 {
        remote
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(offset > 0)
        .write(true)
        .truncate(offset == 0)
        .open(&part)
        .await
        .map_err(|e| e.to_string())?;
    let mut downloaded = offset;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if wait_not_paused(&ctrl).await {
            file.flush().await.map_err(|e| e.to_string())?;
            task.status = TaskStatus::Paused;
            task.downloaded = downloaded;
            return Ok(task);
        }
        if downloaded >= total {
            break;
        }
        let want = ((total - downloaded).min(buf.len() as u64)) as usize;
        // Wake every 200ms so cancel lands while server stalls.
        let n = loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                remote.read(&mut buf[..want]),
            )
            .await
            {
                Ok(r) => break r.map_err(|e| e.to_string())?,
                Err(_) if ctrl.cancelled.load(Ordering::Relaxed) => {
                    file.flush().await.map_err(|e| e.to_string())?;
                    task.status = TaskStatus::Paused;
                    task.downloaded = downloaded;
                    return Ok(task);
                }
                Err(_) => {
                    if wait_not_paused(&ctrl).await {
                        file.flush().await.map_err(|e| e.to_string())?;
                        task.status = TaskStatus::Paused;
                        task.downloaded = downloaded;
                        return Ok(task);
                    }
                }
            }
        };
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        downloaded += n as u64;
        ctrl.downloaded.store(downloaded, Ordering::Relaxed);
        if let Some(p) = ctrl
            .parts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_mut()
        {
            p.0 = downloaded;
        }
        task.downloaded = downloaded;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    if downloaded != total {
        return Err(format!(
            "SFTP size mismatch: got {downloaded} expected {total}"
        ));
    }
    if dest.exists() {
        tokio::fs::remove_file(&dest)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::rename(&part, &dest)
        .await
        .map_err(|e| e.to_string())?;
    task.downloaded = downloaded;
    task.status = TaskStatus::Complete;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full() {
        let u =
            parse_sftp_url("sftp://alice:secret@example.com:2222/dir/file.bin", "", "").unwrap();
        assert_eq!(
            (
                u.host.as_str(),
                u.port,
                u.user.as_str(),
                u.pass.as_str(),
                u.path.as_str()
            ),
            ("example.com", 2222, "alice", "secret", "/dir/file.bin")
        );
    }
    #[test]
    fn parse_defaults_and_ssh_alias() {
        let u = parse_sftp_url("ssh://bob@example.com/f.bin", "", "").unwrap();
        assert_eq!(
            (u.port, u.user.as_str(), u.path.as_str()),
            (22, "bob", "/f.bin")
        );
        let u2 = parse_sftp_url("sftp://example.com/f.bin", "", "").unwrap();
        assert_eq!(u2.port, 22);
        assert!(!u2.user.is_empty());
    }
    #[test]
    fn parse_task_creds_win_and_pct_decode() {
        let u =
            parse_sftp_url("sftp://alice:p%40ss@example.com/dir%20f.bin", "carol", "pw").unwrap();
        assert_eq!(
            (u.user.as_str(), u.pass.as_str(), u.path.as_str()),
            ("carol", "pw", "/dir f.bin")
        );
    }
    #[test]
    fn parse_rejects() {
        assert!(parse_sftp_url("https://example.com/f", "", "").is_err());
        assert!(parse_sftp_url("sftp://example.com/", "", "").is_err());
        assert!(parse_sftp_url("sftp:///f.bin", "", "").is_err());
    }
    #[test]
    fn tofu_store_match_mismatch() {
        let dir = std::env::temp_dir().join(format!("downloadcraft-kh-{}", std::process::id()));
        let path = dir.join("known_hosts.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(verify_or_store_at(&path, "h:22", "SHA256:AAA"), Ok(false));
        assert_eq!(verify_or_store_at(&path, "h:22", "SHA256:AAA"), Ok(true));
        let e = verify_or_store_at(&path, "h:22", "SHA256:BBB").unwrap_err();
        assert!(e.contains("host key changed"), "{e}");
        // Stored value untouched by mismatch.
        assert_eq!(
            load_known_hosts_at(&path).get("h:22").map(|s| s.as_str()),
            Some("SHA256:AAA")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn pick_auth_order() {
        assert_eq!(
            pick_auth(true, true),
            vec![
                SftpAuth::Key,
                SftpAuth::Password,
                SftpAuth::KeyboardInteractive
            ]
        );
        assert_eq!(
            pick_auth(false, true),
            vec![SftpAuth::Password, SftpAuth::KeyboardInteractive]
        );
        assert_eq!(pick_auth(true, false), vec![SftpAuth::Key]);
        assert_eq!(pick_auth(false, false), vec![SftpAuth::None]);
    }
    #[test]
    fn b64_nopad_vectors() {
        assert_eq!(b64_nopad(b""), "");
        assert_eq!(b64_nopad(b"f"), "Zg");
        assert_eq!(b64_nopad(b"fo"), "Zm8");
        assert_eq!(b64_nopad(b"foo"), "Zm9v");
        assert_eq!(b64_nopad(&[0xfb, 0xff]), "+/8");
    }
}
