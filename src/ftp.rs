// Minimal FTP/FTPS client (PASV/REST binary) over tokio TCP + rustls.
// Schemes: ftp:// plain (default port 21); ftps:// explicit FTPS - plain
// connect, then AUTH TLS, PBSZ 0, PROT P (default port 21, NOT 990);
// ftpsi:// implicit FTPS - TLS on connect (default port 990). Data
// connections use TLS whenever control does (PROT P). Certs verified
// against webpki roots (no accept-invalid), server name = URL host.
use crate::engine::Controls;
use crate::model::{DownloadTask, TaskStatus};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader,
    ReadHalf, WriteHalf,
};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsMode {
    Plain,
    Explicit,
    Implicit,
}

/// Scheme -> (TLS mode, default port). Explicit FTPS defaults 21, not 990.
fn tls_mode_and_port(url: &url::Url) -> Result<(TlsMode, u16), String> {
    let (mode, default) = match url.scheme() {
        "ftp" => (TlsMode::Plain, 21),
        "ftps" => (TlsMode::Explicit, 21),
        "ftpsi" => (TlsMode::Implicit, 990),
        s => return Err(format!("unsupported FTP scheme: {s}")),
    };
    Ok((mode, url.port().unwrap_or(default)))
}

enum Wire {
    Plain(TcpStream),
    // Boxed: TlsStream dwarfs TcpStream (clippy large_enum_variant).
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
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

// rustls config mirrors src/nntp.rs: webpki roots, no client auth.
async fn wrap_tls(
    tcp: TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| "invalid FTPS TLS host name".to_string())?;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp),
    )
    .await
    .map_err(|_| "FTPS TLS handshake timeout".to_string())?
    .map_err(|e| format!("FTPS TLS: {e}"))
}

struct Ftp {
    rd: BufReader<ReadHalf<Wire>>,
    wr: WriteHalf<Wire>,
    local_ip: IpAddr,
}

impl Ftp {
    /// AUTH TLS upgrade (consuming: halves unsplit for handshake, reader
    /// buffer dropped - server sends nothing between 234 and handshake).
    async fn upgrade_tls(self, host: &str) -> Result<Self, String> {
        let plain = match self.rd.into_inner().unsplit(self.wr) {
            Wire::Plain(tcp) => tcp,
            Wire::Tls(_) => return Err("FTP control already TLS".into()),
        };
        let (rd, wr) = tokio::io::split(Wire::Tls(Box::new(wrap_tls(plain, host).await?)));
        Ok(Self {
            rd: BufReader::new(rd),
            wr,
            local_ip: self.local_ip,
        })
    }

    async fn reply(&mut self, stage: &str) -> Result<(u16, String), String> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.rd.read_line(&mut line),
            )
            .await
            .map_err(|_| format!("FTP {stage}: reply timeout"))?
            .map_err(|e| format!("FTP {stage}: {e}"))?;
            if n == 0 {
                return Err(format!("FTP {stage}: connection closed"));
            }
            let code: u16 = line.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);
            text.push_str(line.trim_end());
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text));
            }
            text.push(' ');
        }
    }
    async fn cmd(&mut self, stage: &str, wire: &str) -> Result<(u16, String), String> {
        self.wr
            .write_all(format!("{wire}\r\n").as_bytes())
            .await
            .map_err(|e| format!("FTP {stage}: {e}"))?;
        self.wr
            .flush()
            .await
            .map_err(|e| format!("FTP {stage}: {e}"))?;
        self.reply(stage).await
    }
    async fn cmd_to(
        &mut self,
        stage: &str,
        wire: &str,
        secs: u64,
    ) -> Result<(u16, String), String> {
        // ponytail: short probe timeout so broken EPSV/PASV costs seconds, not 30s each.
        self.wr
            .write_all(format!("{wire}\r\n").as_bytes())
            .await
            .map_err(|e| format!("FTP {stage}: {e}"))?;
        self.wr
            .flush()
            .await
            .map_err(|e| format!("FTP {stage}: {e}"))?;
        match tokio::time::timeout(Duration::from_secs(secs), self.reply(stage)).await {
            Ok(r) => r,
            Err(_) => Err(format!("FTP {stage}: reply timeout")),
        }
    }
    /// Errors carry `stage` only, never wire text: PASS wire contains password.
    async fn expect(&mut self, stage: &str, wire: &str, ok: &[u16]) -> Result<String, String> {
        let (code, text) = self.cmd(stage, wire).await?;
        if ok.contains(&code) {
            Ok(text)
        } else {
            Err(format!("FTP {stage}: server replied {code} {text}"))
        }
    }
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

async fn wait_not_paused(ctrl: &Controls) -> bool {
    while ctrl.paused.load(Ordering::Relaxed) {
        if ctrl.cancelled.load(Ordering::Relaxed) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    ctrl.cancelled.load(Ordering::Relaxed)
}

async fn connect_timeout(addr: (&str, u16)) -> Result<TcpStream, String> {
    match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(format!("FTP connect {}:{}: {e}", addr.0, addr.1)),
        Err(_) => Err(format!("FTP connect {}:{}: timeout", addr.0, addr.1)),
    }
}

fn parse_passive(reply: &str) -> Option<Ipv4Addr> {
    reply.chars().find(|c| c.is_ascii_digit()).map(|_| ())?;
    // last 6 numeric groups in reply: h1..h4,p1,p2
    let nums: Vec<u8> = reply
        .split(|c: char| !(c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 6 {
        return None;
    }
    let n = &nums[nums.len() - 6..];
    Some(Ipv4Addr::new(n[0], n[1], n[2], n[3]))
}
fn parse_pasv_port(reply: &str) -> Option<u16> {
    let nums: Vec<u16> = reply
        .split(|c: char| !(c.is_ascii_digit()))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 6 {
        return None;
    }
    let n = &nums[nums.len() - 2..];
    Some(n[0] * 256 + n[1])
}
fn parse_epsv_port(reply: &str) -> Option<u16> {
    // 229 (|||port|)
    let inner = reply
        .split('(')
        .nth(1)?
        .split(')')
        .next()?
        .trim()
        .to_string();
    inner
        .split('|')
        .filter(|s| !s.is_empty())
        .next_back()?
        .parse()
        .ok()
}
fn is_private_or_unspec(ip: Ipv4Addr) -> bool {
    // std is_private covers 10/8, 172.16/12, 192.168/16.
    ip.is_private() || ip.is_loopback() || ip.is_unspecified()
}

/// PASV target host: announced private/NAT IP differing from control host is a
/// NAT lie - control host reachable by definition, announced LAN IP usually not.
fn pasv_host(announced: Ipv4Addr, control: &str) -> String {
    if is_private_or_unspec(announced) {
        if let Ok(IpAddr::V4(c)) = control.parse::<IpAddr>() {
            if c != announced {
                return control.to_string();
            }
        } else if !control.is_empty() {
            // hostname control: announced private != resolver result assumed; use name.
            return control.to_string();
        }
    }
    announced.to_string()
}

/// Data stream: plain TcpStream or TLS-wrapped; accept-side for PORT.
enum DataChan {
    Ready(Box<dyn AsyncRead + Send + Unpin>),
    /// Server will connect to listener after RETR.
    Accept(TcpListener),
}

async fn wrap_data(
    tcp: TcpStream,
    mode: TlsMode,
    host: &str,
) -> Result<Box<dyn AsyncRead + Send + Unpin>, String> {
    // ponytail: FTPS data stays TLS even in active mode (PROT P already negotiated; plain active FTPS would leak + most servers reject).
    match mode {
        TlsMode::Plain => Ok(Box::new(tcp) as Box<dyn AsyncRead + Send + Unpin>),
        _ => Ok(Box::new(wrap_tls(tcp, host).await?) as Box<dyn AsyncRead + Send + Unpin>),
    }
}

/// Try EPSV then PASV then PORT. EPSV first: RFC 2428, host implicit (no NAT IP
/// lie), most modern servers; cost one short probe on legacy servers.
async fn open_data(ftp: &mut Ftp, host: &str, mode: TlsMode) -> Result<DataChan, String> {
    let mut errs = Vec::new();
    // EPSV
    match ftp.cmd_to("EPSV", "EPSV", 10).await {
        Ok((229, text)) => {
            if let Some(p) = parse_epsv_port(&text) {
                match connect_timeout((host, p)).await {
                    Ok(tcp) => {
                        return Ok(DataChan::Ready(wrap_data(tcp, mode, host).await?));
                    }
                    Err(e) => errs.push(e),
                }
            } else {
                errs.push("bad EPSV reply".into());
            }
        }
        Ok((code, t)) => errs.push(format!("EPSV {code} {t}")),
        Err(e) => errs.push(e),
    }
    // PASV
    match ftp.cmd_to("PASV", "PASV", 10).await {
        Ok((227, text)) => {
            if let (Some(ip), Some(p)) = (parse_passive(&text), parse_pasv_port(&text)) {
                let host_str = pasv_host(ip, host);
                match connect_timeout((host_str.as_str(), p)).await {
                    Ok(tcp) => {
                        return Ok(DataChan::Ready(wrap_data(tcp, mode, host).await?));
                    }
                    Err(e) => errs.push(e),
                }
            } else {
                errs.push(format!("bad PASV reply: {text}"));
            }
        }
        Ok((code, t)) => errs.push(format!("PASV {code} {t}")),
        Err(e) => errs.push(e),
    }
    // PORT (active): listener on control local IP; server connects back after RETR.
    if mode != TlsMode::Plain {
        // ponytail: FTPS active needs server as TLS client (some servers expect client role / reject); refuse loudly vs silently downgrade to plain data.
        return Err(format!(
            "FTP data: {}; PORT: active mode unsupported for FTPS (passive-only)",
            errs.join("; ")
        ));
    }
    let v4 = match ftp.local_ip {
        IpAddr::V4(v) => v,
        IpAddr::V6(_) => {
            return Err(format!(
                "FTP data: {}; PORT: active mode needs IPv4 control connection",
                errs.join("; ")
            ))
        }
    };
    let listener = TcpListener::bind((v4, 0))
        .await
        .map_err(|e| format!("FTP PORT bind: {e}"))?;
    let lp = listener.local_addr().map_err(|e| e.to_string())?.port();
    let o = v4.octets();
    let wire = format!(
        "PORT {},{},{},{},{},{}",
        o[0],
        o[1],
        o[2],
        o[3],
        lp / 256,
        lp % 256
    );
    match ftp.cmd("PORT", &wire).await {
        Ok((200, _)) => Ok(DataChan::Accept(listener)),
        Ok((code, t)) => Err(format!("FTP data: {}; PORT {code} {t}", errs.join("; "))),
        Err(e) => Err(format!("FTP data: {}; PORT: {e}", errs.join("; "))),
    }
}

async fn accept_data(
    chan: DataChan,
    mode: TlsMode,
    host: &str,
) -> Result<Box<dyn AsyncRead + Send + Unpin>, String> {
    match chan {
        DataChan::Ready(r) => Ok(r),
        DataChan::Accept(listener) => {
            match tokio::time::timeout(Duration::from_secs(15), listener.accept()).await {
                Ok(Ok((tcp, _))) => wrap_data(tcp, mode, host).await,
                Ok(Err(e)) => Err(format!("FTP PORT accept: {e}")),
                Err(_) => Err("FTP PORT accept: timeout (server did not connect back)".into()),
            }
        }
    }
}

struct Login {
    host: String,
    port: u16,
    mode: TlsMode,
    user: String,
    pass: String,
    path: String,
}

/// One control connection: greeting, FTPS upgrade, login, TYPE I. SIZE left to caller.
async fn login(lg: &Login) -> Result<Ftp, String> {
    let stream = connect_timeout((lg.host.as_str(), lg.port)).await?;
    let local_ip = stream.local_addr().map_err(|e| e.to_string())?.ip();
    let wire = match lg.mode {
        TlsMode::Implicit => Wire::Tls(Box::new(wrap_tls(stream, &lg.host).await?)),
        _ => Wire::Plain(stream),
    };
    let (rd, wr) = tokio::io::split(wire);
    let mut ftp = Ftp {
        rd: BufReader::new(rd),
        wr,
        local_ip,
    };
    let (code, text) = ftp.reply("greeting").await?;
    if code != 220 {
        return Err(format!("FTP greeting -> {text}"));
    }
    if lg.mode == TlsMode::Explicit {
        ftp.expect("AUTH TLS", "AUTH TLS", &[234]).await?;
        ftp = ftp.upgrade_tls(&lg.host).await?;
    }
    if lg.mode != TlsMode::Plain {
        ftp.expect("PBSZ", "PBSZ 0", &[200]).await?;
        ftp.expect("PROT", "PROT P", &[200]).await?;
    }
    match ftp.cmd("USER", &format!("USER {}", lg.user)).await? {
        (230, _) => {}
        (331, _) => {
            ftp.expect("PASS", &format!("PASS {}", lg.pass), &[230])
                .await
                .map_err(|e| format!("FTP auth failed for user '{}': {e}", lg.user))?;
        }
        (code, text) => {
            return Err(format!(
                "FTP auth failed for user '{}': USER server replied {code} {text}",
                lg.user
            ));
        }
    }
    ftp.expect("TYPE I", "TYPE I", &[200]).await?;
    Ok(ftp)
}

#[allow(clippy::too_many_arguments)] // ponytail: flat args beat a param struct for one caller pair
async fn retr_exact(
    ftp: &mut Ftp,
    lg: &Login,
    rest_off: Option<u64>,
    want: Option<u64>,
    sink: &mut (dyn AsyncWrite + Send + Unpin),
    ctrl: &Arc<Controls>,
    part_idx: Option<usize>,
    downloaded: &mut u64,
) -> Result<(), String> {
    if let Some(off) = rest_off {
        if off > 0 {
            let (code, _) = ftp.cmd("REST", &format!("REST {off}")).await?;
            if code != 350 {
                return Err(format!("FTP REST: server replied {code}"));
            }
        }
    }
    let chan = open_data(ftp, &lg.host, lg.mode).await?;
    ftp.expect("RETR", &format!("RETR {}", lg.path), &[125, 150])
        .await?;
    let mut data = accept_data(chan, lg.mode, &lg.host).await?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;
    loop {
        if let Some(w) = want {
            if got >= w {
                break;
            }
        }
        if wait_not_paused(ctrl).await {
            return Err("cancelled".into());
        }
        let n = loop {
            match tokio::time::timeout(Duration::from_millis(200), data.read(&mut buf)).await {
                Ok(r) => break r.map_err(|e| e.to_string())?,
                Err(_) if ctrl.cancelled.load(Ordering::Relaxed) => return Err("cancelled".into()),
                Err(_) => {}
            }
        };
        if n == 0 {
            break;
        }
        let take = match want {
            Some(w) => ((w - got) as usize).min(n),
            None => n,
        };
        sink.write_all(&buf[..take])
            .await
            .map_err(|e| e.to_string())?;
        got += take as u64;
        *downloaded += take as u64;
        if part_idx.is_none() {
            ctrl.downloaded.store(*downloaded, Ordering::Relaxed);
        } else {
            ctrl.downloaded.fetch_add(take as u64, Ordering::Relaxed);
        }
        if let Some(i) = part_idx {
            if let Some(p) = ctrl
                .parts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(i)
            {
                p.0 += take as u64;
            }
        } else if let Some(p) = ctrl
            .parts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_mut()
        {
            p.0 = *downloaded;
        }
    }
    sink.flush().await.map_err(|e| e.to_string())?;
    if let Some(w) = want {
        if got != w {
            return Err(format!("FTP segment short read: got {got} expected {w}"));
        }
    }
    let (code, text) = ftp.reply("transfer-complete").await?;
    if code != 226 && code != 250 {
        return Err(format!("FTP transfer -> {text}"));
    }
    Ok(())
}

/// FTP download: SIZE total, EPSV/PASV/PORT data, REST resume; segmented when segments>1 + REST works.
pub async fn download_ftp(
    mut task: DownloadTask,
    ctrl: Arc<Controls>,
) -> Result<DownloadTask, String> {
    let url = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let host = url.host_str().ok_or("FTP URL missing host")?.to_string();
    let (mode, port) = tls_mode_and_port(&url)?;
    let path = pct_decode(url.path());
    if path.is_empty() || path == "/" {
        return Err("FTP URL missing file path".into());
    }
    let user = if !task.username.is_empty() {
        task.username.clone()
    } else if !url.username().is_empty() {
        pct_decode(url.username())
    } else {
        "anonymous".into()
    };
    let pass = if !task.password.is_empty() {
        task.password.clone()
    } else if let Some(p) = url.password() {
        pct_decode(p)
    } else {
        "anonymous@".into()
    };
    for v in [&user, &pass, &path] {
        if v.contains(['\r', '\n']) {
            return Err("invalid FTP credential/path".into());
        }
    }
    if task.filename.is_empty() {
        task.filename = crate::model::filename_from_url(&task.url);
    }
    tokio::fs::create_dir_all(&task.dest_dir)
        .await
        .map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part = task.dest_dir.join(format!("{}.part", task.filename));
    let lg = Login {
        host: host.clone(),
        port,
        mode,
        user,
        pass,
        path: path.clone(),
    };
    let mut ftp = login(&lg).await?;
    let total: Option<u64> = match ftp.cmd("SIZE", &format!("SIZE {path}")).await? {
        (213, t) => t.split_whitespace().last().and_then(|v| v.parse().ok()),
        _ => None,
    };
    let wanted = if task.segments == 0 {
        crate::model::Settings::default().max_connections
    } else {
        task.segments
    };
    // Segmented probe: REST 0 -> 350 on this control conn. Only when worth splitting.
    let mut segs: Vec<(u64, u64)> = Vec::new();
    if let Some(t) = total {
        if wanted > 1 && t >= 1024 {
            let (code, _) = ftp.cmd("REST", "REST 0").await?;
            if code == 350 {
                segs = crate::engine::plan_segments(t, wanted);
            }
            if segs.len() <= 1 {
                segs.clear();
            }
        }
    }
    if !segs.is_empty() {
        let t = total.unwrap();
        task.total = Some(t);
        // ponytail: segmented resume not tracked; preallocated .part size lies (set_len), so any existing .part is discarded and file restarted. Tradeoff: pause mid-segmented run loses progress; add per-segment state file when segmented FTP resume matters.
        let _ = tokio::fs::remove_file(&part).await;
        {
            let f = tokio::fs::File::create(&part)
                .await
                .map_err(|e| e.to_string())?;
            f.set_len(t).await.map_err(|e| e.to_string())?;
        }
        ctrl.total.store(t, Ordering::Relaxed);
        ctrl.downloaded.store(0, Ordering::Relaxed);
        ctrl.parts_are_segments.store(false, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) =
            segs.iter().map(|(s, e)| (0u64, e + 1 - s)).collect();
        task.downloaded = 0;
        task.status = TaskStatus::Downloading;
        drop(ftp);
        let lg = Arc::new(lg);
        let part = Arc::new(part);
        let mut handles = Vec::new();
        for (idx, (start, end)) in segs.iter().enumerate() {
            let (lg2, ctrl2, part2) = (lg.clone(), ctrl.clone(), part.clone());
            let (start, len) = (*start, end + 1 - start);
            handles.push(tokio::spawn(async move {
                let mut c = login(&lg2).await?;
                let mut f = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&*part2)
                    .await
                    .map_err(|e| e.to_string())?;
                f.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| e.to_string())?;
                let mut dl = 0u64;
                retr_exact(
                    &mut c,
                    &lg2,
                    Some(start),
                    Some(len),
                    &mut f,
                    &ctrl2,
                    Some(idx),
                    &mut dl,
                )
                .await
            }));
        }
        for h in handles {
            match h.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if ctrl.cancelled.load(Ordering::Relaxed) || e == "cancelled" {
                        task.status = TaskStatus::Paused;
                        task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
                        return Ok(task);
                    }
                    return Err(e);
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        if ctrl.cancelled.load(Ordering::Relaxed) {
            task.status = TaskStatus::Paused;
            task.downloaded = ctrl.downloaded.load(Ordering::Relaxed);
            return Ok(task);
        }
        if ctrl.downloaded.load(Ordering::Relaxed) != t {
            return Err(format!(
                "FTP size mismatch: got {} expected {t}",
                ctrl.downloaded.load(Ordering::Relaxed)
            ));
        }
        if dest.exists() {
            tokio::fs::remove_file(&dest)
                .await
                .map_err(|e| e.to_string())?;
        }
        tokio::fs::rename(&*part, &dest)
            .await
            .map_err(|e| e.to_string())?;
        task.downloaded = t;
        task.status = TaskStatus::Complete;
        return Ok(task);
    }
    // Single connection (unchanged resume semantics).
    let mut offset = tokio::fs::metadata(&part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if let Some(t) = total {
        task.total = Some(t);
        ctrl.total.store(t, Ordering::Relaxed);
        ctrl.parts_are_segments.store(false, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(offset.min(t), t)];
        if t > 0 && offset == t {
            if dest.exists() {
                tokio::fs::remove_file(&dest)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            tokio::fs::rename(&part, &dest)
                .await
                .map_err(|e| e.to_string())?;
            ctrl.downloaded.store(t, Ordering::Relaxed);
            task.downloaded = t;
            task.status = TaskStatus::Complete;
            return Ok(task);
        }
        if offset > t {
            let _ = tokio::fs::remove_file(&part).await;
            offset = 0;
        }
    }
    ctrl.downloaded.store(offset, Ordering::Relaxed);
    if total.is_none() {
        ctrl.parts_are_segments.store(false, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()) = vec![(offset, 0)];
    }
    task.downloaded = offset;
    task.status = TaskStatus::Downloading;
    // REST failure here restarts from 0 (probe above only ran when segmenting).
    let rest_ok = if offset > 0 {
        let (code, _) = ftp.cmd("REST", &format!("REST {offset}")).await?;
        if code == 350 {
            Some(offset)
        } else {
            offset = 0;
            ctrl.downloaded.store(0, Ordering::Relaxed);
            if let Some(p) = ctrl
                .parts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_mut()
            {
                p.0 = 0;
            }
            None
        }
    } else {
        None
    };
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(offset > 0)
        .write(true)
        .truncate(offset == 0)
        .open(&part)
        .await
        .map_err(|e| e.to_string())?;
    let mut downloaded = offset;
    let _ = rest_ok; // REST already sent above; retr_exact would duplicate it
    let r = retr_exact(
        &mut ftp,
        &lg,
        None,
        None,
        &mut file,
        &ctrl,
        None,
        &mut downloaded,
    )
    .await;
    if let Err(e) = r {
        if e == "cancelled" {
            file.flush().await.map_err(|x| x.to_string())?;
            task.status = TaskStatus::Paused;
            task.downloaded = downloaded;
            return Ok(task);
        }
        return Err(e);
    }
    drop(file);
    let _ = ftp.cmd("QUIT", "QUIT").await;
    if let Some(t) = total {
        if downloaded != t {
            return Err(format!("FTP size mismatch: got {downloaded} expected {t}"));
        }
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
    if task.total.is_none() {
        task.total = Some(downloaded);
        ctrl.total.store(downloaded, Ordering::Relaxed);
        if let Some(p) = ctrl
            .parts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_mut()
        {
            *p = (downloaded, downloaded);
        }
    }
    task.status = TaskStatus::Complete;
    Ok(task)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Copy, PartialEq)]
    enum FakeAuth {
        Normal,
        User230,
        Pass530,
    }
    #[derive(Clone, Copy, PartialEq)]
    enum FakeMode {
        Normal,
        PasvHang,
        PrivatePasv,
        NoRest,
    }
    struct FakeState {
        cmds: Mutex<Vec<String>>,
        rest: Mutex<Vec<u64>>,
        auth: FakeAuth,
        multiline: bool,
        mode: FakeMode,
        epsv_used: Mutex<bool>,
        port_used: Mutex<bool>,
    }
    impl FakeState {
        fn new(auth: FakeAuth, multiline: bool, mode: FakeMode) -> Self {
            Self {
                cmds: Mutex::new(Vec::new()),
                rest: Mutex::new(Vec::new()),
                auth,
                multiline,
                mode,
                epsv_used: Mutex::new(false),
                port_used: Mutex::new(false),
            }
        }
    }

    async fn fake_conn(sock: TcpStream, data: Arc<Vec<u8>>, st: Arc<FakeState>) {
        let (rd, mut wr) = tokio::io::split(sock);
        let mut lines = BufReader::new(rd).lines();
        enum Pend {
            Pasv(TcpListener),
            Port(String),
            Epsv(TcpListener),
        }
        let mut pend: Option<Pend> = None;
        let mut offset = 0u64;
        if st.multiline {
            wr.write_all(b"220-fake one\r\n220-fake two\r\n220 fake\r\n")
                .await
                .unwrap();
        } else {
            wr.write_all(b"220 fake\r\n").await.unwrap();
        }
        while let Ok(Some(line)) = lines.next_line().await {
            st.cmds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(line.clone());
            let mut it = line.splitn(2, ' ');
            let cmd = it.next().unwrap_or("").to_ascii_uppercase();
            let arg = it.next().unwrap_or("").to_string();
            match cmd.as_str() {
                "USER" => {
                    if st.auth == FakeAuth::User230 {
                        wr.write_all(b"230 no pass needed\r\n").await.unwrap();
                    } else {
                        wr.write_all(b"331 pass please\r\n").await.unwrap();
                    }
                }
                "PASS" => {
                    if st.auth == FakeAuth::Pass530 {
                        wr.write_all(b"530 Login incorrect\r\n").await.unwrap();
                    } else {
                        wr.write_all(b"230 ok\r\n").await.unwrap();
                    }
                }
                "TYPE" => {
                    wr.write_all(b"200 ok\r\n").await.unwrap();
                }
                "SIZE" => {
                    wr.write_all(format!("213 {}\r\n", data.len()).as_bytes())
                        .await
                        .unwrap();
                }
                "EPSV" => {
                    if st.mode == FakeMode::PrivatePasv {
                        wr.write_all(b"502 no epsv\r\n").await.unwrap();
                    } else if st.mode == FakeMode::PasvHang { /* hang: never reply */
                    } else {
                        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let p = l.local_addr().unwrap().port();
                        *st.epsv_used.lock().unwrap_or_else(|e| e.into_inner()) = true;
                        wr.write_all(
                            format!("229 Entering Extended Passive Mode (|||{p}|)\r\n").as_bytes(),
                        )
                        .await
                        .unwrap();
                        pend = Some(Pend::Epsv(l));
                    }
                }
                "PASV" => {
                    if st.mode == FakeMode::PasvHang { /* hang: simulates server never replying PASV (reported bug) */
                    } else {
                        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let p = l.local_addr().unwrap().port();
                        let ip = if st.mode == FakeMode::PrivatePasv {
                            "10,0,0,99"
                        } else {
                            "127,0,0,1"
                        };
                        wr.write_all(
                            format!(
                                "227 Entering Passive Mode ({ip},{},{})\r\n",
                                p / 256,
                                p % 256
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                        pend = Some(Pend::Pasv(l));
                    }
                }
                "PORT" => {
                    *st.port_used.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    pend = Some(Pend::Port(arg.clone()));
                    wr.write_all(b"200 ok\r\n").await.unwrap();
                }
                "REST" => {
                    if st.mode == FakeMode::NoRest {
                        wr.write_all(b"502 no rest\r\n").await.unwrap();
                    } else {
                        offset = arg.parse().unwrap_or(0);
                        st.rest
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(offset);
                        wr.write_all(b"350 ok\r\n").await.unwrap();
                    }
                }
                "RETR" => {
                    wr.write_all(b"150 go\r\n").await.unwrap();
                    match pend.take().unwrap() {
                        Pend::Pasv(l) | Pend::Epsv(l) => {
                            let (mut ds, _) = l.accept().await.unwrap();
                            ds.write_all(&data[offset as usize..]).await.unwrap();
                            ds.shutdown().await.unwrap();
                        }
                        Pend::Port(a) => {
                            let n: Vec<u16> =
                                a.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                            let addr = format!("{}.{}.{}.{}", n[0], n[1], n[2], n[3]);
                            let mut ds = TcpStream::connect((addr.as_str(), n[4] * 256 + n[5]))
                                .await
                                .unwrap();
                            ds.write_all(&data[offset as usize..]).await.unwrap();
                            ds.shutdown().await.unwrap();
                        }
                    }
                    offset = 0;
                    wr.write_all(b"226 done\r\n").await.unwrap();
                }
                "QUIT" => {
                    wr.write_all(b"221 bye\r\n").await.unwrap();
                    break;
                }
                _ => {
                    wr.write_all(b"502 nope\r\n").await.unwrap();
                }
            }
        }
    }

    async fn fake_ftp(listener: TcpListener, data: Vec<u8>, st: Arc<FakeState>) {
        let data = Arc::new(data);
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(fake_conn(sock, data.clone(), st.clone()));
        }
    }

    struct RunCfg {
        auth: FakeAuth,
        multiline: bool,
        mode: FakeMode,
        segments: u32,
        prefill: usize,
    }
    impl Default for RunCfg {
        fn default() -> Self {
            Self {
                auth: FakeAuth::Normal,
                multiline: false,
                mode: FakeMode::Normal,
                segments: 1,
                prefill: 0,
            }
        }
    }

    async fn run_cfg(
        payload_len: usize,
        user: &str,
        pass: &str,
        cfg: RunCfg,
        expect_ok: bool,
    ) -> Option<(
        DownloadTask,
        std::path::PathBuf,
        Arc<FakeState>,
        Arc<Controls>,
    )> {
        let payload: Vec<u8> = (0..payload_len as u32).map(|i| (i % 251) as u8).collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let st = Arc::new(FakeState::new(cfg.auth, cfg.multiline, cfg.mode));
        tokio::spawn(fake_ftp(listener, payload.clone(), st.clone()));
        let dir = std::env::temp_dir().join(format!(
            "downloadcraft-ftp-{}-{}-{port}",
            std::process::id(),
            payload_len
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if cfg.prefill > 0 {
            std::fs::write(dir.join("file.bin.part"), &payload[..cfg.prefill]).unwrap();
        }
        let task = DownloadTask {
            id: "f1".into(),
            url: if user.is_empty() && pass.is_empty() {
                format!("ftp://bob:urlpw@127.0.0.1:{port}/dir/file.bin")
            } else {
                format!("ftp://127.0.0.1:{port}/dir/file.bin")
            },
            filename: "file.bin".into(),
            dest_dir: dir.clone(),
            username: user.into(),
            password: pass.into(),
            segments: cfg.segments,
            ..Default::default()
        };
        let ctrl = Arc::new(Controls::new(0));
        let done = match download_ftp(task, ctrl.clone()).await {
            Ok(t) => t,
            Err(e) => {
                assert!(!expect_ok, "unexpected ftp error: {e}");
                assert!(e.contains("FTP auth failed"), "{e}");
                assert!(e.contains("530"), "{e}");
                assert!(!e.contains("secret"), "password leaked: {e}");
                assert!(!e.contains("reply timeout"), "{e}");
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
        };
        assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), payload_len as u64);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), payload_len as u64);
        assert!(!ctrl.parts_are_segments.load(Ordering::Relaxed));
        Some((done, dir, st, ctrl))
    }

    async fn run(
        payload_len: usize,
        prefill: usize,
        user: &str,
        pass: &str,
    ) -> (DownloadTask, std::path::PathBuf, Arc<FakeState>) {
        let (d, dir, st, _) = run_cfg(
            payload_len,
            user,
            pass,
            RunCfg {
                prefill,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        (d, dir, st)
    }
    fn payload(n: usize) -> Vec<u8> {
        (0..n as u32).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn scheme_port_defaults() {
        let (m, p) = tls_mode_and_port(&url::Url::parse("ftp://h/f").unwrap()).unwrap();
        assert_eq!((m, p), (TlsMode::Plain, 21));
        let (m, p) = tls_mode_and_port(&url::Url::parse("ftps://h/f").unwrap()).unwrap();
        assert_eq!((m, p), (TlsMode::Explicit, 21));
        let (m, p) = tls_mode_and_port(&url::Url::parse("ftpsi://h/f").unwrap()).unwrap();
        assert_eq!((m, p), (TlsMode::Implicit, 990));
        let (m, p) = tls_mode_and_port(&url::Url::parse("ftpsi://h:21/f").unwrap()).unwrap();
        assert_eq!((m, p), (TlsMode::Implicit, 21));
        assert!(tls_mode_and_port(&url::Url::parse("http://h/f").unwrap()).is_err());
    }

    #[test]
    fn pasv_host_cases() {
        assert_eq!(
            pasv_host("10.1.2.3".parse().unwrap(), "203.0.113.5"),
            "203.0.113.5"
        );
        assert_eq!(
            pasv_host("192.168.1.9".parse().unwrap(), "127.0.0.1"),
            "127.0.0.1"
        );
        assert_eq!(
            pasv_host("10.1.2.3".parse().unwrap(), "10.1.2.3"),
            "10.1.2.3"
        );
        assert_eq!(
            pasv_host("203.0.113.9".parse().unwrap(), "127.0.0.1"),
            "203.0.113.9"
        );
        assert_eq!(
            parse_epsv_port("229 Entering Extended Passive Mode (|||50000|)"),
            Some(50000)
        );
    }

    // Live FTPS integration test intentionally skipped: no in-tree TLS FTP
    // server fixture (rcgen + vsftpd-class server too heavy for unit suite);
    // TLS wiring covered by scheme tests + shared wrap_tls with nntp.rs.

    #[tokio::test]
    async fn ftp_fresh_download_auth() {
        let (done, dir, st) = run(50_000, 0, "alice", "secret").await;
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(done.total, Some(50_000));
        assert_eq!(
            std::fs::read(dir.join("file.bin")).unwrap(),
            payload(50_000)
        );
        assert!(!dir.join("file.bin.part").exists());
        let cmds = st.cmds.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(cmds.iter().any(|c| c == "USER alice"), "{cmds:?}");
        assert!(cmds.iter().any(|c| c == "PASS secret"), "{cmds:?}");
        assert!(
            cmds.iter().any(|c| c.starts_with("RETR /dir/file.bin")),
            "{cmds:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_pass_530_is_auth_error_not_timeout() {
        let r = run_cfg(
            1_000,
            "alice",
            "secret",
            RunCfg {
                auth: FakeAuth::Pass530,
                ..Default::default()
            },
            false,
        )
        .await;
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn ftp_user_230_needs_no_pass() {
        let (done, dir, st, _) = run_cfg(
            1_000,
            "alice",
            "secret",
            RunCfg {
                auth: FakeAuth::User230,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        let cmds = st.cmds.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(cmds.iter().any(|c| c == "USER alice"), "{cmds:?}");
        assert!(!cmds.iter().any(|c| c.starts_with("PASS")), "{cmds:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_multiline_greeting_and_url_creds() {
        let (done, dir, st, _) = run_cfg(
            1_000,
            "",
            "",
            RunCfg {
                multiline: true,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        let cmds = st.cmds.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(cmds.iter().any(|c| c == "USER bob"), "{cmds:?}");
        assert!(cmds.iter().any(|c| c == "PASS urlpw"), "{cmds:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_resume_via_rest() {
        let (done, dir, st) = run(50_000, 4_000, "anonymous", "anonymous@").await;
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(
            st.rest.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec![4_000]
        );
        assert_eq!(
            std::fs::read(dir.join("file.bin")).unwrap(),
            payload(50_000)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_passive_hang_falls_back_to_port() {
        let (done, dir, st, _) = run_cfg(
            50_000,
            "alice",
            "secret",
            RunCfg {
                mode: FakeMode::PasvHang,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(
            std::fs::read(dir.join("file.bin")).unwrap(),
            payload(50_000)
        );
        assert!(*st.port_used.lock().unwrap_or_else(|e| e.into_inner()));
        let cmds = st.cmds.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(cmds.iter().any(|c| c.starts_with("PORT ")), "{cmds:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_private_pasv_ip_uses_control_host() {
        let (done, dir, _, _) = run_cfg(
            50_000,
            "alice",
            "secret",
            RunCfg {
                mode: FakeMode::PrivatePasv,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(
            std::fs::read(dir.join("file.bin")).unwrap(),
            payload(50_000)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_epsv_used() {
        let (done, dir, st, _) = run_cfg(50_000, "alice", "secret", RunCfg::default(), true)
            .await
            .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        assert!(*st.epsv_used.lock().unwrap_or_else(|e| e.into_inner()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_segmented_download_and_rest_probe() {
        let (done, dir, st, ctrl) = run_cfg(
            200_000,
            "alice",
            "secret",
            RunCfg {
                segments: 4,
                prefill: 5_000,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        // prefilled .part restarted (segmented resume tradeoff): content still exact
        assert_eq!(
            std::fs::read(dir.join("file.bin")).unwrap(),
            payload(200_000)
        );
        let rests = st.rest.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(rests.contains(&0), "{rests:?}");
        for s in [50_000u64, 100_000, 150_000] {
            assert!(rests.contains(&s), "{rests:?}");
        }
        let parts = ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            parts,
            vec![
                (50_000, 50_000),
                (50_000, 50_000),
                (50_000, 50_000),
                (50_000, 50_000)
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_no_rest_falls_back_single() {
        let (done, dir, _, _) = run_cfg(
            50_000,
            "alice",
            "secret",
            RunCfg {
                mode: FakeMode::NoRest,
                segments: 4,
                ..Default::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(
            std::fs::read(dir.join("file.bin")).unwrap(),
            payload(50_000)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
