//! Debrid normalization layer over Real-Debrid (`crate::debrid_rd`),
//! AllDebrid (`crate::debrid_ad`), TorBox (`crate::debrid_tb`).
//!
//! Rules: tokens + resolved direct URLs never logged; persisted tasks keep
//! original magnet/hoster URL (direct URLs expire / IP-bound), resolution
//! happens fresh on every download start with same reqwest client used for
//! download (IP binding). Any debrid failure -> caller falls back.

use crate::model::{DebridProviderCfg, Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Rd,
    Ad,
    Tb,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Rd => "Real-Debrid",
            Provider::Ad => "AllDebrid",
            Provider::Tb => "TorBox",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Provider::Rd => "rd",
            Provider::Ad => "ad",
            Provider::Tb => "tb",
        }
    }
    pub fn from_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rd" | "real-debrid" | "realdebrid" => Some(Provider::Rd),
            "ad" | "alldebrid" => Some(Provider::Ad),
            "tb" | "torbox" => Some(Provider::Tb),
            _ => None,
        }
    }
    pub fn cfg(self, s: &Settings) -> &DebridProviderCfg {
        match self {
            Provider::Rd => &s.debrid_rd,
            Provider::Ad => &s.debrid_ad,
            Provider::Tb => &s.debrid_tb,
        }
    }
}

pub const ALL: [Provider; 3] = [Provider::Rd, Provider::Ad, Provider::Tb];

/// Enabled+configured providers in preference order (preferred first,
/// then RD, AD, TB).
pub fn enabled_providers(s: &Settings) -> Vec<Provider> {
    let mut out: Vec<Provider> = Vec::new();
    if let Some(p) = Provider::from_id(&s.debrid_preferred) {
        if p.cfg(s).usable() {
            out.push(p);
        }
    }
    for p in ALL {
        if p.cfg(s).usable() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

pub fn any_for_hosters(s: &Settings) -> bool {
    enabled_providers(s)
        .iter()
        .any(|p| p.cfg(s).use_for_hosters)
}
pub fn any_for_magnets(s: &Settings) -> bool {
    enabled_providers(s)
        .iter()
        .any(|p| p.cfg(s).use_for_magnets)
}

#[derive(Clone, Debug, Default)]
pub struct Account {
    pub provider: &'static str,
    pub username: String,
    pub email: String,
    pub plan: String,
    pub is_premium: bool,
    /// Unix seconds.
    pub expires_at: Option<i64>,
}

impl Account {
    pub fn summary(&self) -> String {
        match days_left(self.expires_at, now_unix()) {
            Some(d) if d >= 0 => format!(
                "{}: {} <{}> plan {} premium until {} ({}d left)",
                self.provider,
                self.username,
                self.email,
                self.plan,
                fmt_date(self.expires_at.unwrap_or(0)),
                d
            ),
            _ if self.is_premium => format!(
                "{}: {} <{}> plan {} premium (no expiry reported)",
                self.provider, self.username, self.email, self.plan
            ),
            _ => format!(
                "{}: {} <{}> plan {} NOT premium (free/expired)",
                self.provider, self.username, self.email, self.plan
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostStatus {
    pub name: String,
    pub domain: String,
    pub up: bool,
    pub supported: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedFile {
    pub name: String,
    pub size: Option<u64>,
    /// Direct download URL. Never persist, never log.
    pub url: String,
    /// Provider already knew file cached (instant availability). `None`
    /// = API gave no cheap signal. Display only; never persist.
    pub cached: Option<bool>,
}

/// Pure builder for `DownloadTask::debrid_info`:
/// `"<Provider label> · <hoster host> · <cached|cached: unknown|not cached>"`.
/// Magnet URLs have no hoster host; caller passes name-derived label
/// (`"magnet"`). Empty host -> omitted only if caller passes empty string;
/// keeps format exact for hoster path (host always from `url_host`).
pub fn format_debrid_info(provider: &str, host: &str, cached: Option<bool>) -> String {
    let cache = match cached {
        Some(true) => "cached",
        Some(false) => "not cached",
        None => "cached: unknown",
    };
    format!("{provider} · {host} · {cache}")
}

/// ISO-8601 `2026-11-18T10:00:00.000Z` -> unix secs (UTC), via TB parser.
pub fn parse_iso_unix(s: &str) -> Option<i64> {
    crate::debrid_tb::parse_expiry(s)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn days_left(expires_at: Option<i64>, now: i64) -> Option<i64> {
    expires_at.map(|e| (e - now).div_euclid(86400))
}

/// YYYY-MM-DD UTC, no chrono dep.
pub fn fmt_date(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    // civil-from-days (Hinnant)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Strip scheme/www, lowercase host from URL. Empty on garbage.
pub fn url_host(url: &str) -> String {
    let s = url.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    let host = s.split(['/', '?', '#', ':']).next().unwrap_or("");
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

pub fn is_magnet(url: &str) -> bool {
    crate::torrent::is_magnet(url)
}

/// btih hash from magnet (hex 40 or base32 32), lowercase hex if hex.
pub fn magnet_hash(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let i = lower.find("urn:btih:")? + "urn:btih:".len();
    let rest = &lower[i..];
    let h: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    // hex40 or base32-32 both usable (providers accept magnet itself)
    if (h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())) || h.len() == 32 {
        Some(h)
    } else {
        None
    }
}

// ---------- per-provider dispatch ----------

pub async fn account(http: reqwest::Client, p: Provider, key: &str) -> Result<Account, String> {
    match p {
        Provider::Rd => {
            let c = crate::debrid_rd::RdClient::new(key, http);
            let u = c.user().await?;
            // RD expiry: ISO string `expiration`; premium secs authoritative.
            let expires_at = u
                .expiration
                .as_deref()
                .and_then(parse_iso_unix)
                .or_else(|| {
                    if u.premium > 0 {
                        Some(now_unix() + u.premium)
                    } else {
                        None
                    }
                });
            let is_premium = u.is_premium();
            Ok(Account {
                provider: "Real-Debrid",
                username: u.username,
                email: u.email,
                plan: u.account_type,
                is_premium,
                expires_at,
            })
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let u = c.user().await?;
            let is_premium = u.premium_active(now_unix());
            Ok(Account {
                provider: "AllDebrid",
                username: u.username,
                email: u.email,
                plan: if u.is_trial {
                    "trial".into()
                } else if u.is_premium {
                    "premium".into()
                } else {
                    "free".into()
                },
                is_premium,
                expires_at: u.premium_until,
            })
        }
        Provider::Tb => {
            // TbClient builds redirect-none client; account call unaffected.
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            let u = c.me().await?;
            let is_premium = u.is_premium();
            Ok(Account {
                provider: "TorBox",
                username: u.email.clone().unwrap_or_default(),
                email: u.email.unwrap_or_default(),
                plan: crate::debrid_tb::plan_label(u.plan).into(),
                is_premium,
                expires_at: u.expires_at,
            })
        }
    }
}

// ---------- hosts cache (successful lists only, 10 min TTL) ----------
const HOSTS_TTL: std::time::Duration = std::time::Duration::from_secs(600);
type HostsCache = std::collections::HashMap<(Provider, u64), (std::time::Instant, Vec<HostStatus>)>;
static HOSTS_CACHE: std::sync::OnceLock<std::sync::Mutex<HostsCache>> = std::sync::OnceLock::new();
fn hosts_cache() -> &'static std::sync::Mutex<HostsCache> {
    HOSTS_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
/// Hash only: raw key never stored/logged; token change -> cache miss.
fn key_hash(key: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}
fn hosts_cache_get(cache: &HostsCache, p: Provider, kh: u64, now: std::time::Instant) -> Option<Vec<HostStatus>> {
    let (at, hosts) = cache.get(&(p, kh))?;
    (now.duration_since(*at) < HOSTS_TTL).then(|| hosts.clone())
}
fn hosts_cache_put(cache: &mut HostsCache, p: Provider, kh: u64, now: std::time::Instant, hosts: Vec<HostStatus>) {
    cache.insert((p, kh), (now, hosts));
}

pub async fn hosts(
    http: reqwest::Client,
    p: Provider,
    key: &str,
) -> Result<Vec<HostStatus>, String> {
    let kh = key_hash(key);
    if let Some(hit) = hosts_cache().lock().ok().and_then(|c| hosts_cache_get(&c, p, kh, std::time::Instant::now())) {
        return Ok(hit);
    }
    let list = hosts_uncached(http, p, key).await?;
    if let Ok(mut c) = hosts_cache().lock() {
        hosts_cache_put(&mut c, p, kh, std::time::Instant::now(), list.clone());
    }
    Ok(list)
}

async fn hosts_uncached(
    http: reqwest::Client,
    p: Provider,
    key: &str,
) -> Result<Vec<HostStatus>, String> {
    match p {
        Provider::Rd => {
            let c = crate::debrid_rd::RdClient::new(key, http);
            Ok(c.hosts_status()
                .await?
                .into_iter()
                .map(|h| {
                    let up = h.is_up();
                    HostStatus {
                        name: if h.name.is_empty() {
                            h.domain.clone()
                        } else {
                            h.name
                        },
                        up,
                        domain: h.domain,
                        supported: h.supported,
                    }
                })
                .collect())
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            // per-account quotas when key present, else public
            let list = if key.trim().is_empty() {
                c.hosts().await?
            } else {
                c.user_hosts().await.or(c.hosts().await)?
            };
            Ok(list
                .into_iter()
                .map(|h| HostStatus {
                    name: h.name,
                    domain: if h.host.is_empty() {
                        h.domains.first().cloned().unwrap_or_default()
                    } else {
                        h.host
                    },
                    up: h.status.unwrap_or(false),
                    supported: h.status.is_some(),
                })
                .collect())
        }
        Provider::Tb => {
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            Ok(c.hosters()
                .await?
                .into_iter()
                .map(|h| HostStatus {
                    name: h.name,
                    domain: h.domains.first().cloned().unwrap_or_default(),
                    up: h.status,
                    supported: true,
                })
                .collect())
        }
    }
}

pub fn host_up(hosts: &[HostStatus], url: &str) -> Option<bool> {
    let want = url_host(url);
    if want.is_empty() {
        return None;
    }
    hosts
        .iter()
        .find(|h| {
            let d = url_host(&h.domain);
            d == want || want.ends_with(&format!(".{d}"))
        })
        .map(|h| h.supported && h.up)
}

/// Check-then-sleep poll: first check immediate; interval slept only
/// before later checks, at most max_polls checks total.
async fn poll_delayed<C, CFut, S, SFut, T>(max_polls: usize, interval: std::time::Duration, mut sleep: S, mut check: C) -> Result<Option<T>, String>
where
    C: FnMut() -> CFut,
    CFut: std::future::Future<Output = Result<Option<T>, String>>,
    S: FnMut(std::time::Duration) -> SFut,
    SFut: std::future::Future<Output = ()>,
{
    for i in 0..max_polls {
        if i > 0 { sleep(interval).await; }
        if let Some(v) = check().await? { return Ok(Some(v)); }
    }
    Ok(None)
}

/// Resolve hoster URL to direct link via provider. Err = unsupported/fail.
pub async fn unrestrict(
    http: reqwest::Client,
    p: Provider,
    key: &str,
    url: &str,
) -> Result<ResolvedFile, String> {
    match p {
        Provider::Rd => {
            let c = crate::debrid_rd::RdClient::new(key, http);
            let r = c.unrestrict_link(url).await?;
            // ponytail: RD hoster unrestrict/link returns no cache-residency
            // field (only magnet instantAvailability, disabled upstream);
            // cached unknown, not fabricated. Add RD check-based signal
            // only if API exposes residency.
            Ok(ResolvedFile {
                name: r.filename,
                size: r.filesize,
                url: r.download,
                cached: None,
            })
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let u = c.unlock(url).await?;
            if let Some(link) = u.link {
                return Ok(ResolvedFile {
                    name: u.filename,
                    size: u.filesize.map(|n| n.max(0) as u64),
                    url: link,
                    cached: None,
                });
            }
            if let Some(id) = u.delayed_id {
                // First delayed poll immediate (unlock satisfies AD >=5s
                // spacing), then 5s polls, up to ~60s.
                let ready = poll_delayed(13, std::time::Duration::from_secs(5), |d| tokio::time::sleep(d), || async {
                    let d = c.delayed(id).await?;
                    Ok::<Option<crate::debrid_ad::AdDelayed>, String>(d.is_ready().then_some(d))
                }).await?;
                if let Some(d) = ready {
                    if let Some(link) = d.link {
                        return Ok(ResolvedFile {
                            name: d.filename,
                            size: d.filesize.map(|n| n.max(0) as u64),
                            url: link,
                            cached: None,
                        });
                    }
                }
                return Err("AllDebrid: delayed link not ready after 60s".into());
            }
            Err("AllDebrid: unlock returned no link".into())
        }
        Provider::Tb => {
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            let w = c.create_web_download(url).await?;
            let f = w
                .files
                .first()
                .ok_or("TorBox: web download has no files yet")?;
            let link = c.request_web_dl(w.web_id, f.id).await?;
            Ok(ResolvedFile {
                name: if f.name.is_empty() {
                    w.name
                } else {
                    f.name.clone()
                },
                size: Some(f.size),
                url: link,
                cached: None,
            })
        }
    }
}

/// Cached check for magnet hash. Ok(false) = not cached.
pub async fn magnet_cached(
    http: reqwest::Client,
    p: Provider,
    key: &str,
    magnet: &str,
) -> Result<bool, String> {
    match p {
        Provider::Rd => {
            let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
            let c = crate::debrid_rd::RdClient::new(key, http);
            // instantAvailability disabled upstream (403 disabled_endpoint,
            // live 2026-10-19): degrade to unknown=not-cached-confirmable,
            // callers polling add-flow will still work. Report false here:
            // cached-check is advisory; magnet_files re-checks cached_only.
            match c.instant_availability(&hash).await {
                Ok(a) => Ok(a.cached),
                Err(_) => Ok(false),
            }
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let up = c.magnet_upload(magnet).await?;
            if up.ready {
                return Ok(true);
            }
            let st = c.magnet_status(up.id).await?;
            Ok(st.is_ready())
        }
        Provider::Tb => {
            let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            Ok(c.check_cached(&hash).await?.cached)
        }
    }
}

/// Add magnet via provider, poll until files ready, return per-file links.
/// Single provider attempt; caller tries next provider / local fallback.
pub async fn magnet_files(
    http: reqwest::Client,
    p: Provider,
    key: &str,
    magnet: &str,
    cached_only: bool,
) -> Result<Vec<ResolvedFile>, String> {
    match p {
        Provider::Rd => {
            if cached_only {
                // instantAvailability disabled upstream (403 disabled_endpoint):
                // cannot confirm cache -> treat as not cached for cached_only
                // (local fallback), never hard-fail with endpoint error.
                let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
                let c0 = crate::debrid_rd::RdClient::new(key, http.clone());
                let cached = c0
                    .instant_availability(&hash)
                    .await
                    .map(|a| a.cached)
                    .unwrap_or(false);
                if !cached {
                    return Err("Real-Debrid: magnet not confirmed cached (cached_only; instantAvailability disabled upstream)".into());
                }
            }
            let c = crate::debrid_rd::RdClient::new(key, http);
            let id = c.add_magnet(magnet).await?;
            // wait download status then select all + links
            let mut info = None;
            for i in 0..30 {
                let t = c.torrent_info(&id).await?;
                if t.status == "waiting_files_selection" {
                    let ids: Vec<String> = t.files.iter().map(|f| f.id.to_string()).collect();
                    if !ids.is_empty() {
                        c.select_files(&id, &ids.join(",")).await?;
                    }
                }
                if !t.links.is_empty()
                    && (t.status == "downloaded"
                        || t.status == "waiting_files_selection"
                        || !t.files.is_empty())
                {
                    // need selected files; poll until links appear after selection
                    if t.status == "downloaded" || i > 2 {
                        info = Some(t);
                        break;
                    }
                }
                if t.status == "error" || t.status == "magnet_error" || t.status == "dead" {
                    return Err(format!("Real-Debrid torrent status {}", t.status));
                }
                info = Some(t);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            let t = info.ok_or("Real-Debrid: no torrent info")?;
            if t.links.is_empty() {
                return Err(format!(
                    "Real-Debrid: torrent not ready (status {})",
                    t.status
                ));
            }
            let mut out = Vec::new();
            // links correspond to selected files in order; unrestrict each
            let selected: Vec<_> = t.files.iter().filter(|f| f.selected == 1).collect();
            for (i, link) in t.links.iter().enumerate() {
                let r = c.unrestrict_link(link).await?;
                let name = if !r.filename.is_empty() {
                    r.filename
                } else if let Some(f) = selected.get(i) {
                    f.path
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&f.path)
                        .to_string()
                } else {
                    String::new()
                };
                out.push(ResolvedFile {
                    name,
                    size: r.filesize,
                    url: r.download,
                    // RD links present == torrent finished on RD side, but
                    // instant-availability field unavailable; unknown.
                    cached: None,
                });
            }
            Ok(out)
        }
        Provider::Ad => {
            let c = crate::debrid_ad::AdClient::with_client(http, key);
            let up = c.magnet_upload(magnet).await?;
            if cached_only && !up.ready {
                // check status once
                let st = c.magnet_status(up.id).await?;
                if !st.is_ready() {
                    let _ = c.magnet_delete(up.id).await;
                    return Err("AllDebrid: magnet not cached (cached_only)".into());
                }
            }
            if !up.ready {
                let mut ready = false;
                for _ in 0..30 {
                    let st = c.magnet_status(up.id).await?;
                    if st.is_ready() {
                        ready = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                if !ready {
                    return Err("AllDebrid: magnet not ready after polling".into());
                }
            }
            let files = c.magnet_files(up.id).await?;
            if files.is_empty() {
                return Err("AllDebrid: magnet has no files/links".into());
            }
            // file links are hoster links needing unlock
            let mut out = Vec::new();
            for f in files {
                let u = c.unlock(&f.link).await;
                match u {
                    Ok(u) => {
                        if let Some(link) = u.link {
                            out.push(ResolvedFile {
                                name: if f.name.is_empty() {
                                    u.filename
                                } else {
                                    f.name.clone()
                                },
                                size: f.size.map(|n| n.max(0) as u64),
                                url: link,
                                cached: Some(up.ready),
                            });
                        } else if let Some(id) = u.delayed_id {
                            // short poll
                            let mut got = None;
                            for _ in 0..6 {
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                if let Ok(d) = c.delayed(id).await {
                                    if d.is_ready() {
                                        got = d.link;
                                        break;
                                    }
                                }
                            }
                            if let Some(link) = got {
                                out.push(ResolvedFile {
                                    name: f.name.clone(),
                                    size: f.size.map(|n| n.max(0) as u64),
                                    url: link,
                                    cached: Some(up.ready),
                                });
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            if out.is_empty() {
                return Err("AllDebrid: no downloadable files resolved".into());
            }
            Ok(out)
        }
        Provider::Tb => {
            if cached_only {
                let hash = magnet_hash(magnet).ok_or("no btih hash in magnet")?;
                let c0 = crate::debrid_tb::TbClient::new(key);
                if !c0.check_cached(&hash).await?.cached {
                    return Err("TorBox: magnet not cached (cached_only)".into());
                }
            }
            let _ = http;
            let c = crate::debrid_tb::TbClient::new(key);
            let created = c.create_torrent(magnet).await?;
            let tid = created.torrent_id;
            if tid == 0 {
                return Err("TorBox: createtorrent returned no id".into());
            }
            let mut files = Vec::new();
            for _ in 0..30 {
                let list = c.torrent_list(Some(tid)).await?;
                if let Some(t) = list.into_iter().next() {
                    if t.download_ready || !t.files.is_empty() && t.progress == Some(1.0) {
                        files = t.files;
                        break;
                    }
                    if !t.files.is_empty() && t.download_ready {
                        files = t.files;
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            if files.is_empty() {
                return Err("TorBox: torrent not ready / no files after polling".into());
            }
            let mut out = Vec::new();
            for f in files {
                let link = c.request_torrent_dl(tid, f.id).await?;
                out.push(ResolvedFile {
                    name: f.name,
                    size: Some(f.size),
                    url: link,
                    // torrent_list at poll gives ready state but no
                    // cached flag (checkcached not called here) — unknown.
                    cached: None,
                });
            }
            Ok(out)
        }
    }
}

/// Try providers in preference order for hoster URL.
pub async fn resolve_hoster(
    http: reqwest::Client,
    s: &Settings,
    url: &str,
) -> Result<(Provider, ResolvedFile), String> {
    let mut last = String::new();
    let mut tried = false;
    for p in enabled_providers(s) {
        let cfg = p.cfg(s);
        if !cfg.use_for_hosters {
            continue;
        }
        tried = true;
        // Only route known-up hosters; normal HTTP URLs must not be
        // hijacked (or burn unrestrict quota). Hosts fetch fails -> skip
        // provider rather than guessing.
        match hosts(http.clone(), p, &cfg.api_key).await {
            Ok(list) => {
                if host_up(&list, url) != Some(true) {
                    last = format!("{}: hoster unsupported or down", p.label());
                    continue;
                }
            }
            Err(e) => {
                last = format!("{}: hosts check failed: {e}", p.label());
                continue;
            }
        }
        match unrestrict(http.clone(), p, &cfg.api_key, url).await {
            Ok(f) => return Ok((p, f)),
            Err(e) => last = format!("{}: {e}", p.label()),
        }
    }
    if !tried {
        return Err("no debrid provider enabled for hosters".into());
    }
    Err(last)
}

/// Try providers in preference order for magnet.
pub async fn resolve_magnet(
    http: reqwest::Client,
    s: &Settings,
    magnet: &str,
) -> Result<(Provider, Vec<ResolvedFile>), String> {
    let mut last = String::new();
    let mut tried = false;
    for p in enabled_providers(s) {
        let cfg = p.cfg(s);
        if !cfg.use_for_magnets {
            continue;
        }
        tried = true;
        match magnet_files(http.clone(), p, &cfg.api_key, magnet, cfg.cached_only).await {
            Ok(f) if !f.is_empty() => return Ok((p, f)),
            Ok(_) => last = format!("{}: no files", p.label()),
            Err(e) => last = format!("{}: {e}", p.label()),
        }
    }
    if !tried {
        return Err("no debrid provider enabled for magnets".into());
    }
    Err(last)
}

/// Download one task via debrid: resolve original URL -> direct link(s),
/// download first/single matching file via engine, restore original URL on
/// returned task. Multi-file magnets: if task filename matches a resolved
/// file name, download that file; else if >1 files, error (Add-dialog
/// fan-out creates one task per file; never silently pick one).
pub async fn download_via_debrid(
    http: reqwest::Client,
    mut task: crate::model::DownloadTask,
    ctrl: std::sync::Arc<crate::engine::Controls>,
    settings: &Settings,
) -> Result<crate::model::DownloadTask, String> {
    let original_url = task.url.clone();
    let want_file = if !task.debrid_file_name.is_empty() {
        task.debrid_file_name.clone()
    } else {
        task.filename.clone()
    };
    if is_magnet(&original_url) {
        let (p, files) = resolve_magnet(http.clone(), settings, &original_url).await?;
        let chosen = if files.len() == 1 {
            files.into_iter().next().unwrap()
        } else {
            match files.iter().find(|f| f.name == want_file).cloned() {
                Some(f) => f,
                None => {
                    return Err(format!(
                        "debrid magnet resolved {} files; use Add dialog (debrid fan-out creates one task per file) or local torrent. Files: {}",
                        files.len(),
                        files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
                    ))
                }
            }
        };
        // Magnet has no hoster host; token btih hash would be sensitive-ish
        // in UI, so literal "magnet" placeholder keeps format shape.
        // Downloaded stream path restores original URL (match spec).
        let cached = chosen.cached;
        task.url = chosen.url;
        task.debrid_info = format_debrid_info(p.label(), "magnet", cached);
        let debrid_info = task.debrid_info.clone();
        if !chosen.name.is_empty() {
            task.filename = crate::model::sanitize_filename(&chosen.name);
        }
        let mut done = crate::engine::download_http(http, task, ctrl).await?;
        done.url = original_url;
        done.debrid_info = debrid_info;
        // keep resolved filename (done.filename already correct)
        return Ok(done);
    }
    // hoster URL
    let (p, f) = resolve_hoster(http.clone(), settings, &original_url).await?;
    let host = url_host(&original_url);
    task.url = f.url;
    task.debrid_info = format_debrid_info(p.label(), &host, f.cached);
    let debrid_info = task.debrid_info.clone();
    if !f.name.is_empty() {
        task.filename = crate::model::sanitize_filename(&f.name);
    }
    let mut done = crate::engine::download_http(http, task, ctrl).await?;
    done.url = original_url;
    done.debrid_info = debrid_info;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_and_order() {
        let mut s = Settings::default();
        assert!(enabled_providers(&s).is_empty());
        s.debrid_rd.enabled = true;
        s.debrid_rd.api_key = "k".into();
        s.debrid_ad.enabled = true;
        s.debrid_ad.api_key = "k".into();
        s.debrid_preferred = "ad".into();
        assert_eq!(enabled_providers(&s), vec![Provider::Ad, Provider::Rd]);
    }

    #[test]
    fn expiry_days_and_date() {
        assert_eq!(days_left(Some(86400 * 10), 0), Some(10));
        assert_eq!(days_left(Some(0), 86400 * 2), Some(-2));
        assert_eq!(days_left(None, 0), None);
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(1798761600), "2027-01-01");
    }

    #[test]
    fn host_and_magnet_hash() {
        assert_eq!(
            url_host("https://www.RapidGator.net/file/x"),
            "rapidgator.net"
        );
        assert_eq!(url_host("https://rg.to/x"), "rg.to");
        assert_eq!(
            magnet_hash("magnet:?xt=urn:btih:B38E5B966A72F86834EF0EFB59388D636E82473B&dn=x")
                .as_deref(),
            Some("b38e5b966a72f86834ef0efb59388d636e82473b")
        );
        assert_eq!(magnet_hash("https://example.com"), None);
    }

    #[test]
    fn settings_token_roundtrip() {
        // Token save/load roundtrip through Settings JSON (state.json shape).
        let mut s = Settings::default();
        s.debrid_rd.enabled = true;
        s.debrid_rd.api_key = "SECRET_RD".into();
        s.debrid_rd.cached_only = true;
        s.debrid_tb.enabled = true;
        s.debrid_tb.api_key = "SECRET_TB".into();
        s.debrid_preferred = "tb".into();
        let txt = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&txt).unwrap();
        assert_eq!(back.debrid_rd.api_key, "SECRET_RD");
        assert!(back.debrid_rd.cached_only && back.debrid_rd.enabled);
        assert_eq!(back.debrid_tb.api_key, "SECRET_TB");
        assert_eq!(back.debrid_preferred, "tb");
        // old state.json without debrid fields parses with defaults
        let mut v: serde_json::Value = serde_json::from_str(&txt).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("debrid_rd");
        obj.remove("debrid_ad");
        obj.remove("debrid_tb");
        obj.remove("debrid_preferred");
        let old: Settings = serde_json::from_value(v).unwrap();
        assert!(!old.debrid_rd.enabled && old.debrid_rd.api_key.is_empty());
        assert!(old.debrid_rd.use_for_hosters && old.debrid_rd.use_for_magnets);
    }

    #[test]
    fn debrid_info_format_contract() {
        assert_eq!(
            format_debrid_info("Real-Debrid", "rapidgator.net", Some(true)),
            "Real-Debrid · rapidgator.net · cached"
        );
        assert_eq!(
            format_debrid_info("Real-Debrid", "rapidgator.net", None),
            "Real-Debrid · rapidgator.net · cached: unknown"
        );
        assert_eq!(
            format_debrid_info("AllDebrid", "rg.to", Some(false)),
            "AllDebrid · rg.to · not cached"
        );
        let orig = "https://rapidgator.net/file/b6df95dee983c06f41b3caa31755a9fe/x.part1.rar.html";
        assert_eq!(url_host(orig), "rapidgator.net");
        assert_eq!(
            format_debrid_info("Real-Debrid", &url_host(orig), Some(true)),
            "Real-Debrid · rapidgator.net · cached"
        );
    }

    #[test]
    fn host_up_matching() {
        let hosts = vec![
            HostStatus {
                name: "Rapidgator".into(),
                domain: "rapidgator.net".into(),
                up: true,
                supported: true,
            },
            HostStatus {
                name: "Down".into(),
                domain: "down.example".into(),
                up: false,
                supported: true,
            },
        ];
        assert_eq!(host_up(&hosts, "https://rapidgator.net/file/x"), Some(true));
        assert_eq!(
            host_up(&hosts, "https://www.rapidgator.net/file/x"),
            Some(true)
        );
        assert_eq!(host_up(&hosts, "https://down.example/x"), Some(false));
        assert_eq!(host_up(&hosts, "https://unknown.example/x"), None);
    }

    #[test]
    fn account_summary_expired_warns() {
        let a = Account {
            provider: "Real-Debrid",
            username: "bob".into(),
            email: "b@x".into(),
            plan: "premium".into(),
            is_premium: false,
            expires_at: Some(0),
        };
        assert!(a.summary().contains("NOT premium"));
        let b = Account {
            provider: "TorBox",
            username: "a@b".into(),
            email: "a@b".into(),
            plan: "Standard".into(),
            is_premium: true,
            expires_at: Some(1798761600),
        };
        assert!(b.summary().contains("2027-01-01"));
    }
}
