//! AllDebrid client (https://docs.alldebrid.com). Bearer apikey auth
//! (alldebrid.com/apikeys); apikey never in URL, never logged.
//! API returns HTTP 200 with `{status:"error",error:{code,message}}`
//! even on failure — envelope always checked before HTTP status use.
//! Limits: 12 req/s, 600 req/min (caller throttles; no retry here).
//! POST endpoints take query params (reqwest has no `form` feature in
//! this crate): params percent-encoded into URL, empty body.

const BASE_V4: &str = "https://api.alldebrid.com/v4";
const BASE_V41: &str = "https://api.alldebrid.com/v4.1";

#[derive(Clone, Debug, PartialEq)]
pub struct AdUser {
    pub username: String,
    pub email: String,
    pub is_premium: bool,
    pub is_trial: bool,
    /// Unix seconds; `None` when absent.
    pub premium_until: Option<i64>,
}

impl AdUser {
    pub fn premium_active(&self, now_unix: i64) -> bool {
        self.is_premium && self.premium_until.map(|u| u > now_unix).unwrap_or(true)
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AdHost {
    /// Display name, e.g. "Rapidgator".
    pub name: String,
    /// Canonical host id, e.g. "rapidgator.net".
    pub host: String,
    pub domains: Vec<String>,
    /// `None` when API gives no status for this host.
    pub status: Option<bool>,
    /// Per-account quota (v4.1 user/hosts only), units as API returns.
    pub quota: Option<i64>,
    pub quota_remaining: Option<i64>,
}

/// True when `hosts` supports URL or bare host `url_or_host`.
/// Case-insensitive, ignores scheme/path and a leading `www.` on either side.
pub fn host_supported(hosts: &[AdHost], url_or_host: &str) -> bool {
    let want = normalize_host(url_or_host);
    if want.is_empty() {
        return false;
    }
    hosts.iter().any(|h| {
        std::iter::once(h.host.as_str())
            .chain(std::iter::once(h.name.as_str()))
            .chain(h.domains.iter().map(String::as_str))
            .any(|c| normalize_host(c) == want)
    })
}

fn normalize_host(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    let host = s.split(['/', '?', '#', ':']).next().unwrap_or("");
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AdUnlock {
    /// Direct download link; `None` when queued as delayed.
    pub link: Option<String>,
    pub delayed_id: Option<i64>,
    pub filename: String,
    pub filesize: Option<i64>,
    pub host: String,
}

impl AdUnlock {
    pub fn is_delayed(&self) -> bool {
        self.delayed_id.is_some() && self.link.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AdDelayed {
    /// 2 = ready; 1 = in progress (poll again after >= 5s).
    pub status: i64,
    pub link: Option<String>,
    pub filename: String,
    pub filesize: Option<i64>,
}

impl AdDelayed {
    pub fn is_ready(&self) -> bool {
        self.status == 2
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AdMagnetUpload {
    pub id: i64,
    pub hash: String,
    pub name: String,
    /// True when already cached: files available without waiting.
    pub ready: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AdMagnetStatus {
    pub id: i64,
    pub filename: String,
    pub status: String,
    pub status_code: i32,
    pub size: Option<i64>,
    pub hash: String,
    /// Files embedded in status payload (`magnets[].files` tree, flattened).
    pub files: Vec<AdMagnetFile>,
}

impl AdMagnetStatus {
    pub fn is_ready(&self) -> bool {
        is_ready(self.status_code)
    }
}

/// AllDebrid magnet ready code (`statusCode == 4`).
pub fn is_ready(status_code: i32) -> bool {
    status_code == 4
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct AdMagnetFile {
    pub name: String,
    pub size: Option<i64>,
    pub link: String,
}

#[derive(Clone)]
pub struct AdClient {
    http: reqwest::Client,
    api_key: String,
}

impl AdClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_client(reqwest::Client::new(), api_key)
    }

    pub fn with_client(http: reqwest::Client, api_key: impl Into<String>) -> Self {
        Self {
            http,
            api_key: api_key.into(),
        }
    }

    pub async fn user(&self) -> Result<AdUser, String> {
        parse_user(&self.get(BASE_V4, "user", &[]).await?)
    }

    /// Public host list (no account quotas).
    pub async fn hosts(&self) -> Result<Vec<AdHost>, String> {
        parse_hosts(&self.get(BASE_V4, "hosts", &[]).await?)
    }

    /// Per-account host quotas (v4.1 only).
    pub async fn user_hosts(&self) -> Result<Vec<AdHost>, String> {
        parse_hosts(&self.get(BASE_V41, "user/hosts", &[]).await?)
    }

    pub async fn unlock(&self, link: &str) -> Result<AdUnlock, String> {
        parse_unlock(&self.post(BASE_V4, "link/unlock", &[("link", link)]).await?)
    }

    /// Poll a delayed unlock; caller waits >= 5s between polls.
    pub async fn delayed(&self, id: i64) -> Result<AdDelayed, String> {
        let id = id.to_string();
        parse_delayed(&self.post(BASE_V4, "link/delayed", &[("id", &id)]).await?)
    }

    pub async fn magnet_upload(&self, magnet: &str) -> Result<AdMagnetUpload, String> {
        parse_magnet_upload(
            &self
                .post(BASE_V4, "magnet/upload", &[("magnets[]", magnet)])
                .await?,
        )
    }

    pub async fn magnet_status(&self, id: i64) -> Result<AdMagnetStatus, String> {
        let id = id.to_string();
        parse_magnet_status(&self.post(BASE_V41, "magnet/status", &[("id", &id)]).await?)
    }

    /// Files/links of a ready magnet (see `is_ready` / `AdMagnetUpload::ready`).
    /// POST /v4/magnet/files dead upstream (MAGNET_INVALID_ID); files from
    /// POST /v4.1/magnet/status payload (`AdMagnetStatus::files`).
    pub async fn magnet_files(&self, id: i64) -> Result<Vec<AdMagnetFile>, String> {
        Ok(self.magnet_status(id).await?.files)
    }

    pub async fn magnet_delete(&self, id: i64) -> Result<(), String> {
        let id = id.to_string();
        self.post(BASE_V4, "magnet/delete", &[("id", &id)])
            .await
            .map(|_| ())
    }

    async fn get(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        self.send(reqwest::Method::GET, base, path, params).await
    }

    async fn post(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        self.send(reqwest::Method::POST, base, path, params).await
    }

    async fn send(
        &self,
        method: reqwest::Method,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let mut url = format!("{base}/{path}");
        if !params.is_empty() {
            let qs: Vec<String> = params
                .iter()
                .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
                .collect();
            url = format!("{url}?{}", qs.join("&"));
        }
        let mut req = self.http.request(method, &url);
        if !self.api_key.is_empty() {
            req = req.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key),
            );
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let code = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        check_response(code, &body)
    }
}

/// Percent-encode one query component (RFC 3986 unreserved pass through).
fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Envelope gate: API errors arrive as HTTP 200 too. Returns `data`.
/// Error string is `CODE: message` from envelope — never contains apikey.
pub fn check_response(http_status: u16, body: &str) -> Result<serde_json::Value, String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if v.get("status").and_then(|s| s.as_str()) == Some("error") {
            let e = &v["error"];
            let code = str_of(e, &["code"]).unwrap_or_else(|| "UNKNOWN".into());
            let msg = str_of(e, &["message"]).unwrap_or_else(|| "AllDebrid error".into());
            return Err(format!("{code}: {msg}"));
        }
        if (200..300).contains(&http_status) {
            if v.get("status").and_then(|s| s.as_str()) == Some("success") {
                return Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null));
            }
            return Ok(v);
        }
    }
    match http_status {
        401 => Err("HTTP 401: unauthorized (check AllDebrid apikey)".into()),
        429 => Err("HTTP 429: rate limited (AllDebrid limit 12/s, 600/min)".into()),
        200..=299 => Err("AllDebrid: unparseable response body".into()),
        _ => Err(format!("AllDebrid HTTP {http_status}")),
    }
}

fn str_of(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

fn bool_of(v: &serde_json::Value, keys: &[&str]) -> bool {
    keys.iter().any(|k| match v.get(*k) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_i64().map(|n| n != 0).unwrap_or(false),
        Some(serde_json::Value::String(s)) => {
            matches!(s.to_ascii_lowercase().as_str(), "true" | "1")
        }
        _ => false,
    })
}

fn i64_of(v: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

fn opt_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    str_of(v, keys).filter(|s| !s.is_empty())
}

/// `data` of GET /v4/user: `{user:{...}}`.
pub fn parse_user(data: &serde_json::Value) -> Result<AdUser, String> {
    let u = data.get("user").unwrap_or(data);
    Ok(AdUser {
        username: str_of(u, &["username"]).unwrap_or_default(),
        email: str_of(u, &["email"]).unwrap_or_default(),
        is_premium: bool_of(u, &["isPremium", "is_premium"]),
        is_trial: bool_of(u, &["isTrial", "is_trial"]),
        premium_until: i64_of(u, &["premiumUntil", "premium_until"]),
    })
}

/// `data` of GET /v4/hosts and GET /v4.1/user/hosts. `data.hosts`
/// is a list on some endpoints, a host-keyed map on others.
pub fn parse_hosts(data: &serde_json::Value) -> Result<Vec<AdHost>, String> {
    let hosts = data.get("hosts").unwrap_or(data);
    let mut out = Vec::new();
    match hosts {
        serde_json::Value::Array(list) => {
            for h in list {
                out.push(parse_host("", h));
            }
        }
        serde_json::Value::Object(map) => {
            for (key, h) in map {
                out.push(parse_host(key, h));
            }
        }
        _ => return Err("AllDebrid hosts: unexpected `hosts` shape".into()),
    }
    Ok(out)
}

fn parse_host(key: &str, h: &serde_json::Value) -> AdHost {
    let domains = match h.get("domains") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|d| d.as_str().map(str::to_string))
            .collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    let status = match h.get("status").or_else(|| h.get("supported")) {
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(serde_json::Value::String(s)) => Some(matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "1" | "up"
        )),
        _ => None,
    };
    AdHost {
        name: str_of(h, &["name"]).unwrap_or_else(|| key.to_string()),
        host: str_of(h, &["host", "domain"]).unwrap_or_else(|| key.to_string()),
        domains,
        status,
        quota: i64_of(h, &["quota", "quotaMax", "maxQuota"]),
        quota_remaining: i64_of(h, &["quotaRemaining", "quota_remaining", "remainingQuota"]),
    }
}

/// `data` of POST /v4/link/unlock: direct `{link,...}` or `{delayed:id}`.
pub fn parse_unlock(data: &serde_json::Value) -> Result<AdUnlock, String> {
    Ok(AdUnlock {
        link: opt_str(data, &["link"]),
        delayed_id: i64_of(data, &["delayed", "delayedId", "delayed_id"]),
        filename: str_of(data, &["filename"]).unwrap_or_default(),
        filesize: i64_of(data, &["filesize", "size"]),
        host: str_of(data, &["host"]).unwrap_or_default(),
    })
}

/// `data` of POST /v4/link/delayed.
pub fn parse_delayed(data: &serde_json::Value) -> Result<AdDelayed, String> {
    Ok(AdDelayed {
        status: i64_of(data, &["status"]).unwrap_or(0),
        link: opt_str(data, &["link"]),
        filename: str_of(data, &["filename"]).unwrap_or_default(),
        filesize: i64_of(data, &["filesize", "size"]),
    })
}

fn first_magnet(data: &serde_json::Value) -> Result<&serde_json::Value, String> {
    match data.get("magnets") {
        Some(serde_json::Value::Array(a)) => a
            .first()
            .ok_or_else(|| "AllDebrid: empty magnets list".into()),
        _ => data
            .get("magnet")
            .ok_or_else(|| "AllDebrid: missing magnets in response".into()),
    }
}

/// `data` of POST /v4/magnet/upload: `{magnets:[{id,hash,name,ready,...}]}`.
pub fn parse_magnet_upload(data: &serde_json::Value) -> Result<AdMagnetUpload, String> {
    let m = first_magnet(data)?;
    Ok(AdMagnetUpload {
        id: i64_of(m, &["id"]).unwrap_or(0),
        hash: str_of(m, &["hash"]).unwrap_or_default(),
        name: str_of(m, &["name", "filename"]).unwrap_or_default(),
        ready: bool_of(m, &["ready"]),
    })
}

/// `data` of POST /v4.1/magnet/status: `{magnets:[{statusCode,...,
/// files:[...]}]}`; embedded `files` tree flattened via `parse_magnet_files`.
pub fn parse_magnet_status(data: &serde_json::Value) -> Result<AdMagnetStatus, String> {
    let m = first_magnet(data)?;
    Ok(AdMagnetStatus {
        id: i64_of(m, &["id"]).unwrap_or(0),
        filename: str_of(m, &["filename", "name"]).unwrap_or_default(),
        status: str_of(m, &["status"]).unwrap_or_default(),
        status_code: i64_of(m, &["statusCode", "status_code"]).unwrap_or(0) as i32,
        size: i64_of(m, &["size"]),
        hash: str_of(m, &["hash"]).unwrap_or_default(),
        files: parse_magnet_files(data),
    })
}

/// `data` of magnet payload (`magnets[].files`, e.g. embedded in status):
/// nested file tree flattened to leaf files with direct links
/// (`l`/`link` keys, `e` = children).
pub fn parse_magnet_files(data: &serde_json::Value) -> Vec<AdMagnetFile> {
    let mut out = Vec::new();
    if let Ok(m) = first_magnet(data) {
        if let Some(files) = m.get("files") {
            walk_files(files, &mut out);
        }
    }
    out
}

fn walk_files(v: &serde_json::Value, out: &mut Vec<AdMagnetFile>) {
    match v {
        serde_json::Value::Array(list) => {
            for item in list {
                walk_files(item, out);
            }
        }
        serde_json::Value::Object(_) => {
            if let Some(link) = opt_str(v, &["l", "link"]) {
                out.push(AdMagnetFile {
                    name: str_of(v, &["n", "name"]).unwrap_or_default(),
                    size: i64_of(v, &["s", "size"]),
                    link,
                });
            }
            if let Some(children) = v.get("e") {
                walk_files(children, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_premium_expired_trial() {
        // Fixture: GET /v4/user -> data.user shape (docs, 2026-10-19).
        let premium = parse_user(&json!({"user":{
            "username":"bob","email":"b@x.io","isPremium":true,
            "isTrial":false,"premiumUntil":2_000_000_000,"lang":"en"
        }}))
        .unwrap();
        assert_eq!(premium.username, "bob");
        assert!(premium.is_premium && !premium.is_trial);
        assert!(premium.premium_active(1_700_000_000));
        assert!(!premium.premium_active(2_100_000_000), "past premiumUntil");

        let expired = parse_user(&json!({"user":{
            "username":"eve","email":"e@x.io","isPremium":false,
            "isTrial":false,"premiumUntil":1_600_000_000
        }}))
        .unwrap();
        assert!(!expired.premium_active(1_700_000_000));

        let trial = parse_user(&json!({"user":{
            "username":"tri","email":"t@x.io","isPremium":true,
            "isTrial":true,"premiumUntil":2_000_000_000
        }}))
        .unwrap();
        assert!(trial.is_trial && trial.premium_active(1_700_000_000));
    }

    #[test]
    fn hosts_list_map_and_user_quota_shapes() {
        // Fixture: GET /v4/hosts public list shape.
        let list = parse_hosts(&json!({"hosts":[
            {"name":"Rapidgator","host":"rapidgator.net","status":true,
             "domains":["rapidgator.net","rg.to"]},
            {"name":"1fichier","host":"1fichier.com","status":true,
             "domains":["1fichier.com"]}
        ]}))
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, Some(true));
        assert_eq!(list[0].quota, None);

        // Fixture: host-keyed map shape.
        let map = parse_hosts(&json!({"hosts":{
            "rapidgator.net":{"name":"Rapidgator","status":true,
             "domains":["rapidgator.net"]}
        }}))
        .unwrap();
        assert_eq!(map[0].host, "rapidgator.net");

        // Fixture: GET /v4.1/user/hosts per-account quota shape.
        let acct = parse_hosts(&json!({"hosts":[
            {"name":"Rapidgator","host":"rapidgator.net","status":true,
             "quota":100,"quotaRemaining":42}
        ]}))
        .unwrap();
        assert_eq!(acct[0].quota, Some(100));
        assert_eq!(acct[0].quota_remaining, Some(42));
    }

    #[test]
    fn host_matching_case_www_and_url() {
        let hosts = parse_hosts(&json!({"hosts":[
            {"name":"Rapidgator","host":"rapidgator.net","status":true,
             "domains":["rapidgator.net","rg.to"]}
        ]}))
        .unwrap();
        // Rapidgator test link from case study.
        assert!(host_supported(
            &hosts,
            "https://rapidgator.net/file/b6df95dee983c06f41b3caa31755a9fe/Cas1tlevBelm7ontsCur4se-elamigos.part1.rar.html"
        ));
        assert!(host_supported(&hosts, "https://www.RAPIDGATOR.net/file/x"));
        assert!(host_supported(&hosts, "RG.to"));
        assert!(host_supported(&hosts, "rapidgator.net"));
        assert!(!host_supported(&hosts, "https://example.com/file/x"));
        assert!(!host_supported(&hosts, "https://notrapidgator.net/x"));
        assert!(!host_supported(&hosts, ""));
    }

    #[test]
    fn unlock_direct_and_delayed() {
        // Fixture: POST /v4/link/unlock direct.
        let direct = parse_unlock(&json!({
            "link":"https://cdn.alldebrid.com/dl/abc/file.rar",
            "host":"rapidgator.net","filename":"file.rar","filesize":12345
        }))
        .unwrap();
        assert!(direct.link.is_some() && !direct.is_delayed());
        assert_eq!(direct.filesize, Some(12345));

        // Fixture: unlock queued -> delayed id, poll link/delayed.
        let queued = parse_unlock(&json!({"delayed":98765,"host":"rapidgator.net"})).unwrap();
        assert_eq!(queued.delayed_id, Some(98765));
        assert!(queued.link.is_none() && queued.is_delayed());
    }

    #[test]
    fn delayed_ready_vs_in_progress() {
        // Fixture: POST /v4/link/delayed, status 2 = ready.
        let ready = parse_delayed(&json!({
            "status":2,"link":"https://cdn.alldebrid.com/dl/abc/file.rar",
            "filename":"file.rar","filesize":12345
        }))
        .unwrap();
        assert!(ready.is_ready() && ready.link.is_some());

        let busy = parse_delayed(&json!({"status":1})).unwrap();
        assert!(!busy.is_ready() && busy.link.is_none());
    }

    #[test]
    fn magnet_upload_and_status_ready_codes() {
        // Fixture: POST /v4/magnet/upload (ready=true = cached).
        let up = parse_magnet_upload(&json!({"magnets":[{
            "magnet":"magnet:?xt=urn:btih:B38E5B966A72F86834EF0EFB59388D636E82473B",
            "name":"thing","hash":"B38E5B966A72F86834EF0EFB59388D636E82473B",
            "id":4242,"ready":true
        }]}))
        .unwrap();
        assert_eq!(up.id, 4242);
        assert_eq!(up.hash, "B38E5B966A72F86834EF0EFB59388D636E82473B");
        assert!(up.ready);

        // Fixture: POST /v4.1/magnet/status, statusCode 4 = ready.
        for (code, want) in [(4, true), (1, false), (2, false), (3, false)] {
            let st = parse_magnet_status(&json!({"magnets":[{
                "id":4242,"filename":"thing","status":"Ready","statusCode":code,
                "size":999,"hash":"B38E5B966A72F86834EF0EFB59388D636E82473B"
            }]}))
            .unwrap();
            assert_eq!(st.is_ready(), want, "statusCode {code}");
            assert_eq!(is_ready(code), want);
        }
    }

    #[test]
    fn magnet_status_embedded_files_populated() {
        // Fixture: POST /v4.1/magnet/status embeds magnets[].files tree;
        // POST /v4/magnet/files dead upstream (MAGNET_INVALID_ID).
        let st = parse_magnet_status(&json!({"magnets":[{
            "id":4242,"filename":"thing","status":"Ready","statusCode":4,
            "size":1500,"hash":"B38E5B966A72F86834EF0EFB59388D636E82473B",
            "files":[
                {"n":"root.rar","s":100,"l":"https://cdn/dl/root.rar"},
                {"n":"folder","e":[
                    {"n":"part1.rar","s":200,"l":"https://cdn/dl/part1.rar"},
                    {"n":"sub","e":[
                        {"n":"part2.rar","s":300,"l":"https://cdn/dl/part2.rar"},
                        {"n":"part3.rar","s":400,"l":"https://cdn/dl/part3.rar"},
                        {"n":"part4.rar","s":500,"l":"https://cdn/dl/part4.rar"}
                    ]}
                ]}
            ]
        }]}))
        .unwrap();
        assert!(st.is_ready());
        assert_eq!(st.status_code, 4);
        assert_eq!(st.size, Some(1500));
        assert_eq!(st.files.len(), 5);
        assert_eq!(st.files[0].name, "root.rar");
        assert_eq!(st.files[0].size, Some(100));
        assert_eq!(st.files[0].link, "https://cdn/dl/root.rar");
        assert_eq!(st.files[2].name, "part2.rar");
        assert_eq!(st.files[4].name, "part4.rar");
        assert_eq!(st.files[4].size, Some(500));
        assert_eq!(st.files[4].link, "https://cdn/dl/part4.rar");

        // No `files` key -> empty, no error (still downloading).
        let busy = parse_magnet_status(&json!({"magnets":[{
            "id":1,"filename":"x","status":"Downloading","statusCode":1
        }]}))
        .unwrap();
        assert!(busy.files.is_empty() && !busy.is_ready());
    }

    #[test]
    fn magnet_files_flatten_nested_tree() {
        // Fixture: magnets[].files nested `e` tree, leaves carry `l`.
        let files = parse_magnet_files(&json!({"magnets":[{"id":1,"files":[
            {"n":"root.rar","s":100,"l":"https://cdn/dl/root.rar"},
            {"n":"folder","e":[
                {"n":"part1.rar","s":200,"l":"https://cdn/dl/part1.rar"},
                {"n":"part2.rar","s":300,"l":"https://cdn/dl/part2.rar"}
            ]}
        ]}]}));
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].name, "part1.rar");
        assert_eq!(files[1].size, Some(200));
        assert_eq!(files[2].link, "https://cdn/dl/part2.rar");
    }

    #[test]
    fn error_envelopes_surface_code_and_message() {
        // Fixtures: HTTP 200 error envelopes (verified API behavior).
        for (code, body) in [
            (
                "AUTH_MISSING_APIKEY",
                json!({"status":"error","error":{"code":"AUTH_MISSING_APIKEY","message":"You must provide an apikey"}}),
            ),
            (
                "MUST_BE_PREMIUM",
                json!({"status":"error","error":{"code":"MUST_BE_PREMIUM","message":"You must be premium to use this feature"}}),
            ),
            (
                "MAGNET_MUST_BE_PREMIUM",
                json!({"status":"error","error":{"code":"MAGNET_MUST_BE_PREMIUM","message":"You must be premium to use magnets"}}),
            ),
        ] {
            let err = check_response(200, &body.to_string()).unwrap_err();
            assert!(err.starts_with(&format!("{code}: ")), "{err}");
            assert!(!err.contains("Bearer"), "no credential leak: {err}");
        }
    }

    #[test]
    fn http_401_429_and_success_envelope() {
        assert!(check_response(401, "not json").unwrap_err().contains("401"));
        assert!(check_response(429, "not json").unwrap_err().contains("429"));
        let data = check_response(
            200,
            &json!({"status":"success","data":{"user":{"username":"bob"}}}).to_string(),
        )
        .unwrap();
        assert_eq!(data["user"]["username"], "bob");
    }

    #[test]
    fn pct_encode_escapes_magnet_and_link() {
        assert_eq!(
            pct_encode("magnet:?xt=a&b=c d"),
            "magnet%3A%3Fxt%3Da%26b%3Dc%20d"
        );
        assert_eq!(pct_encode("magnets[]"), "magnets%5B%5D");
        assert_eq!(pct_encode("abc-._~"), "abc-._~");
    }
}
