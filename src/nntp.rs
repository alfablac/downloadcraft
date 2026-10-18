// NNTP/NZB download backend (v1). NZB (HTTP URL or local path) -> segments
// fetched sequentially per file over one NNTP connection (plain or TLS),
// yEnc-decoded, assembled to sanitized NZB file names in dest_dir.
// ponytail: no PAR2/repair, no archive unpack, no per-segment resume (pause
// restarts current file; completed files skipped via nzbstate sidecar),
// one connection. Add when real providers require them: par2 crate +
// segment cache in .downloadcraft-{task.id} workdir.
use crate::engine::Controls;
use crate::model::{DownloadTask, Settings, TaskStatus};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};

// ---------- NZB parsing ----------

#[derive(Clone, Debug, PartialEq)]
pub struct NzbSegment {
    pub id: String,
    pub bytes: u64,
    pub number: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NzbFile {
    pub name: String,
    pub poster: String,
    pub date: i64,
    pub groups: Vec<String>,
    pub segments: Vec<NzbSegment>,
    /// Sum of NZB segment `bytes` (on-wire yEnc size, slightly > decoded size).
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Nzb {
    pub files: Vec<NzbFile>,
    pub total_bytes: u64,
}

/// Filename from NZB subject: text inside first pair of quotes wins
/// (`poster "file.rar" yEnc (1/3)`), else whole subject.
fn name_from_subject(subject: &str) -> String {
    let name = match (subject.find('"'), subject.rfind('"')) {
        (Some(a), Some(b)) if b > a => &subject[a + 1..b],
        _ => subject.trim(),
    };
    crate::model::sanitize_filename(name)
}

pub fn parse_nzb(xml: &str) -> Result<Nzb, String> {
    // Real NZBs ship a newzBin DOCTYPE; roxmltree rejects DTD by default.
    let opts = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, opts).map_err(|e| e.to_string())?;
    if doc.root_element().tag_name().name() != "nzb" {
        return Err("not an NZB document".into());
    }
    let mut files = Vec::new();
    for file in doc.descendants().filter(|n| n.has_tag_name("file")) {
        let subject = file.attribute("subject").unwrap_or("");
        let mut f = NzbFile {
            name: name_from_subject(subject),
            poster: file.attribute("poster").unwrap_or("").to_string(),
            date: file
                .attribute("date")
                .and_then(|d| d.parse().ok())
                .unwrap_or(0),
            groups: Vec::new(),
            segments: Vec::new(),
            total_bytes: 0,
        };
        for n in file.descendants() {
            if n.has_tag_name("group") {
                if let Some(t) = n.text().map(|t| t.trim()).filter(|t| !t.is_empty()) {
                    f.groups.push(t.to_string());
                }
            } else if n.has_tag_name("segment") {
                let Some(id) = n.text().map(|t| t.trim()).filter(|t| !t.is_empty()) else {
                    continue;
                };
                let number = n
                    .attribute("number")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(f.segments.len() as u32 + 1);
                let bytes = n
                    .attribute("bytes")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                f.segments.push(NzbSegment {
                    id: id.to_string(),
                    bytes,
                    number,
                });
            }
        }
        f.segments.sort_by_key(|s| s.number);
        f.total_bytes = f.segments.iter().map(|s| s.bytes).sum();
        files.push(f);
    }
    if files.is_empty() {
        return Err("NZB has no <file> entries".into());
    }
    let total_bytes = files.iter().map(|f| f.total_bytes).sum();
    Ok(Nzb { files, total_bytes })
}

// ---------- CRC32 (IEEE, table-free bitwise) ----------

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

// ---------- yEnc decoding ----------

#[derive(Clone, Debug, PartialEq)]
pub struct YencPart {
    pub name: Option<String>,
    pub part: Option<u32>,
    pub begin: Option<u64>,
    pub end: Option<u64>,
    pub data: Vec<u8>,
}

fn kv_u64(params: &str, key: &str) -> Option<u64> {
    params.split_whitespace().find_map(|p| {
        p.strip_prefix(key)
            .and_then(|v| v.strip_prefix('='))
            .and_then(|v| v.parse().ok())
    })
}

/// Decode one yEnc article body. Verifies `=yend` size and pcrc32/crc32 when
/// present. Bodies without `=ybegin` pass through raw (plain text postings).
pub fn yenc_decode(input: &[u8]) -> Result<YencPart, String> {
    // Raw bytes, NOT lossy UTF-8: encoded bytes >= 0x80 are data, UTF-8
    // replacement chars would corrupt them.
    let mut part = YencPart {
        name: None,
        part: None,
        begin: None,
        end: None,
        data: Vec::new(),
    };
    let mut in_data = false;
    let mut saw_yenc = false;
    for raw in input.split(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if let Some(rest) = line.strip_prefix(b"=ybegin".as_slice()) {
            let rest = String::from_utf8_lossy(rest);
            saw_yenc = true;
            in_data = true;
            part.part = kv_u64(&rest, "part").map(|v| v as u32);
            if let Some(i) = rest.find("name=") {
                part.name = Some(rest[i + 5..].trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix(b"=ypart".as_slice()) {
            let rest = String::from_utf8_lossy(rest);
            part.begin = kv_u64(&rest, "begin");
            part.end = kv_u64(&rest, "end");
            in_data = true;
        } else if let Some(rest) = line.strip_prefix(b"=yend".as_slice()) {
            let rest = String::from_utf8_lossy(rest);
            in_data = false;
            if let Some(want) = kv_u64(&rest, "size") {
                if want as usize != part.data.len() {
                    return Err(format!(
                        "yEnc size mismatch: got {} want {want}",
                        part.data.len()
                    ));
                }
            }
            let crc_s = rest.split_whitespace().find_map(|p| {
                p.strip_prefix("pcrc32=")
                    .or_else(|| p.strip_prefix("crc32="))
            });
            if let Some(hex) = crc_s {
                let want =
                    u32::from_str_radix(hex, 16).map_err(|_| "bad yEnc crc32".to_string())?;
                let got = crc32(&part.data);
                if got != want {
                    return Err(format!(
                        "yEnc crc32 mismatch: got {got:08x} want {want:08x}"
                    ));
                }
            }
        } else if in_data {
            let mut i = 0;
            while i < line.len() {
                let mut c = line[i];
                if c == b'=' {
                    i += 1;
                    if i >= line.len() {
                        break;
                    }
                    c = line[i].wrapping_sub(64);
                }
                part.data.push(c.wrapping_sub(42));
                i += 1;
            }
        }
    }
    if !saw_yenc {
        part.data = input.to_vec();
    }
    Ok(part)
}

// ---------- NNTP client ----------

pub struct NntpConfig {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub user: String,
    pub pass: String,
}

impl NntpConfig {
    /// Global server settings; per-task username/password override when set.
    pub fn from_settings(s: &Settings, task: &DownloadTask) -> Self {
        Self {
            host: s.nntp_host.clone(),
            port: s.nntp_port,
            tls: s.nntp_tls,
            user: if task.username.is_empty() {
                s.nntp_user.clone()
            } else {
                task.username.clone()
            },
            pass: if task.password.is_empty() {
                s.nntp_pass.clone()
            } else {
                task.password.clone()
            },
        }
    }
}

enum Wire {
    Plain(tokio::net::TcpStream),
    // Boxed: TlsStream dwarfs TcpStream (clippy large_enum_variant).
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl AsyncRead for Wire {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for Wire {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_write(cx, b),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_write(cx, b),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Wire::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Wire::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

struct Nntp {
    rd: BufReader<ReadHalf<Wire>>,
    wr: WriteHalf<Wire>,
    group: Option<String>,
}

impl Nntp {
    async fn connect(cfg: &NntpConfig) -> Result<Self, String> {
        if cfg.host.trim().is_empty() {
            return Err("NNTP server not configured (Options > Connection > NNTP)".into());
        }
        for v in [&cfg.host, &cfg.user, &cfg.pass] {
            if v.contains(['\r', '\n']) {
                return Err("invalid NNTP host/credential".into());
            }
        }
        let tcp = tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port))
            .await
            .map_err(|e| format!("NNTP connect {}:{}: {e}", cfg.host, cfg.port))?;
        let wire = if cfg.tls {
            use tokio_rustls::rustls;
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from(cfg.host.clone())
                .map_err(|_| "invalid NNTP TLS host name".to_string())?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("NNTP TLS: {e}"))?;
            Wire::Tls(Box::new(tls))
        } else {
            Wire::Plain(tcp)
        };
        let (rd, wr) = tokio::io::split(wire);
        let mut c = Self {
            rd: BufReader::new(rd),
            wr,
            group: None,
        };
        let (code, text) = c.reply().await?;
        if code != 200 && code != 201 {
            return Err(format!("NNTP greeting -> {text}"));
        }
        if !cfg.user.is_empty() {
            let (code, text) = c.cmd(&format!("AUTHINFO USER {}", cfg.user)).await?;
            if code == 381 {
                c.expect(&format!("AUTHINFO PASS {}", cfg.pass), &[281])
                    .await?;
            } else if code != 281 {
                return Err(format!("NNTP AUTHINFO USER -> {text}"));
            }
        }
        Ok(c)
    }

    /// One CRLF line. Polls in 200ms slices so cancel/pause land mid-stall.
    async fn read_line(&mut self, ctrl: &Controls) -> Result<Vec<u8>, String> {
        let mut line = Vec::new();
        loop {
            crate::engine::wait_not_paused(ctrl).await?;
            let buf = match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.rd.fill_buf(),
            )
            .await
            {
                Err(_) => continue,
                Ok(r) => r.map_err(|e| e.to_string())?,
            };
            if buf.is_empty() {
                return if line.is_empty() {
                    Err("NNTP connection closed".into())
                } else {
                    Ok(line)
                };
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let chunk = buf[..=i].to_vec();
                    line.extend_from_slice(&chunk);
                    self.rd.consume(i + 1);
                    return Ok(line);
                }
                None => {
                    let n = buf.len();
                    line.extend_from_slice(buf);
                    self.rd.consume(n);
                    if line.len() > 8 * 1024 * 1024 {
                        return Err("NNTP line too long".into());
                    }
                }
            }
        }
    }

    async fn reply(&mut self) -> Result<(u16, String), String> {
        // Greeting precedes any task Controls polling; no ctrl available here.
        let mut line = Vec::new();
        loop {
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.rd.read_until(b'\n', &mut line),
            )
            .await
            .map_err(|_| "NNTP reply timeout".to_string())?
            .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("NNTP connection closed".into());
            }
            if line.ends_with(b"\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&line).trim_end().to_string();
        let code: u16 = text.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok((code, text))
    }

    async fn cmd(&mut self, c: &str) -> Result<(u16, String), String> {
        self.wr
            .write_all(format!("{c}\r\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.wr.flush().await.map_err(|e| e.to_string())?;
        self.reply().await
    }

    async fn expect(&mut self, c: &str, ok: &[u16]) -> Result<String, String> {
        let (code, text) = self.cmd(c).await?;
        if ok.contains(&code) {
            Ok(text)
        } else {
            Err(format!("NNTP {c} -> {text}"))
        }
    }

    async fn ensure_group(&mut self, group: &str) -> Result<(), String> {
        if group.contains(['\r', '\n']) {
            return Err("invalid NNTP group".into());
        }
        if self.group.as_deref() != Some(group) {
            self.expect(&format!("GROUP {group}"), &[211]).await?;
            self.group = Some(group.to_string());
        }
        Ok(())
    }

    /// BODY <msgid>: 222 + dot-terminated body. 430 = article gone (no
    /// PAR2 in v1, so caller treats it as fatal for the file).
    async fn body(&mut self, msgid: &str, ctrl: &Controls) -> Result<Vec<u8>, String> {
        if msgid.contains(['\r', '\n']) {
            return Err("invalid NZB message-id".into());
        }
        let id = if msgid.starts_with('<') {
            msgid.to_string()
        } else {
            format!("<{msgid}>")
        };
        let (code, text) = self.cmd(&format!("BODY {id}")).await?;
        if code == 430 {
            return Err(format!(
                "NNTP article {id} missing (430); PAR2 repair not in v1"
            ));
        }
        if code != 222 {
            return Err(format!("NNTP BODY {id} -> {text}"));
        }
        let mut out = Vec::new();
        loop {
            let raw = self.read_line(ctrl).await?;
            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line == b"." {
                return Ok(out);
            }
            // Dot-unstuffing: leading ".." -> "."
            let line = line
                .strip_prefix(b".")
                .filter(|_| line.starts_with(b".."))
                .unwrap_or(line);
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
}

async fn fetch_nzb_text(client: &reqwest::Client, task: &DownloadTask) -> Result<String, String> {
    let url = task.url.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        let mut req = client.get(url);
        if !task.referrer.is_empty() {
            req = req.header("Referer", &task.referrer);
        }
        if !task.cookies.is_empty() {
            req = req.header("Cookie", &task.cookies);
        }
        for (k, v) in &task.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("NZB fetch HTTP {} for {url}", resp.status()));
        }
        resp.text().await.map_err(|e| e.to_string())
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("NZB read {path}: {e}"))
    }
}

/// Download an NZB task: fetch/parse NZB, then per file fetch segments in
/// `number` order, yEnc-decode, append to dest_dir/<sanitized NZB name>.
pub async fn download_nzb(
    client: reqwest::Client,
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
    cfg: &NntpConfig,
) -> Result<DownloadTask, String> {
    let xml = fetch_nzb_text(&client, &task).await?;
    let nzb = parse_nzb(&xml)?;
    tokio::fs::create_dir_all(&task.dest_dir)
        .await
        .map_err(|e| e.to_string())?;

    let total_segs: u64 = nzb.files.iter().map(|f| f.segments.len() as u64).sum();
    task.total = Some(nzb.total_bytes);
    ctrl.total.store(nzb.total_bytes, Ordering::Relaxed);
    // Multi-file: one Controls.parts entry per file (decoded bytes done,
    // NZB on-wire bytes total — done snaps to total on completion/skip,
    // mirroring global total/downloaded convention). Single file keeps
    // segment-count convention (parts_are_segments).
    let multi = nzb.files.len() > 1;
    ctrl.parts_are_segments.store(!multi, Ordering::Relaxed);
    *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = if multi {
        nzb.files.iter().map(|f| (0, f.total_bytes)).collect()
    } else {
        vec![(0, total_segs)]
    };
    task.status = TaskStatus::Downloading;

    let mut conn = Nntp::connect(cfg).await?;
    let mut downloaded: u64 = 0;
    let mut segs_done: u64 = 0;
    let mut failed: Vec<String> = Vec::new();
    for (file_idx, file) in nzb.files.iter().enumerate() {
        let dest = task.dest_dir.join(&file.name);
        // File-level resume: sidecar written on file completion records
        // decoded bytes. Dest with exactly that size is skipped whole;
        // mid-file partials carry no sidecar and restart from scratch.
        let state_path = task.dest_dir.join(format!("{}.nzbstate", file.name));
        let skip = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .filter(|v| {
                v.get("file_idx").and_then(|x| x.as_u64()) == Some(file_idx as u64)
                    && v.get("seg_idx").and_then(|x| x.as_u64())
                        == Some(file.segments.len() as u64)
            })
            .and_then(|v| v.get("bytes").and_then(|x| x.as_u64()))
            .filter(|&b| std::fs::metadata(&dest).map(|m| m.len()).ok() == Some(b));
        if let Some(b) = skip {
            downloaded += b;
            ctrl.downloaded.store(downloaded, Ordering::Relaxed);
            task.downloaded = downloaded;
            segs_done += file.segments.len() as u64;
            if multi {
                if let Some(p) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).get_mut(file_idx) {
                    *p = (file.total_bytes, file.total_bytes);
                }
            } else if let Some(p) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).first_mut() {
                p.0 = segs_done;
            }
            continue;
        }
        let group = file
            .groups
            .first()
            .ok_or_else(|| format!("NZB file '{}' has no <group>", file.name))?;
        let mut out = tokio::fs::File::create(&dest)
            .await
            .map_err(|e| e.to_string())?;
        let mut written: u64 = 0;
        let mut missing: u64 = 0;
        for seg in &file.segments {
            if let Err(e) = crate::engine::wait_not_paused(&ctrl).await {
                return paused(task, downloaded, e);
            }
            // One reconnect+retry on transport errors; 430/cancel stay fatal.
            let mut attempt = 0;
            let mut seg_missing = false;
            let body = loop {
                attempt += 1;
                let r = async {
                    conn.ensure_group(group).await?;
                    conn.body(&seg.id, &ctrl).await
                }
                .await;
                match r {
                    Ok(b) => break b,
                    Err(e) if attempt == 1 && !e.contains("430") && e != "cancelled" => {
                        conn = Nntp::connect(cfg).await?;
                        continue;
                    }
                    Err(e) => {
                        if e == "cancelled" {
                            drop(out);
                            let _ = tokio::fs::remove_file(&dest).await;
                            return paused(task, downloaded, e);
                        }
                        // 430 missing article: tolerate per segment, mark
                        // file failed below, continue other files.
                        if e.contains("430") {
                            seg_missing = true;
                            break Vec::new();
                        }
                        return Err(format!("NZB file '{}': {e}", file.name));
                    }
                }
            };
            if seg_missing {
                missing += 1;
                continue;
            }
            let part = yenc_decode(&body)
                .map_err(|e| format!("NZB file '{}' segment {}: {e}", file.name, seg.number))?;
            if missing == 0 {
                if let Some(begin) = part.begin {
                    if begin != written + 1 {
                        return Err(format!(
                            "NZB file '{}' segment {}: yEnc offset {begin} != {}",
                            file.name,
                            seg.number,
                            written + 1
                        ));
                    }
                }
            }
            out.write_all(&part.data).await.map_err(|e| e.to_string())?;
            written += part.data.len() as u64;
            downloaded += part.data.len() as u64;
            segs_done += 1;
            ctrl.downloaded.store(downloaded, Ordering::Relaxed);
            if multi {
                if let Some(p) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).get_mut(file_idx) {
                    p.0 = written.min(p.1);
                }
            } else if let Some(p) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).first_mut() {
                p.0 = segs_done;
            }
            task.downloaded = downloaded;
        }
        out.flush().await.map_err(|e| e.to_string())?;
        drop(out);
        if missing > 0 {
            // Incomplete file must not masquerade as output; fetched bytes discarded.
            let _ = tokio::fs::remove_file(&dest).await;
            downloaded -= written;
            ctrl.downloaded.store(downloaded, Ordering::Relaxed);
            task.downloaded = downloaded;
            if multi {
                if let Some(p) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).get_mut(file_idx) {
                    p.0 = 0;
                }
            }
            failed.push(format!(
                "{} ({missing} missing segment(s), 430)",
                file.name
            ));
            continue;
        }
        if written == 0 && !file.segments.is_empty() {
            return Err(format!("NZB file '{}' assembled empty", file.name));
        }
        if multi {
            if let Some(p) = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).get_mut(file_idx) {
                *p = (file.total_bytes, file.total_bytes);
            }
        }
        // Sidecar marks file complete for file-level resume (see top of loop).
        let _ = tokio::fs::write(
            &state_path,
            serde_json::json!({"file_idx": file_idx, "seg_idx": file.segments.len(), "bytes": written}).to_string(),
        )
        .await;
    }
    let _ = conn.cmd("QUIT").await;
    if !failed.is_empty() {
        return Err(format!(
            "NZB: {} file(s) failed: {}",
            failed.len(),
            failed.join("; ")
        ));
    }
    for file in &nzb.files {
        let _ = tokio::fs::remove_file(task.dest_dir.join(format!("{}.nzbstate", file.name))).await;
    }
    // NZB totals are on-wire sizes; report decoded bytes actually written.
    task.total = Some(downloaded);
    ctrl.total.store(downloaded, Ordering::Relaxed);
    task.downloaded = downloaded;
    task.status = TaskStatus::Complete;
    Ok(task)
}

fn paused(mut task: DownloadTask, downloaded: u64, _e: String) -> Result<DownloadTask, String> {
    task.status = TaskStatus::Paused;
    task.downloaded = downloaded;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster@example.com" date="1700000000" subject="poster &quot;file1.bin&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group><group>alt.binaries.misc</group></groups>
    <segments>
      <segment bytes="200" number="2">seg2@news</segment>
      <segment bytes="100" number="1">seg1@news</segment>
    </segments>
  </file>
  <file poster="p2" date="1700000100" subject="plain name.rar">
    <groups><group>alt.binaries.test</group></groups>
    <segments><segment bytes="50" number="1">only@news</segment></segments>
  </file>
</nzb>"#;

    #[test]
    fn parse_nzb_sorts_segments_and_totals() {
        let nzb = parse_nzb(SAMPLE).unwrap();
        assert_eq!(nzb.files.len(), 2);
        assert_eq!(nzb.files[0].name, "file1.bin");
        assert_eq!(nzb.files[0].poster, "poster@example.com");
        assert_eq!(nzb.files[0].date, 1700000000);
        assert_eq!(
            nzb.files[0].groups,
            vec!["alt.binaries.test", "alt.binaries.misc"]
        );
        assert_eq!(
            nzb.files[0]
                .segments
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["seg1@news", "seg2@news"]
        );
        assert_eq!(nzb.files[0].segments[0].number, 1);
        assert_eq!(nzb.files[0].total_bytes, 300);
        assert_eq!(nzb.files[1].name, "plain name.rar");
        assert_eq!(nzb.files[1].total_bytes, 50);
        assert_eq!(nzb.total_bytes, 350);
    }

    #[test]
    fn parse_nzb_rejects_bad_docs() {
        assert!(parse_nzb("<MPD/>").is_err());
        assert!(parse_nzb("<nzb").is_err());
        assert!(parse_nzb("<nzb/>").is_err(), "no files");
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// Minimal yEnc encoder for tests (single line body).
    fn encode(data: &[u8], name: &str) -> Vec<u8> {
        let mut out = format!(
            "=ybegin part=1 total=1 line=128 size={} name={name}\r\n",
            data.len()
        )
        .into_bytes();
        for &b in data {
            let c = b.wrapping_add(42);
            if matches!(c, 0 | 9 | 10 | 13 | 61) {
                out.push(b'=');
                out.push(c.wrapping_add(64));
            } else {
                out.push(c);
            }
        }
        out.extend_from_slice(
            format!(
                "\r\n=yend size={} part=1 pcrc32={:08x}\r\n",
                data.len(),
                crc32(data)
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn yenc_roundtrip_with_escapes_and_crc() {
        // Bytes chosen so +42 hits NUL, TAB, LF, CR, '=' critical chars.
        let data: Vec<u8> = (0u16..=255).map(|v| v as u8).collect();
        let part = yenc_decode(&encode(&data, "x.bin")).unwrap();
        assert_eq!(part.data, data);
        assert_eq!(part.name.as_deref(), Some("x.bin"));
    }

    #[test]
    fn yenc_crc_vector_and_bad_crc() {
        let part = yenc_decode(&encode(b"123456789", "v")).unwrap();
        assert_eq!(part.data, b"123456789");
        let mut bad = encode(b"123456789", "v");
        let pos = bad.windows(8).position(|w| w == b"cbf43926").unwrap();
        bad[pos] = b'0';
        assert!(yenc_decode(&bad).is_err(), "corrupt pcrc32 must fail");
    }

    #[test]
    fn yenc_plain_body_passthrough_and_parts() {
        assert_eq!(yenc_decode(b"hello plain").unwrap().data, b"hello plain");
        let mut body =
            b"=ybegin part=2 total=2 line=128 size=4 name=f.bin\r\n=ypart begin=5 end=8\r\n"
                .to_vec();
        // Encode DATA payload lines only (strip nested headers by hand).
        let enc = encode(b"DATA", "f.bin");
        let payload = &enc[enc.windows(2).position(|w| w == b"\r\n").unwrap() + 2..];
        let payload = &payload[..payload.windows(6).position(|w| w == b"\r\n=yen").unwrap()];
        body.extend_from_slice(payload);
        body.extend_from_slice(
            format!("\r\n=yend size=4 part=2 pcrc32={:08x}\r\n", crc32(b"DATA")).as_bytes(),
        );
        let part = yenc_decode(&body).unwrap();
        assert_eq!(part.data, b"DATA");
        assert_eq!(part.begin, Some(5));
        assert_eq!(part.end, Some(8));
    }
}
