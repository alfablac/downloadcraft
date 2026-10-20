//! Import rclone.conf sections -> DriveAccount (pure, unit tested).
//! Supports drive/onedrive/dropbox/webdav/s3/sftp/ftp/local + crypt
//! modifier applied to referenced account. Passwords: rclone obscure
//! reversible (AES-256-CTR static key, base64url raw, 16B IV prefix).
use crate::model::{DriveAccount, DriveKind};
use aes::cipher::{KeyIvInit, StreamCipher};
use aes::Aes256;
use ctr::Ctr64BE;

type Aes256Ctr = Ctr64BE<Aes256>;

// rclone lib/obscure default key (public, shipped in rclone source).
const OBSCURE_KEY: [u8; 32] = [
    0x9c, 0x93, 0x5b, 0x48, 0x73, 0x0a, 0x55, 0x4d, 0x6b, 0xfd, 0x7c, 0x63, 0xc8, 0x86, 0xa9, 0x2b,
    0xd3, 0x90, 0x19, 0x8e, 0xb8, 0x12, 0x8a, 0xfb, 0xf4, 0xde, 0x16, 0x2b, 0x8b, 0x95, 0xf6, 0x38,
];

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut vals = Vec::new();
    for c in s.trim().chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' => 62,
            '_' => 63,
            '=' => break,
            _ => return Err(format!("obscure: bad char {c}")),
        };
        vals.push(v);
    }
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        let mut n: u32 = 0;
        for (i, v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Reveal rclone-obscured password. Plain text passes through unchanged.
pub fn reveal_obscure(s: &str) -> Result<String, String> {
    let raw = b64url_decode(s)?;
    if raw.len() <= 16 {
        return Ok(s.to_string());
    }
    let (iv, ct) = raw.split_at(16);
    let mut buf = ct.to_vec();
    let mut cipher = Aes256Ctr::new(&OBSCURE_KEY.into(), iv.into());
    cipher.apply_keystream(&mut buf);
    String::from_utf8(buf).map_err(|_| "obscure reveal not utf8".to_string())
}

/// Obscure (for tests; mirrors rclone obscure with given IV).
pub fn obscure_with_iv(plain: &str, iv: &[u8; 16]) -> String {
    let mut buf = plain.as_bytes().to_vec();
    let mut cipher = Aes256Ctr::new(&OBSCURE_KEY.into(), iv.into());
    cipher.apply_keystream(&mut buf);
    let mut raw = iv.to_vec();
    raw.extend(buf);
    b64url_encode(&raw)
}

fn b64url_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
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

struct Section {
    name: String,
    kv: Vec<(String, String)>,
}
impl Section {
    fn get(&self, k: &str) -> &str {
        self.kv
            .iter()
            .find(|(kk, _)| kk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

fn parse_ini(text: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') {
            out.push(Section {
                name: l[1..l.len() - 1].trim().to_string(),
                kv: Vec::new(),
            });
            continue;
        }
        if let Some(cur) = out.last_mut() {
            if let Some((k, v)) = l.split_once('=') {
                cur.kv
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

fn token_fields(acc: &mut DriveAccount, sec: &Section) {
    let t = sec.get("token");
    if t.is_empty() {
        return;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        acc.access_token = v["access_token"].as_str().unwrap_or("").into();
        acc.refresh_token = v["refresh_token"].as_str().unwrap_or("").into();
        if let Some(exp) = v["expiry"].as_str() {
            acc.token_expires = super::parse_time(exp);
        }
    }
}

fn pw(sec: &Section, key: &str, warns: &mut Vec<String>, secname: &str) -> String {
    let v = sec.get(key);
    if v.is_empty() {
        return String::new();
    }
    match reveal_obscure(v) {
        Ok(x) => x,
        Err(e) => {
            warns.push(format!("{secname}: {key} {e} (kept raw)"));
            v.to_string()
        }
    }
}

/// Parse pasted rclone config -> (accounts, warnings).
pub fn parse_rclone_config(text: &str) -> (Vec<DriveAccount>, Vec<String>) {
    let mut warns: Vec<String> = Vec::new();
    let sections = parse_ini(text);
    let mut out: Vec<DriveAccount> = Vec::new();
    let mut crypts: Vec<(String, String, String)> = Vec::new(); // (remote name, pw, salt)
    for sec in &sections {
        let ty = sec.get("type").to_ascii_lowercase();
        let mut acc = DriveAccount {
            id: crate::model::new_id(),
            name: sec.name.clone(),
            streams: 4,
            ..Default::default()
        };
        match ty.as_str() {
            "drive" => {
                acc.kind = DriveKind::GoogleDrive;
                acc.client_id = sec.get("client_id").into();
                acc.client_secret = sec.get("client_secret").into();
                acc.team_drive = sec.get("team_drive").into();
                acc.root = sec.get("root_folder_id").into();
                token_fields(&mut acc, sec);
                if !sec.get("service_account_file").is_empty() {
                    warns.push(format!("{}: service account unsupported (ignored)", sec.name));
                }
            }
            "onedrive" => {
                acc.kind = DriveKind::OneDrive;
                acc.client_id = sec.get("client_id").into();
                acc.client_secret = sec.get("client_secret").into();
                token_fields(&mut acc, sec);
                if !sec.get("drive_id").is_empty() {
                    warns.push(format!("{}: OneDrive drive_id not honored (default drive used)", sec.name));
                }
            }
            "dropbox" => {
                acc.kind = DriveKind::Dropbox;
                acc.client_id = if sec.get("client_id").is_empty() {
                    sec.get("app_key").into()
                } else {
                    sec.get("client_id").into()
                };
                acc.client_secret = if sec.get("client_secret").is_empty() {
                    sec.get("app_secret").into()
                } else {
                    sec.get("client_secret").into()
                };
                token_fields(&mut acc, sec);
            }
            "webdav" => {
                acc.kind = DriveKind::WebDav;
                acc.endpoint = sec.get("url").into();
                acc.username = sec.get("user").into();
                acc.secret = pw(sec, "pass", &mut warns, &sec.name);
                if !sec.get("bearer_token").is_empty() {
                    warns.push(format!("{}: bearer_token unsupported", sec.name));
                }
            }
            "s3" => {
                acc.kind = DriveKind::S3;
                acc.username = sec.get("access_key_id").into();
                acc.secret = sec.get("secret_access_key").into();
                acc.endpoint = sec.get("endpoint").into();
                acc.region = {
                    let r = sec.get("region");
                    if r.is_empty() { "auto".into() } else { r.into() }
                };
                if acc.endpoint.is_empty() && !sec.get("provider").is_empty() && sec.get("provider") != "AWS" {
                    warns.push(format!("{}: set endpoint for provider {}", sec.name, sec.get("provider")));
                }
                warns.push(format!("{}: set Bucket (Root) after import", sec.name));
            }
            "sftp" => {
                acc.kind = DriveKind::Sftp;
                acc.endpoint = sec.get("host").into();
                acc.username = sec.get("user").into();
                acc.port = sec.get("port").parse().unwrap_or(0);
                acc.secret = pw(sec, "pass", &mut warns, &sec.name);
                acc.ssh_key_path = sec.get("key_file").into();
                if !sec.get("key_pem").is_empty() {
                    warns.push(format!("{}: inline key_pem unsupported, use key_file", sec.name));
                }
            }
            "ftp" => {
                acc.kind = DriveKind::Ftp;
                let host = sec.get("host").trim_start_matches("ftp://").trim_start_matches("ftps://");
                acc.username = sec.get("user").into();
                acc.port = sec.get("port").parse().unwrap_or(0);
                acc.secret = pw(sec, "pass", &mut warns, &sec.name);
                if sec.get("explicit_tls") == "true" {
                    acc.endpoint = format!("ftps://{host}");
                } else {
                    acc.endpoint = host.into();
                    if sec.get("tls") == "true" && acc.port == 0 {
                        acc.port = 990;
                    }
                }
            }
            "local" => {
                acc.kind = DriveKind::Local;
                warns.push(format!("{}: set Folder after import", sec.name));
            }
            "teldrive" => {
                acc.kind = DriveKind::Teldrive;
                acc.endpoint = if sec.get("api_host").is_empty() {
                    sec.get("url").into()
                } else {
                    sec.get("api_host").into()
                };
                let k = sec.get("api_key");
                acc.secret = if k.is_empty() {
                    sec.get("token").into()
                } else {
                    format!("api_key:{k}")
                };
            }
            "crypt" => {
                let remote = sec.get("remote").trim_end_matches(':').to_string();
                let p = pw(sec, "password", &mut warns, &sec.name);
                let s = pw(sec, "password2", &mut warns, &sec.name);
                crypts.push((remote, p, s));
                continue;
            }
            "" => {
                warns.push(format!("{}: no type=, skipped", sec.name));
                continue;
            }
            other => {
                warns.push(format!("{}: type={other} unsupported, skipped", sec.name));
                continue;
            }
        }
        out.push(acc);
    }
    for (remote, p, s) in crypts {
        if let Some(acc) = out.iter_mut().find(|a| a.name == remote) {
            acc.crypt_password = p;
            acc.crypt_salt = s;
        } else {
            warns.push(format!("crypt -> {remote}: remote not in import, skipped"));
        }
    }
    (out, warns)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn obscure_roundtrip() {
        let iv = [7u8; 16];
        let enc = obscure_with_iv("s3cret-päss", &iv);
        assert_eq!(reveal_obscure(&enc).unwrap(), "s3cret-päss");
    }
    #[test]
    fn parse_full() {
        let txt = r#"
[g]
type = drive
client_id = cid
client_secret = cs
team_drive = 0ATeam
root_folder_id = 0AFold
token = {"access_token":"a","refresh_token":"r","expiry":"2024-01-01T00:00:00Z"}

[w]
type = webdav
url = https://h/dav/
user = u
pass = plainpass-not-obscured-but-short

[s]
type = s3
provider = Cloudflare
access_key_id = AK
secret_access_key = SK
endpoint = https://acct.r2.cloudflarestorage.com
region = auto

[x]
type = mega

[gcrypt]
type = crypt
remote = g:
password = plainpass-not-obscured-but-short
"#;
        let (accs, warns) = parse_rclone_config(txt);
        assert_eq!(accs.len(), 3);
        let g = &accs[0];
        assert_eq!(g.kind, DriveKind::GoogleDrive);
        assert_eq!(g.team_drive, "0ATeam");
        assert_eq!(g.root, "0AFold");
        assert_eq!(g.access_token, "a");
        assert_eq!(g.token_expires, 1704067200);
        assert_eq!(g.crypt_password, "plainpass-not-obscured-but-short");
        assert!(warns.iter().any(|w| w.contains("mega")));
        assert!(warns.iter().any(|w| w.contains("Bucket")));
    }
}
