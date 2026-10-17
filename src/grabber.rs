use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct GrabberProject {
    pub start_url: String,
    #[serde(default = "def_depth")] pub depth: usize,
    #[serde(default = "def_pages")] pub max_pages: usize,
    #[serde(default)] pub max_files: usize,
    #[serde(default)] pub max_total_bytes: u64,
    /// 0=same host,1=same registrable domain (see `registrable_domain`),2=external allowed
    #[serde(default)] pub scope: u8,
    #[serde(default)] pub extensions: Vec<String>,
    #[serde(default)] pub mime_allow: Vec<String>,
    #[serde(default)] pub include_regex: String,
    #[serde(default)] pub exclude_regex: String,
    #[serde(default)] pub min_size: u64,
    #[serde(default)] pub max_size: u64,
    #[serde(default = "def_true")] pub respect_robots: bool,
    #[serde(default)] pub crawl_delay_ms: u64,
    #[serde(default)] pub mirror: bool,
}
fn def_depth() -> usize { 2 } fn def_pages() -> usize { 200 } fn def_true() -> bool { true }

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FoundFile {
    pub url: String,
    pub size: Option<u64>,
    pub mime: String,
    /// Referrer: page on which this file was found (empty if unknown).
    #[serde(default)]
    pub page_url: String,
    pub selected: bool,
}

pub fn canonicalize(u: &Url) -> String {
    let mut c = u.clone(); c.set_fragment(None);
    // strip default ports handled by Url; lowercase host via Url itself
    c.to_string()
}

fn host_of(u: &Url) -> String { u.host_str().unwrap_or("").to_ascii_lowercase() }
/// Common two-label public suffixes (ponytail: tiny heuristic list, not full PSL;
/// upgrade path: `publicsuffix` crate if coverage matters).
const MULTI_LABEL_SUFFIXES: &[&str] = &[
    "ac.uk", "co.uk", "gov.uk", "org.uk", "com.au", "net.au", "org.au", "co.nz", "com.br",
    "com.cn", "com.tw", "com.hk", "co.jp", "ne.jp", "or.jp", "co.kr", "com.mx", "com.ar",
    "co.in", "com.sg",
];

/// Registrable domain: last 2 labels, or last 3 under a known multi-label suffix
/// (`example.co.uk`, not `co.uk`).
pub fn registrable_domain(host: &str) -> String {
    let labels: Vec<&str> = host.trim_end_matches('.').split('.').filter(|s| !s.is_empty()).collect();
    if labels.len() <= 2 { return labels.join("."); }
    let last_two = labels[labels.len() - 2..].join(".").to_lowercase();
    let take = if MULTI_LABEL_SUFFIXES.contains(&last_two.as_str()) { 3 } else { 2 };
    labels[labels.len() - take..].join(".")
}

fn domain_of(u: &Url) -> String { registrable_domain(&host_of(u)) }

pub fn in_scope(start: &Url, cand: &Url, scope: u8) -> bool {
    match scope { 0 => host_of(start) == host_of(cand), 1 => domain_of(start) == domain_of(cand), _ => true }
}

fn ext_of(url: &str) -> String { url.split(['?', '#']).next().unwrap_or(url).rsplit('.').next().unwrap_or("").to_ascii_lowercase() }

/// First `<base href>` wins per HTML spec; resolved against page URL. Relative links then resolve against it, not the page.
pub fn document_base(html: &str, page: &Url) -> Url {
    let lower = html.to_ascii_lowercase();
    if let Some(idx) = lower.find("<base") {
        let slice = &lower[idx..(idx + 600).min(lower.len())];
        if let Some(ai) = slice.find("href=") {
            let rest = html[idx + ai + 5..].trim_start();
            let val = if let Some(s) = rest.strip_prefix('"') { s.split('"').next().unwrap_or("") } else if let Some(s) = rest.strip_prefix('\'') { s.split('\'').next().unwrap_or("") } else { rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("") };
            if !val.is_empty() { if let Ok(u) = page.join(val) { return u; } }
        }
    }
    page.clone()
}

pub fn extract_links(html: &str, base: &Url) -> (Vec<Url>, Vec<Url>) {
    // returns (page_links, file_links) - heuristic: assets from src/href with extension treated as files via caller anyway; here split <a href> vs asset tags
    let base = &document_base(html, base);
    let mut pages = Vec::new(); let mut files = Vec::new();
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes(); let mut i = 0;
    // simple tag scanner
    for (tag, attr) in [("a", "href"), ("link", "href"), ("img", "src"), ("script", "src"), ("video", "src"), ("audio", "src"), ("source", "src")] {
        let needle = format!("<{tag}");
        let mut pos = 0;
        while let Some(idx) = lower[pos..].find(&needle).map(|x| x + pos) {
            // find attr within next 500 chars
            let slice = &lower[idx..(idx + 600).min(lower.len())];
            if let Some(ai) = slice.find(&format!("{attr}=")) {
                let rest = &html[idx + ai + attr.len() + 1..];
                let rest_trim = rest.trim_start();
                let val = if let Some(stripped) = rest_trim.strip_prefix('"') { stripped.split('"').next().unwrap_or("") } else if let Some(stripped) = rest_trim.strip_prefix('\'') { stripped.split('\'').next().unwrap_or("") } else { rest_trim.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("") };
                if !val.is_empty() && !val.starts_with('#') && !val.starts_with("javascript:") && !val.starts_with("mailto:") && !val.starts_with("data:") {
                    if let Ok(u) = base.join(val) { if u.scheme() == "http" || u.scheme() == "https" { if tag == "a" { pages.push(u); } else { files.push(u); } } }
                }
            }
            pos = idx + needle.len();
        }
        let _ = (bytes, &mut i);
    }
    (pages, files)
}

/// Exact wildcard match: `*` matches any sequence (incl. empty), all else literal.
fn robots_wildcard(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() { if p[i - 1] == '*' { dp[i][0] = dp[i - 1][0]; } }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                c => c == t[j - 1] && dp[i - 1][j - 1],
            };
        }
    }
    dp[p.len()][t.len()]
}

fn robots_pattern_matches(pat: &str, path: &str) -> bool {
    // `$` at end anchors to end of path; otherwise pattern is a prefix pattern.
    if let Some(anchored) = pat.strip_suffix('$') {
        robots_wildcard(anchored, path)
    } else {
        robots_wildcard(&format!("{pat}*"), path)
    }
}

/// Robots.txt check for `User-agent: *` group only. `Allow`/`Disallow` both
/// supported; longest matching pattern wins, `Allow` wins ties. Patterns may
/// contain `*` wildcards and a trailing `$` end anchor.
pub fn robots_allows(robots_txt: &str, path: &str) -> bool {
    let mut in_star = false;
    let mut best: Option<(usize, bool)> = None; // (pattern len, allowed)
    for line in robots_txt.lines() {
        let l = line.trim();
        let lower = l.to_ascii_lowercase();
        if lower.starts_with("user-agent:") {
            in_star = l[11..].trim() == "*";
        } else if in_star && (lower.starts_with("disallow:") || lower.starts_with("allow:")) {
            let is_allow = lower.starts_with("allow:");
            let p = l[if is_allow { 6 } else { 9 }..].trim();
            if p.is_empty() { continue; }
            if robots_pattern_matches(p, path) {
                let better = match best {
                    None => true,
                    Some((len, allowed)) => p.len() > len || (p.len() == len && is_allow && !allowed),
                };
                if better { best = Some((p.len(), is_allow)); }
            }
        }
    }
    best.map(|(_, allowed)| allowed).unwrap_or(true)
}

pub struct Crawler { pub project: GrabberProject }

impl Crawler {
    pub async fn crawl(&self, client: &reqwest::Client) -> Result<(Vec<String>, Vec<FoundFile>), String> {
        let start = Url::parse(&self.project.start_url).map_err(|e| e.to_string())?;
        let mut robots = String::new();
        if self.project.respect_robots {
            let robots_url = start.join("/robots.txt").map_err(|e| e.to_string())?;
            if let Ok(r) = client.get(robots_url).send().await { if r.status().is_success() { robots = r.text().await.unwrap_or_default(); } }
        }
        let include_re = compile_opt(&self.project.include_regex);
        let exclude_re = simple_re(&self.project.exclude_regex);
        let _ = include_re;
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(Url, usize)> = VecDeque::new();
        queue.push_back((start.clone(), 0));
        visited.insert(canonicalize(&start));
        let mut pages: Vec<String> = Vec::new();
        let mut files: Vec<FoundFile> = Vec::new();
        let mut seen_files: BTreeSet<String> = BTreeSet::new();
        let mut total_bytes: u64 = 0;
        while let Some((url, depth)) = queue.pop_front() {
            if pages.len() >= self.project.max_pages { break; }
            if self.project.respect_robots && !robots_allows(&robots, url.path()) { continue; }
            if self.project.crawl_delay_ms > 0 { tokio::time::sleep(std::time::Duration::from_millis(self.project.crawl_delay_ms)).await; }
            let resp = match client.get(url.clone()).send().await { Ok(r) => r, Err(_) => continue };
            let ct = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("text/html").to_string();
            let is_html = ct.contains("html");
            pages.push(canonicalize(&url));
            if !is_html { continue; }
            let html = resp.text().await.unwrap_or_default();
            // page text exclude/include via substring check on html if regex is plain word (regex compiled via simple contains fallback)
            let (links, assets) = links_with_doc_base(&html, &url);
            let mut candidates: Vec<Url> = Vec::new();
            candidates.extend(links.iter().cloned()); candidates.extend(assets.iter().cloned());
            for cand in candidates {
                let canon = canonicalize(&cand);
                let ext = ext_of(&canon);
                let is_page_like = ext.is_empty() || ["html", "htm", "php", "asp", "aspx"].contains(&ext.as_str()) || cand.path().ends_with('/');
                // filters
                if !self.project.extensions.is_empty() && !is_page_like && !self.project.extensions.iter().any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext)) { /* still may be page */ if !is_page_like { continue; } }
                if let Some(re) = &exclude_re { if re.is_match(&canon) { continue; } }
                if !self.project.include_regex.is_empty() { if let Some(re) = &include_re { if !re.is_match(&canon) { /* include applies to files; pages still crawl */ if !is_page_like { continue; } } } }
                if is_page_like {
                    if depth < self.project.depth && in_scope(&start, &cand, self.project.scope) && !visited.contains(&canon) {
                        visited.insert(canon); queue.push_back((cand, depth + 1));
                    }
                } else {
                    if !in_scope(&start, &cand, self.project.scope) { continue; }
                    if seen_files.contains(&canon) { continue; }
                    // HEAD for size/mime where possible (GET fallback in tests uses HEAD support)
                    let (size, mime) = head_info(client, &cand).await;
                    // Unknown size cannot satisfy a positive minimum: skip.
                    if self.project.min_size > 0 { match size { Some(s) if s >= self.project.min_size => {}, _ => continue } }
                    if self.project.max_size > 0 { if let Some(s) = size { if s > self.project.max_size { continue; } } }
                    if !self.project.mime_allow.is_empty() && !mime.is_empty() && !self.project.mime_allow.iter().any(|m| mime.contains(m)) { continue; }
                    if self.project.max_files > 0 && files.len() >= self.project.max_files { continue; }
                    if let Some(s) = size { total_bytes += s; if self.project.max_total_bytes > 0 && total_bytes > self.project.max_total_bytes { continue; } }
                    seen_files.insert(canon.clone());
                    files.push(FoundFile { url: canon, size, mime, page_url: url.to_string(), selected: true });
                }
            }
        }
        Ok((pages, files))
    }
}

async fn head_info(client: &reqwest::Client, url: &Url) -> (Option<u64>, String) {
    match client.head(url.clone()).send().await {
        Ok(r) => {
            let size = r.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
            let mime = r.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            (size, mime)
        }
        Err(_) => (None, String::new()),
    }
}

// Tiny regex via `regex` not in deps: use substring-or-pattern matching implemented as literal contains OR basic wildcard.
// ponytail: full regex via std only is unavailable; treat pattern as literal substring if it has no regex meta, else use simple wildcard matching.
pub struct SimpleRe(String);
impl SimpleRe { fn is_match(&self, s: &str) -> bool { wildcard_match(&self.0, s) || s.contains(&self.0) } }
fn simple_re(p: &str) -> Option<SimpleRe> { if p.is_empty() { None } else { Some(SimpleRe(p.to_string())) } }
fn compile_opt(p: &str) -> Option<SimpleRe> { simple_re(p) }
pub fn wildcard_match(pat: &str, text: &str) -> bool {
    // supports * and ? and substring for plain patterns; also strips ^ $ anchors common in filters
    let pat = pat.trim_start_matches('^').trim_end_matches('$');
    if !pat.contains('*') && !pat.contains('?') && !pat.contains('.') { return text.contains(pat); }
    // convert escaped dot
    let pat = pat.replace("\\.", "\u{1}");
    let p: Vec<char> = pat.chars().collect(); let t: Vec<char> = text.chars().collect();
    // classic DP wildcard where '.' in pattern without escape means any char? Keep '.' literal except \. case: treat '.' as any single char only if pattern looks regex-ish (contains .* )
    let regexish = pat.contains(".*");
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() { if p[i - 1] == '*' { dp[i][0] = dp[i - 1][0]; } }
    for i in 1..=p.len() { for j in 1..=t.len() {
        dp[i][j] = match p[i - 1] {
            '*' => dp[i - 1][j] || dp[i][j - 1],
            '?' => dp[i - 1][j - 1],
            '\u{1}' => t[j - 1] == '.' && dp[i - 1][j - 1],
            '.' if regexish => dp[i - 1][j - 1],
            c => c == t[j - 1] && dp[i - 1][j - 1],
        };
    } }
    // substring semantics for exclude filters
    dp[p.len()][t.len()] || text.contains(&pat.replace('\u{1}', "."))
}

/// Local path for mirror: preserve folder structure.
pub fn mirror_rel_path(_root: &Url, u: &Url) -> String {
    let host = host_of(u);
    let mut p = format!("{host}{}", u.path());
    if p.ends_with('/') { p.push_str("index.html"); }
    else if !p.contains('.') { p.push_str("/index.html"); }
    // Query variants of same path collide on disk: tag filename with short query hash.
    if let Some(q) = u.query().filter(|q| !q.is_empty()) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        q.hash(&mut h);
        let hex = format!("{:016x}", h.finish());
        let seg_start = p.rfind('/').map(|i| i + 1).unwrap_or(0);
        if let Some(d) = p[seg_start..].rfind('.') {
            p.insert_str(seg_start + d, &format!("-{}", &hex[..8]));
        } else {
            p.push_str(&format!("-{}", &hex[..8]));
        }
    }
    // sanitize per segment
    p.split('/').map(crate::model::sanitize_filename).collect::<Vec<_>>().join("/")
}

/// Mirror: crawl then download pages+files preserving structure, rewrite absolute URLs to relative local paths.
pub async fn mirror_site(client: &reqwest::Client, project: &GrabberProject, out_dir: &std::path::Path) -> Result<Vec<String>, String> {
    let crawler = Crawler { project: project.clone() };
    let (pages, files) = crawler.crawl(client).await?;
    let start = Url::parse(&project.start_url).map_err(|e| e.to_string())?;
    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut downloaded: Vec<String> = Vec::new();
    let mut jobs: Vec<String> = pages.clone();
    for f in &files { jobs.push(f.url.clone()); }
    for u in &jobs {
        let Ok(parsed) = Url::parse(u) else { continue };
        let rel = mirror_rel_path(&start, &parsed);
        mapping.insert(u.clone(), rel.clone());
    }
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    for u in &jobs {
        let Ok(parsed) = Url::parse(u) else { continue };
        let rel = mirror_rel_path(&start, &parsed);
        let dest = out_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(p) = dest.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
        let resp = client.get(parsed.clone()).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() { continue; }
        let ct = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if ct.contains("html") {
            let html = String::from_utf8_lossy(&bytes).into_owned();
            // mapping keys are absolute urls; rewrite to paths relative to mirror root, then browser resolves from page location if we also write root-relative handling via <base> injection.
            let mut out = html.clone();
            for (url, r) in &mapping { out = out.replace(url, &format!("/{r}")); }
            // ponytail: root-absolute /path links work when served/opened from mirror root via file:// only if mirrored at filesystem root; inject base pointing to mirror root file URL.
            let base_url = format!("file://{}/", out_dir.display().to_string().replace('\\', "/"));
            out = out.replace("<head>", &format!("<head><base href=\"{base_url}\">"));
            std::fs::write(&dest, out).map_err(|e| e.to_string())?;
        } else {
            std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
        }
        downloaded.push(dest.display().to_string());
    }
    Ok(downloaded)
}

/// Resolve links against the document's `<base href>` when present (falls back to page URL), matching browser semantics.
pub fn links_with_doc_base(html: &str, page_url: &Url) -> (Vec<Url>, Vec<Url>) { extract_links(html, &document_base(html, page_url)) }

pub fn rewrite_links(html: &str, page_url: &Url, mapping: &HashMap<String, String>, mirror_root: &std::path::Path, page_rel: &str) -> String {
    // Source <base href> must not survive: mirrored output carries its own base; relative hrefs were resolved against source base for mapping.
    let lower0 = html.to_ascii_lowercase();
    let html_owned;
    let html = if let Some(i) = lower0.find("<base") { let mut s = html.to_string(); if let Some(j) = s[i..].find('>') { s.replace_range(i..=i + j, ""); } html_owned = s; &html_owned } else { html };
    let _ = (mirror_root, page_rel);
    // Resolve href/src values (relative, root-relative, absolute) against the
    // page URL first, then swap in the mapped mirror path.
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    'scan: while !rest.is_empty() {
        let mut hit: Option<(usize, usize, char)> = None; // (value start, prefix len, quote)
        for attr in ["href", "src"] {
            for quote in ['"', '\''] {
                let needle = format!("{attr}={quote}");
                if let Some(idx) = rest.find(&needle) {
                    let start = idx + needle.len();
                    if hit.is_none_or(|(s, _, _)| start < s) { hit = Some((start, needle.len(), quote)); }
                }
            }
        }
        let Some((vstart, _, quote)) = hit else { break 'scan };
        let Some(vend) = rest[vstart..].find(quote).map(|i| i + vstart) else { break 'scan };
        let val = &rest[vstart..vend];
        let skip = val.is_empty() || val.starts_with(['#', '?']) || ["javascript:", "mailto:", "data:", "tel:"].iter().any(|s| val.to_ascii_lowercase().starts_with(s));
        let replacement = if skip {
            None
        } else {
            // Browser resolves href/src against <base href> if present, else page URL.
            document_base(html, page_url).join(val).ok().and_then(|resolved| {
                mapping.get(&resolved.to_string()).or_else(|| mapping.get(&canonicalize(&resolved))).cloned()
            })
        };
        match replacement {
            Some(rel) => {
                out.push_str(&rest[..vstart]);
                out.push_str(&rel);
                rest = &rest[vend..];
            }
            None => {
                out.push_str(&rest[..vend]);
                rest = &rest[vend..];
            }
        }
    }
    out.push_str(rest);
    // Absolute URLs outside attributes (plain text, meta content, etc.).
    for (url, rel) in mapping {
        out = out.replace(url, rel);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn document_base_href_resolution() {
        let page = Url::parse("https://ex.com/dir/p.html").unwrap();
        let html = r#"<head><base href="https://cdn.ex.com/static/"></head><img src="a.png"><a href="b.html">b</a>"#;
        let (pages, files) = links_with_doc_base(html, &page);
        assert!(files.iter().any(|u| u.as_str() == "https://cdn.ex.com/static/a.png"));
        assert!(pages.iter().any(|u| u.as_str() == "https://cdn.ex.com/static/b.html"));
        let out = rewrite_links(r#"<head><base href="/root/"></head><a href="z.html">z</a>"#, &page, &HashMap::new(), std::path::Path::new("/tmp"), "index.html");
        assert!(!out.to_ascii_lowercase().contains("<base"), "{out}");
    }

    #[test] fn extract_and_scope() {
        let base = Url::parse("https://example.com/a/index.html").unwrap();
        let (pages, files) = extract_links(r#"<a href="/b/">b</a><img src="pic.png"><a href="https://other.com/x.zip">x</a>"#, &base);
        assert!(pages.iter().any(|u| u.path() == "/b/"));
        assert!(files.iter().any(|u| u.path().ends_with("pic.png")));
        assert!(!in_scope(&base, &Url::parse("https://other.com/").unwrap(), 0));
        assert!(!in_scope(&base, &Url::parse("https://sub.example.com/").unwrap(), 0));
        assert!(in_scope(&base, &Url::parse("https://sub.example.com/").unwrap(), 1));
    }
    #[test] fn robots() { assert!(!robots_allows("User-agent: *\nDisallow: /private/", "/private/x")); assert!(robots_allows("User-agent: *\nDisallow: /private/", "/public")); }
    #[test] fn robots_allow_and_wildcards() {
        let txt = "User-agent: *\nDisallow: /private/\nAllow: /private/public.html\nDisallow: /*.pdf$\nDisallow: /tmp/*\n";
        assert!(robots_allows(txt, "/private/public.html")); // longest match wins
        assert!(!robots_allows(txt, "/private/other"));
        assert!(!robots_allows(txt, "/a/b.pdf")); // * + $ anchor
        assert!(robots_allows(txt, "/a/b.pdfx")); // $ prevents suffix match
        assert!(!robots_allows(txt, "/tmp/a/b"));
        assert!(robots_allows("User-agent: bot\nDisallow: /\n", "/anything")); // only * group applies
        assert_eq!(registrable_domain("www.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("a.b.example.com"), "example.com");
        assert_eq!(registrable_domain("sub.shop.com.au"), "shop.com.au");
        let base = Url::parse("https://example.co.uk/").unwrap();
        assert!(in_scope(&base, &Url::parse("https://cdn.example.co.uk/").unwrap(), 1));
        assert!(!in_scope(&base, &Url::parse("https://other.co.uk/").unwrap(), 1));
        assert!(FoundFile::default().page_url.is_empty());
    }
    #[test] fn mirror_query_and_relative_rewrite() {
        let root = Url::parse("https://example.com/").unwrap();
        let plain = mirror_rel_path(&root, &Url::parse("https://example.com/a/page.html").unwrap());
        let q1 = mirror_rel_path(&root, &Url::parse("https://example.com/a/page.html?x=1").unwrap());
        let q2 = mirror_rel_path(&root, &Url::parse("https://example.com/a/page.html?x=2").unwrap());
        assert_eq!(plain, "example.com/a/page.html");
        assert_ne!(q1, plain); assert_ne!(q1, q2);
        assert!(q1.starts_with("example.com/a/page-") && q1.ends_with(".html"), "{q1}");
        assert_eq!(q1, mirror_rel_path(&root, &Url::parse("https://example.com/a/page.html?x=1").unwrap())); // deterministic
        let page = Url::parse("https://example.com/a/c/index.html").unwrap();
        let mut map = HashMap::new();
        map.insert("https://example.com/a/b/page.html".to_string(), "a/b/page.html".to_string());
        let out = rewrite_links(r#"<a href="../b/page.html">r</a><a href="/a/b/page.html">abs</a><a href="https://example.com/a/b/page.html">full</a>"#, &page, &map, std::path::Path::new("/tmp"), "a/c/index.html");
        assert_eq!(out.matches("a/b/page.html").count(), 3, "{out}");
    }
    #[test] fn canon_dedupe() { assert_eq!(canonicalize(&Url::parse("https://EXAMPLE.com/a#frag").unwrap()), canonicalize(&Url::parse("https://example.com/a").unwrap())); }
}
