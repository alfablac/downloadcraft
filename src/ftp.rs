// Minimal FTP/FTPS client (PASV/REST binary) over tokio TCP + rustls.
// Schemes: ftp:// plain (default port 21); ftps:// explicit FTPS - plain
// connect, then AUTH TLS, PBSZ 0, PROT P (default port 21, NOT 990);
// ftpsi:// implicit FTPS - TLS on connect (default port 990). Data
// connections use TLS whenever control does (PROT P). Certs verified
// against webpki roots (no accept-invalid), server name = URL host.
use crate::engine::Controls;
use crate::model::{DownloadTask, TaskStatus};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf,
    WriteHalf,
};
use tokio::net::TcpStream;

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

async fn connect_timeout(addr: (&str, u16)) -> Result<tokio::net::TcpStream, String> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(format!("FTP connect {}:{}: {e}", addr.0, addr.1)),
        Err(_) => Err(format!("FTP connect {}:{}: timeout", addr.0, addr.1)),
    }
}

/// FTP download: SIZE total, PASV data channel, REST resume from `<filename>.part`.
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
    let mut offset = tokio::fs::metadata(&part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let stream = connect_timeout((host.as_str(), port)).await?;
    // Implicit FTPS: TLS before any FTP bytes (greeting inside TLS).
    let wire = match mode {
        TlsMode::Implicit => Wire::Tls(Box::new(wrap_tls(stream, &host).await?)),
        _ => Wire::Plain(stream),
    };
    let (rd, wr) = tokio::io::split(wire);
    let mut ftp = Ftp {
        rd: BufReader::new(rd),
        wr,
    };
    let (code, text) = ftp.reply("greeting").await?;
    if code != 220 {
        return Err(format!("FTP greeting -> {text}"));
    }
    if mode == TlsMode::Explicit {
        // Explicit FTPS: AUTH TLS over plaintext greeting, then upgrade.
        ftp.expect("AUTH TLS", "AUTH TLS", &[234]).await?;
        ftp = ftp.upgrade_tls(&host).await?;
    }
    if mode != TlsMode::Plain {
        ftp.expect("PBSZ", "PBSZ 0", &[200]).await?;
        ftp.expect("PROT", "PROT P", &[200]).await?;
    }
    // USER: 230 = no password needed; 331 = send PASS.
    match ftp.cmd("USER", &format!("USER {user}")).await? {
        (230, _) => {}
        (331, _) => {
            ftp.expect("PASS", &format!("PASS {pass}"), &[230])
                .await
                .map_err(|e| format!("FTP auth failed for user '{user}': {e}"))?;
        }
        (code, text) => {
            return Err(format!(
                "FTP auth failed for user '{user}': USER server replied {code} {text}"
            ));
        }
    }
    ftp.expect("TYPE I", "TYPE I", &[200]).await?;

    // SIZE optional: unknown size still downloads until data EOF.
    let total: Option<u64> = match ftp.cmd("SIZE", &format!("SIZE {path}")).await? {
        (213, t) => t.split_whitespace().last().and_then(|v| v.parse().ok()),
        _ => None,
    };
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

    let pasv = ftp.expect("PASV", "PASV", &[227]).await?;
    let nums: Vec<u8> = pasv
        [pasv.find('(').ok_or("bad PASV reply")? + 1..pasv.find(')').ok_or("bad PASV reply")?]
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    if nums.len() != 6 {
        return Err(format!("bad PASV reply: {pasv}"));
    }
    let data_host = if nums[..4] == [0, 0, 0, 0] {
        host.clone()
    } else {
        format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3])
    };
    let data_port = (nums[4] as u16) * 256 + nums[5] as u16;
    if offset > 0 {
        let (code, _) = ftp.cmd("REST", &format!("REST {offset}")).await?;
        if code != 350 {
            offset = 0;
            ctrl.downloaded.store(0, Ordering::Relaxed);
            if let Some(part) = ctrl
                .parts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_mut()
            {
                part.0 = 0;
            }
        } // server cannot resume: restart below via truncate
    }
    ftp.expect("RETR", &format!("RETR {path}"), &[125, 150])
        .await?;

    // Data TCP+TLS after RETR accepted: FTPS servers may refuse
    // connect/handshake until RETR. Same order for plain, no cost.
    let data_tcp = connect_timeout((data_host.as_str(), data_port)).await?;
    let mut data: Wire = match mode {
        TlsMode::Plain => Wire::Plain(data_tcp),
        _ => Wire::Tls(Box::new(wrap_tls(data_tcp, &host).await?)),
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
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if wait_not_paused(&ctrl).await {
            file.flush().await.map_err(|e| e.to_string())?;
            task.status = TaskStatus::Paused;
            task.downloaded = downloaded;
            return Ok(task);
        }
        // Wake periodically while server stalls so cancelled flag ends transfer.
        let n = loop {
            match tokio::time::timeout(std::time::Duration::from_millis(200), data.read(&mut buf))
                .await
            {
                Ok(r) => break r.map_err(|e| e.to_string())?,
                Err(_) if ctrl.cancelled.load(Ordering::Relaxed) => {
                    file.flush().await.map_err(|e| e.to_string())?;
                    task.status = TaskStatus::Paused;
                    task.downloaded = downloaded;
                    return Ok(task);
                }
                Err(_) => {}
            }
        };
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        downloaded += n as u64;
        ctrl.downloaded.store(downloaded, Ordering::Relaxed);
        if let Some(part) = ctrl
            .parts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_mut()
        {
            part.0 = downloaded;
        }
        task.downloaded = downloaded;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    let (code, text) = ftp.reply("transfer-complete").await?;
    if code != 226 && code != 250 {
        return Err(format!("FTP transfer -> {text}"));
    }
    if let Some(t) = total {
        if downloaded != t {
            return Err(format!("FTP size mismatch: got {downloaded} expected {t}"));
        }
    }
    let _ = ftp.cmd("QUIT", "QUIT").await;
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
        if let Some(part) = ctrl
            .parts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_mut()
        {
            *part = (downloaded, downloaded);
        }
    }
    task.status = TaskStatus::Complete;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    #[derive(Clone, Copy, PartialEq)]
    enum FakeAuth {
        Normal,
        User230,
        Pass530,
    }
    struct FakeState {
        cmds: Mutex<Vec<String>>,
        rest: Mutex<u64>,
        auth: FakeAuth,
        multiline: bool,
    }

    async fn fake_ftp(listener: TcpListener, data: Vec<u8>, st: Arc<FakeState>) {
        let (sock, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = tokio::io::split(sock);
        let mut lines = BufReader::new(rd).lines();
        let mut data_listener: Option<TcpListener> = None;
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
            let arg = it.next().unwrap_or("");
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
                "PASV" => {
                    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let p = l.local_addr().unwrap().port();
                    wr.write_all(
                        format!(
                            "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                            p / 256,
                            p % 256
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                    data_listener = Some(l);
                }
                "REST" => {
                    offset = arg.parse().unwrap_or(0);
                    *st.rest.lock().unwrap_or_else(|e| e.into_inner()) = offset;
                    wr.write_all(b"350 ok\r\n").await.unwrap();
                }
                "RETR" => {
                    wr.write_all(b"150 go\r\n").await.unwrap();
                    let (mut ds, _) = data_listener.take().unwrap().accept().await.unwrap();
                    ds.write_all(&data[offset as usize..]).await.unwrap();
                    ds.shutdown().await.unwrap();
                    drop(ds);
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

    async fn run_with(
        payload_len: usize,
        prefill: usize,
        user: &str,
        pass: &str,
        auth: FakeAuth,
        multiline: bool,
        expect_ok: bool,
    ) -> Option<(DownloadTask, std::path::PathBuf, Arc<FakeState>)> {
        let payload: Vec<u8> = (0..payload_len as u32).map(|i| (i % 251) as u8).collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let st = Arc::new(FakeState {
            cmds: Mutex::new(Vec::new()),
            rest: Mutex::new(0),
            auth,
            multiline,
        });
        tokio::spawn(fake_ftp(listener, payload.clone(), st.clone()));
        let dir = std::env::temp_dir().join(format!(
            "downloadcraft-ftp-{}-{prefill}-{user}-{port}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if prefill > 0 {
            std::fs::write(dir.join("file.bin.part"), &payload[..prefill]).unwrap();
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
            ..Default::default()
        };
        let ctrl = Arc::new(Controls::new(0));
        let done = match download_ftp(task, ctrl.clone()).await {
            Ok(t) => t,
            Err(e) => {
                assert!(!expect_ok, "unexpected ftp error: {e}");
                // Auth failures must be stage-tagged, never "reply timeout",
                // and must never echo the password back.
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
        assert_eq!(
            ctrl.parts.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec![(payload_len as u64, payload_len as u64)]
        );
        Some((done, dir, st))
    }

    async fn run(
        payload_len: usize,
        prefill: usize,
        user: &str,
        pass: &str,
    ) -> (DownloadTask, std::path::PathBuf, Arc<FakeState>) {
        run_with(
            payload_len,
            prefill,
            user,
            pass,
            FakeAuth::Normal,
            false,
            true,
        )
        .await
        .unwrap()
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

    // Live FTPS integration test intentionally skipped: no in-tree TLS FTP
    // server fixture (rcgen + vsftpd-class server too heavy for unit suite);
    // TLS wiring covered by scheme tests + shared wrap_tls with nntp.rs.

    #[tokio::test]
    async fn ftp_fresh_download_auth() {
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let (done, dir, st) = run(50_000, 0, "alice", "secret").await;
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(done.total, Some(50_000));
        assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), payload);
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
        let r = run_with(1_000, 0, "alice", "secret", FakeAuth::Pass530, false, false).await;
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn ftp_user_230_needs_no_pass() {
        let (done, dir, st) = run_with(1_000, 0, "alice", "secret", FakeAuth::User230, false, true)
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
        let (done, dir, st) = run_with(1_000, 0, "", "", FakeAuth::Normal, true, true)
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
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let (done, dir, st) = run(50_000, 4_000, "anonymous", "anonymous@").await;
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(*st.rest.lock().unwrap_or_else(|e| e.into_inner()), 4_000);
        assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
