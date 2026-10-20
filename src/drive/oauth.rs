//! OAuth2 authorization-code + PKCE flow for Google / Microsoft / Dropbox.
//! Fixed loopback redirect http://127.0.0.1:53682/ (rclone-style) so user
//! can register it once in every console. Auto capture + manual paste
//! fallback (paste redirect URL or code). Own client_id/secret only.
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const REDIRECT_PORT: u16 = 53682;
pub const REDIRECT_URI: &str = "http://127.0.0.1:53682/";

pub struct OAuthApp {
    pub authorize_url: String,
    pub token_url: String,
    pub scopes: String,
    pub client_id: String,
    pub client_secret: String,
    pub use_pkce: bool,
    pub extra_auth_params: Vec<(String, String)>,
}

impl OAuthApp {
    fn dup(&self) -> Self {
        Self {
            authorize_url: self.authorize_url.clone(),
            token_url: self.token_url.clone(),
            scopes: self.scopes.clone(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            use_pkce: self.use_pkce,
            extra_auth_params: self.extra_auth_params.clone(),
        }
    }
}

pub struct TokenOut {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// One in-flight sign-in. Never persisted (verifier single-use secret).
pub struct PendingAuth {
    pub app: OAuthApp,
    pub verifier: String,
    pub state: String,
    pub redirect: String,
    pub auth_url: String,
}

fn b64url(data: &[u8]) -> String {
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

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    h.finalize().into()
}

fn rand_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    if getrandom::fill(&mut v).is_err() {
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64) << 32;
        for b in v.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed >> 32) as u8;
        }
    }
    v
}

pub fn preset(kind: crate::model::DriveKind, acc: &crate::model::DriveAccount) -> Option<OAuthApp> {
    use crate::model::DriveKind::*;
    match kind {
        GoogleDrive => Some(OAuthApp {
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            scopes: "https://www.googleapis.com/auth/drive".into(),
            client_id: acc.client_id.clone(),
            client_secret: acc.client_secret.clone(),
            use_pkce: true,
            extra_auth_params: vec![
                ("access_type".into(), "offline".into()),
                ("prompt".into(), "consent".into()),
            ],
        }),
        OneDrive => Some(OAuthApp {
            authorize_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            scopes: "openid profile offline_access Files.ReadWrite.All".into(),
            client_id: acc.client_id.clone(),
            client_secret: acc.client_secret.clone(),
            use_pkce: true,
            extra_auth_params: vec![],
        }),
        Dropbox => Some(OAuthApp {
            authorize_url: "https://www.dropbox.com/oauth2/authorize".into(),
            token_url: "https://api.dropboxapi.com/oauth2/token".into(),
            scopes: "files.metadata.read files.metadata.write files.content.read files.content.write account_info.read".into(),
            client_id: acc.client_id.clone(),
            client_secret: acc.client_secret.clone(),
            use_pkce: true,
            extra_auth_params: vec![("token_access_type".into(), "offline".into())],
        }),
        _ => None,
    }
}

/// Build sign-in URL + PKCE pair (pure besides entropy).
pub fn start(app: &OAuthApp) -> Result<PendingAuth, String> {
    if app.client_id.trim().is_empty() {
        return Err("client_id empty: create your own OAuth app (Setup steps) and paste Client ID".into());
    }
    let verifier = b64url(&rand_bytes(48));
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = b64url(&rand_bytes(16));
    let redirect = REDIRECT_URI.to_string();
    let auth_url = authorize_url(app, &redirect, &challenge, &state);
    Ok(PendingAuth {
        app: app.dup(),
        verifier,
        state,
        redirect,
        auth_url,
    })
}

pub fn authorize_url(app: &OAuthApp, redirect: &str, challenge: &str, state: &str) -> String {
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        app.authorize_url,
        super::pct(&app.client_id),
        super::pct(redirect),
        super::pct(&app.scopes),
        super::pct(state),
    );
    if app.use_pkce {
        url.push_str(&format!(
            "&code_challenge={}&code_challenge_method=S256",
            super::pct(challenge)
        ));
    }
    for (k, v) in &app.extra_auth_params {
        url.push_str(&format!("&{}={}", super::pct(k), super::pct(v)));
    }
    url
}

/// Extract code from pasted bare code or full redirect URL; surfaces error=.
pub fn extract_code(pasted: &str) -> Result<String, String> {
    let s = pasted.trim();
    if s.is_empty() {
        return Err("paste code first".into());
    }
    if !s.contains("code=") && !s.contains("://") {
        return Ok(s.to_string());
    }
    let q = s.split(['?', '#']).nth(1).unwrap_or(s);
    let mut code = String::new();
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = url_decode(v),
            "error" => {
                return Err(format!(
                    "sign-in denied: {} {}",
                    url_decode(v),
                    url_decode(
                        q.split('&')
                            .find_map(|p| p.strip_prefix("error_description="))
                            .unwrap_or("")
                    )
                ))
            }
            _ => {}
        }
    }
    if code.is_empty() {
        Err("no code= found in pasted text — paste full address-bar URL or code".into())
    } else {
        Ok(code)
    }
}

/// Blocking exchange for manual paste path (reuses exchange_code).
pub fn finish(pending: &PendingAuth, pasted: &str) -> Result<TokenOut, String> {
    let code = extract_code(pasted)?;
    exchange_code(&pending.app, &code, &pending.redirect, &pending.verifier)
}

/// Wait on fixed-port loopback for `pending` redirect (blocking thread).
/// Opens browser when `open_browser`. Timeout 5 min. `cancel` settable by UI.
pub fn wait_loopback(
    pending: &PendingAuth,
    cancel: Arc<AtomicBool>,
    open_browser: bool,
) -> Result<TokenOut, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).map_err(|e| {
        format!("port {REDIRECT_PORT} busy ({e}) — close rclone/other webcraft, or paste code below")
    })?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    if open_browser {
        crate::platform::open_url(&pending.auth_url);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while std::time::Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        match listener.accept() {
            Ok((mut sock, _)) => {
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let first = req.lines().next().unwrap_or("").to_string();
                let q = first
                    .split('?')
                    .nth(1)
                    .map(|s| s.split(' ').next().unwrap_or(s).to_string())
                    .unwrap_or_default();
                let mut code = String::new();
                let mut got_state: Option<String> = None;
                let mut err: Option<String> = None;
                for pair in q.split('&') {
                    if pair.is_empty() {
                        continue;
                    }
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    match k {
                        "code" => code = url_decode(v),
                        "state" => got_state = Some(url_decode(v)),
                        "error" => {
                            err = Some(format!(
                                "{} {}",
                                url_decode(v),
                                url_decode(
                                    q.split('&')
                                        .find_map(|p| p.strip_prefix("error_description="))
                                        .unwrap_or("")
                                )
                            ))
                        }
                        _ => {}
                    }
                }
                if code.is_empty() && err.is_none() {
                    let _ = sock.write_all(b"HTTP/1.1 204 No Content\r\n\r\n");
                    continue;
                }
                if let Some(e) = err {
                    let body = "<html><body><h2>webcraft: sign-in failed. Back to app.</h2></body></html>";
                    let _ = sock.write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
                            .as_bytes(),
                    );
                    return Err(format!("OAuth error: {e}"));
                }
                if got_state.as_deref() != Some(pending.state.as_str()) {
                    return Err("OAuth state mismatch (restart sign-in)".into());
                }
                let body = "<html><body><h2>webcraft: sign-in complete. Close this tab.</h2></body></html>";
                let _ = sock.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
                        .as_bytes(),
                );
                return exchange_code(&pending.app, &code, &pending.redirect, &pending.verifier);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("OAuth timed out (no code). Use paste-code fallback.".into())
}

/// Back-compat wrapper: start + wait.
pub fn authorize(app: &OAuthApp, cancel: Arc<AtomicBool>) -> Result<TokenOut, String> {
    let pending = start(app)?;
    wait_loopback(&pending, cancel, true)
}

/// Blocking token exchange (call from std thread).
pub fn exchange_code(
    app: &OAuthApp,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> Result<TokenOut, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), redirect.into()),
        ("client_id".into(), app.client_id.clone()),
    ];
    if !app.client_secret.is_empty() {
        form.push(("client_secret".into(), app.client_secret.clone()));
    }
    if app.use_pkce {
        form.push(("code_verifier".into(), verifier.into()));
    }
    let resp = client
        .post(&app.token_url)
        .form(&form)
        .send()
        .map_err(|e| e.to_string())?;
    parse_token_resp(resp)
}

/// Async refresh. Rotates refresh token when provider returns new one.
pub async fn refresh(app: &OAuthApp, refresh_token: &str) -> Result<TokenOut, String> {
    let client = reqwest::Client::new();
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh_token.into()),
        ("client_id".into(), app.client_id.clone()),
    ];
    if !app.client_secret.is_empty() {
        form.push(("client_secret".into(), app.client_secret.clone()));
    }
    let resp = client
        .post(&app.token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let st = resp.status();
    let txt = resp.text().await.map_err(|e| e.to_string())?;
    if !st.is_success() {
        if txt.contains("invalid_grant") {
            return Err("sign-in expired/revoked: Connect again in Drives > Accounts".into());
        }
        return Err(format!(
            "token refresh HTTP {st}: {}",
            &txt[..txt.len().min(300)]
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&txt).map_err(|e| e.to_string())?;
    Ok(TokenOut {
        access_token: v["access_token"].as_str().unwrap_or("").into(),
        refresh_token: v["refresh_token"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
    })
}

fn parse_token_resp(resp: reqwest::blocking::Response) -> Result<TokenOut, String> {
    let st = resp.status();
    let txt = resp.text().map_err(|e| e.to_string())?;
    if !st.is_success() {
        return Err(format!(
            "token HTTP {st}: {}",
            &txt[..txt.len().min(300)]
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&txt).map_err(|e| e.to_string())?;
    let at = v["access_token"].as_str().unwrap_or("");
    if at.is_empty() {
        return Err("token response missing access_token".into());
    }
    Ok(TokenOut {
        access_token: at.into(),
        refresh_token: v["refresh_token"].as_str().unwrap_or("").into(),
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
    })
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn expires_at(expires_in: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now + expires_in.saturating_sub(60)
}

/// Ensure access token fresh; refresh when expiring within 120s.
pub async fn ensure_fresh(acc: &mut crate::model::DriveAccount) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if acc.access_token.is_empty() && acc.refresh_token.is_empty() {
        return Err("not signed in: Drives > Accounts > Connect".into());
    }
    if !acc.refresh_token.is_empty()
        && (acc.access_token.is_empty() || acc.token_expires <= now + 120)
    {
        let app = preset(acc.kind, acc).ok_or("no OAuth preset")?;
        let out = refresh(&app, &acc.refresh_token).await?;
        acc.access_token = out.access_token;
        if !out.refresh_token.is_empty() {
            acc.refresh_token = out.refresh_token;
        }
        acc.token_expires = expires_at(out.expires_in);
        crate::drive::persist_account(acc);
    }
    if acc.access_token.is_empty() {
        return Err("not signed in: Drives > Accounts > Connect".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn b64url_no_pad() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }
    #[test]
    fn extract_code_forms() {
        assert_eq!(extract_code(" abc123 ").unwrap(), "abc123");
        assert_eq!(
            extract_code("http://127.0.0.1:53682/?code=4%2Fxyz&state=s").unwrap(),
            "4/xyz"
        );
        assert!(extract_code("http://127.0.0.1:53682/?error=access_denied").is_err());
        assert!(extract_code("").is_err());
    }
    #[test]
    fn auth_url_fixed_redirect() {
        let app = OAuthApp {
            authorize_url: "https://example.com/auth".into(),
            token_url: "https://example.com/token".into(),
            scopes: "a b".into(),
            client_id: "cid".into(),
            client_secret: String::new(),
            use_pkce: true,
            extra_auth_params: vec![("access_type".into(), "offline".into())],
        };
        let u = authorize_url(&app, REDIRECT_URI, "chal", "st");
        assert!(u.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2F"), "{u}");
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("access_type=offline"));
        assert_eq!(REDIRECT_URI, "http://127.0.0.1:53682/");
    }
}
