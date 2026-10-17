// Minimal FTP client (PASV/REST binary) over tokio TCP. No new deps.
// ponytail: plain FTP only, no FTPS/active mode; add suppaftp+rustls if FTPS needed.
use crate::engine::Controls;
use crate::model::{DownloadTask, TaskStatus};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

struct Ftp {
    rd: BufReader<ReadHalf<TcpStream>>,
    wr: WriteHalf<TcpStream>,
}

impl Ftp {
    async fn reply(&mut self) -> Result<(u16, String), String> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            let n = self.rd.read_line(&mut line).await.map_err(|e| e.to_string())?;
            if n == 0 { return Err("FTP connection closed".into()); }
            let code: u16 = line.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);
            text.push_str(line.trim_end());
            if line.as_bytes().get(3) != Some(&b'-') { return Ok((code, text)); }
            text.push(' ');
        }
    }
    async fn cmd(&mut self, c: &str) -> Result<(u16, String), String> {
        self.wr.write_all(format!("{c}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        self.wr.flush().await.map_err(|e| e.to_string())?;
        self.reply().await
    }
    async fn expect(&mut self, c: &str, ok: &[u16]) -> Result<String, String> {
        let (code, text) = self.cmd(c).await?;
        if ok.contains(&code) { Ok(text) } else { Err(format!("FTP {c} -> {text}")) }
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) { out.push(v); i += 3; continue; }
        }
        out.push(b[i]); i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn wait_not_paused(ctrl: &Controls) -> bool {
    while ctrl.paused.load(Ordering::Relaxed) {
        if ctrl.cancelled.load(Ordering::Relaxed) { return true; }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    ctrl.cancelled.load(Ordering::Relaxed)
}

/// FTP download: SIZE total, PASV data channel, REST resume from `<filename>.part`.
pub async fn download_ftp(mut task: DownloadTask, ctrl: Arc<Controls>) -> Result<DownloadTask, String> {
    let url = url::Url::parse(&task.url).map_err(|e| e.to_string())?;
    let host = url.host_str().ok_or("FTP URL missing host")?.to_string();
    let port = url.port().unwrap_or(21);
    let path = pct_decode(url.path());
    if path.is_empty() || path == "/" { return Err("FTP URL missing file path".into()); }
    let user = if !task.username.is_empty() { task.username.clone() } else if !url.username().is_empty() { pct_decode(url.username()) } else { "anonymous".into() };
    let pass = if !task.password.is_empty() { task.password.clone() } else if let Some(p) = url.password() { pct_decode(p) } else { "anonymous@".into() };
    for v in [&user, &pass, &path] { if v.contains(['\r', '\n']) { return Err("invalid FTP credential/path".into()); } }
    if task.filename.is_empty() { task.filename = crate::model::filename_from_url(&task.url); }
    tokio::fs::create_dir_all(&task.dest_dir).await.map_err(|e| e.to_string())?;
    let dest = task.dest_dir.join(&task.filename);
    let part = task.dest_dir.join(format!("{}.part", task.filename));
    let mut offset = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);

    let stream = TcpStream::connect((host.as_str(), port)).await.map_err(|e| format!("FTP connect {host}:{port}: {e}"))?;
    let (rd, wr) = tokio::io::split(stream);
    let mut ftp = Ftp { rd: BufReader::new(rd), wr };
    let (code, text) = ftp.reply().await?;
    if code != 220 { return Err(format!("FTP greeting -> {text}")); }
    let (code, text) = ftp.cmd(&format!("USER {user}")).await?;
    if code == 331 { ftp.expect(&format!("PASS {pass}"), &[230]).await?; }
    else if code != 230 { return Err(format!("FTP USER -> {text}")); }
    ftp.expect("TYPE I", &[200]).await?;

    // SIZE optional: unknown size still downloads until data EOF.
    let total: Option<u64> = match ftp.cmd(&format!("SIZE {path}")).await? {
        (213, t) => t.split_whitespace().last().and_then(|v| v.parse().ok()),
        _ => None,
    };
    if let Some(t) = total {
        task.total = Some(t);
        ctrl.total.store(t, Ordering::Relaxed);
        ctrl.parts_are_segments.store(false, Ordering::Relaxed);
        *ctrl.parts.lock().unwrap() = vec![(offset.min(t), t)];
        if t > 0 && offset == t {
            if dest.exists() { tokio::fs::remove_file(&dest).await.map_err(|e| e.to_string())?; }
            tokio::fs::rename(&part, &dest).await.map_err(|e| e.to_string())?;
            ctrl.downloaded.store(t, Ordering::Relaxed);
            task.downloaded = t; task.status = TaskStatus::Complete;
            return Ok(task);
        }
        if offset > t { let _ = tokio::fs::remove_file(&part).await; offset = 0; }
    }
    ctrl.downloaded.store(offset, Ordering::Relaxed);
    if total.is_none() { ctrl.parts_are_segments.store(false, Ordering::Relaxed); *ctrl.parts.lock().unwrap() = vec![(offset, 0)]; }
    task.downloaded = offset;
    task.status = TaskStatus::Downloading;

    let pasv = ftp.expect("PASV", &[227]).await?;
    let nums: Vec<u8> = pasv[pasv.find('(').ok_or("bad PASV reply")? + 1..pasv.find(')').ok_or("bad PASV reply")?]
        .split(',').filter_map(|v| v.trim().parse().ok()).collect();
    if nums.len() != 6 { return Err(format!("bad PASV reply: {pasv}")); }
    let data_host = if nums[..4] == [0, 0, 0, 0] { host.clone() } else { format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3]) };
    let data_port = (nums[4] as u16) * 256 + nums[5] as u16;
    let mut data = TcpStream::connect((data_host.as_str(), data_port)).await.map_err(|e| format!("FTP data connect: {e}"))?;

    if offset > 0 {
        let (code, _) = ftp.cmd(&format!("REST {offset}")).await?;
        if code != 350 { offset = 0; ctrl.downloaded.store(0, Ordering::Relaxed); if let Some(part) = ctrl.parts.lock().unwrap().first_mut() { part.0 = 0; } } // server cannot resume: restart below via truncate
    }
    ftp.expect(&format!("RETR {path}"), &[125, 150]).await?;

    let mut file = tokio::fs::OpenOptions::new().create(true).append(offset > 0).write(true).truncate(offset == 0).open(&part).await.map_err(|e| e.to_string())?;
    let mut downloaded = offset;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if wait_not_paused(&ctrl).await {
            file.flush().await.map_err(|e| e.to_string())?;
            task.status = TaskStatus::Paused; task.downloaded = downloaded;
            return Ok(task);
        }
        // Wake periodically while server stalls so cancelled flag ends transfer.
        let n = loop {
            match tokio::time::timeout(std::time::Duration::from_millis(200), data.read(&mut buf)).await {
                Ok(r) => break r.map_err(|e| e.to_string())?,
                Err(_) if ctrl.cancelled.load(Ordering::Relaxed) => {
                    file.flush().await.map_err(|e| e.to_string())?;
                    task.status = TaskStatus::Paused; task.downloaded = downloaded;
                    return Ok(task);
                }
                Err(_) => {}
            }
        };
        if n == 0 { break; }
        file.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
        downloaded += n as u64;
        ctrl.downloaded.store(downloaded, Ordering::Relaxed);
        if let Some(part) = ctrl.parts.lock().unwrap().first_mut() { part.0 = downloaded; }
        task.downloaded = downloaded;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    let (code, text) = ftp.reply().await?;
    if code != 226 && code != 250 { return Err(format!("FTP transfer -> {text}")); }
    if let Some(t) = total {
        if downloaded != t { return Err(format!("FTP size mismatch: got {downloaded} expected {t}")); }
    }
    let _ = ftp.cmd("QUIT").await;
    if dest.exists() { tokio::fs::remove_file(&dest).await.map_err(|e| e.to_string())?; }
    tokio::fs::rename(&part, &dest).await.map_err(|e| e.to_string())?;
    task.downloaded = downloaded;
    if task.total.is_none() { task.total = Some(downloaded); ctrl.total.store(downloaded, Ordering::Relaxed); if let Some(part) = ctrl.parts.lock().unwrap().first_mut() { *part = (downloaded, downloaded); } }
    task.status = TaskStatus::Complete;
    Ok(task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    struct FakeState { cmds: Mutex<Vec<String>>, rest: Mutex<u64> }

    async fn fake_ftp(listener: TcpListener, data: Vec<u8>, st: Arc<FakeState>) {
        let (sock, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = tokio::io::split(sock);
        let mut lines = BufReader::new(rd).lines();
        let mut data_listener: Option<TcpListener> = None;
        let mut offset = 0u64;
        wr.write_all(b"220 fake\r\n").await.unwrap();
        while let Ok(Some(line)) = lines.next_line().await {
            st.cmds.lock().unwrap().push(line.clone());
            let mut it = line.splitn(2, ' ');
            let cmd = it.next().unwrap_or("").to_ascii_uppercase();
            let arg = it.next().unwrap_or("");
            match cmd.as_str() {
                "USER" => { wr.write_all(b"331 pass please\r\n").await.unwrap(); }
                "PASS" => { wr.write_all(b"230 ok\r\n").await.unwrap(); }
                "TYPE" => { wr.write_all(b"200 ok\r\n").await.unwrap(); }
                "SIZE" => { wr.write_all(format!("213 {}\r\n", data.len()).as_bytes()).await.unwrap(); }
                "PASV" => {
                    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let p = l.local_addr().unwrap().port();
                    wr.write_all(format!("227 Entering Passive Mode (127,0,0,1,{},{})\r\n", p / 256, p % 256).as_bytes()).await.unwrap();
                    data_listener = Some(l);
                }
                "REST" => { offset = arg.parse().unwrap_or(0); *st.rest.lock().unwrap() = offset; wr.write_all(b"350 ok\r\n").await.unwrap(); }
                "RETR" => {
                    wr.write_all(b"150 go\r\n").await.unwrap();
                    let (mut ds, _) = data_listener.take().unwrap().accept().await.unwrap();
                    ds.write_all(&data[offset as usize..]).await.unwrap();
                    ds.shutdown().await.unwrap();
                    drop(ds);
                    wr.write_all(b"226 done\r\n").await.unwrap();
                }
                "QUIT" => { wr.write_all(b"221 bye\r\n").await.unwrap(); break; }
                _ => { wr.write_all(b"502 nope\r\n").await.unwrap(); }
            }
        }
    }

    async fn run(payload_len: usize, prefill: usize, user: &str, pass: &str) -> (DownloadTask, std::path::PathBuf, Arc<FakeState>) {
        let payload: Vec<u8> = (0..payload_len as u32).map(|i| (i % 251) as u8).collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let st = Arc::new(FakeState { cmds: Mutex::new(Vec::new()), rest: Mutex::new(0) });
        tokio::spawn(fake_ftp(listener, payload.clone(), st.clone()));
        let dir = std::env::temp_dir().join(format!("downloadcraft-ftp-{}-{prefill}-{user}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
        if prefill > 0 { std::fs::write(dir.join("file.bin.part"), &payload[..prefill]).unwrap(); }
        let task = DownloadTask { id: "f1".into(), url: format!("ftp://127.0.0.1:{port}/dir/file.bin"), filename: "file.bin".into(), dest_dir: dir.clone(), username: user.into(), password: pass.into(), ..Default::default() };
        let ctrl = Arc::new(Controls::new(0));
        let done = download_ftp(task, ctrl.clone()).await.unwrap();
        assert_eq!(ctrl.downloaded.load(Ordering::Relaxed), payload_len as u64);
        assert_eq!(ctrl.total.load(Ordering::Relaxed), payload_len as u64);
        assert!(!ctrl.parts_are_segments.load(Ordering::Relaxed));
        assert_eq!(ctrl.parts.lock().unwrap().clone(), vec![(payload_len as u64, payload_len as u64)]);
        (done, dir, st)
    }

    #[tokio::test]
    async fn ftp_fresh_download_auth() {
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let (done, dir, st) = run(50_000, 0, "alice", "secret").await;
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(done.total, Some(50_000));
        assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), payload);
        assert!(!dir.join("file.bin.part").exists());
        let cmds = st.cmds.lock().unwrap().clone();
        assert!(cmds.iter().any(|c| c == "USER alice"), "{cmds:?}");
        assert!(cmds.iter().any(|c| c == "PASS secret"), "{cmds:?}");
        assert!(cmds.iter().any(|c| c.starts_with("RETR /dir/file.bin")), "{cmds:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ftp_resume_via_rest() {
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let (done, dir, st) = run(50_000, 4_000, "anonymous", "anonymous@").await;
        assert_eq!(done.status, TaskStatus::Complete);
        assert_eq!(*st.rest.lock().unwrap(), 4_000);
        assert_eq!(std::fs::read(dir.join("file.bin")).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
