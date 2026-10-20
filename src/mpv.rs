//! mpv sidecar for Streams page: play direct video link or local file in mpv.
//! Never shell: Command + argv only.
use crate::model::Settings;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "mpv.exe"
    } else {
        "mpv"
    }
}

/// Find mpv: custom path (non-empty, is_file) wins; else mpv[.exe] beside
/// current exe, else config_dir()/bin/, else PATH search.
pub fn find_mpv(custom: &str) -> Option<PathBuf> {
    let custom = custom.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let p = crate::platform::config_dir().join("bin").join(exe_name());
    if p.is_file() {
        return Some(p);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(exe_name());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Validate input: URL with allowed scheme (http/https/file/rtsp/rtmp/rtmps,
/// case-insensitive) or scheme-less local path (Windows, UNC, relative).
/// Empty, dash-leading (looks-like-flag), control-char rejected.
/// Returns trimmed input.
fn validate_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("empty URL".into());
    }
    if url.starts_with('-') {
        return Err(format!("URL must not start with '-': {url}"));
    }
    if url.chars().any(|c| c.is_control()) {
        return Err("URL must not contain control characters".into());
    }
    if let Some((scheme, _)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if matches!(
            scheme.as_str(),
            "http" | "https" | "file" | "rtsp" | "rtmp" | "rtmps"
        ) {
            return Ok(url.to_string());
        }
        return Err(format!("unsupported URL scheme: {scheme}"));
    }
    Ok(url.to_string())
}

fn charset_ok(v: &str, ok: impl Fn(char) -> bool) -> bool {
    !v.is_empty() && v.chars().all(ok)
}

/// mpv argv for `url` (URL or local path) from Settings. Flags in Settings
/// field order; `--` before URL/path so it never parses as option.
pub fn build_args(s: &Settings, url: &str) -> Result<Vec<String>, String> {
    let url = validate_url(url)?;
    let mut a: Vec<String> = Vec::new();
    if s.mpv_fullscreen {
        a.push("--fullscreen".into());
    }
    if s.mpv_keep_open {
        a.push("--keep-open=yes".into());
    }
    if s.mpv_loop {
        a.push("--loop-file=inf".into());
    }
    if s.mpv_volume > 100 {
        return Err(format!(
            "mpv volume out of range (0..=100): {}",
            s.mpv_volume
        ));
    }
    if s.mpv_volume != 100 {
        a.push(format!("--volume={}", s.mpv_volume));
    }
    if !(0.25..=4.0).contains(&s.mpv_speed) {
        return Err(format!(
            "mpv speed out of range (0.25..=4.0): {}",
            s.mpv_speed
        ));
    }
    if s.mpv_speed != 1.0 {
        a.push(format!("--speed={}", s.mpv_speed));
    }
    let start = s.mpv_start.trim();
    if !start.is_empty() {
        if !charset_ok(start, |c| {
            c.is_ascii_digit() || matches!(c, ':' | '.' | '%')
        }) {
            return Err(format!("invalid mpv start value: {start}"));
        }
        a.push(format!("--start={start}"));
    }
    let hwdec = s.mpv_hwdec.trim();
    if !hwdec.is_empty() {
        if !charset_ok(hwdec, |c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
        }) {
            return Err(format!("invalid mpv hwdec value: {hwdec}"));
        }
        a.push(format!("--hwdec={hwdec}"));
    }
    a.push(format!(
        "--cache={}",
        if s.mpv_cache { "yes" } else { "no" }
    ));
    if s.mpv_cache_secs > 0 {
        a.push(format!("--cache-secs={}", s.mpv_cache_secs));
    }
    let sub_auto = s.mpv_sub_auto.trim();
    if !sub_auto.is_empty() {
        if !charset_ok(sub_auto, |c| c.is_ascii_lowercase() || c == '-') {
            return Err(format!("invalid mpv sub-auto value: {sub_auto}"));
        }
        a.push(format!("--sub-auto={sub_auto}"));
    }
    // ponytail: whitespace split only; quoted extra args with spaces unsupported.
    for tok in s.mpv_extra_args.split_whitespace() {
        a.push(tok.to_string());
    }
    a.push("--".into());
    a.push(url);
    Ok(a)
}

/// Re-export shim: UI codes against `mpv::embedded::*`.
pub mod embedded {
    pub use super::{embedded_args, ipc_pipe_path, spawn_embedded, MpvHandle, PlayerState};
}

/// Spawn mpv detached (null stdio, no window). Returns child pid.
pub fn launch(s: &Settings, url: &str) -> Result<u32, String> {
    let exe = find_mpv(&s.mpv_path).ok_or_else(|| {
        "mpv not found: install mpv, add it to PATH, or set path in Options > mpv".to_string()
    })?;
    let args = build_args(s, url)?;
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut cmd);
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch mpv: {e}"))?;
    Ok(child.id())
}

/// Alias for `launch`.
pub fn play(s: &Settings, url: &str) -> Result<u32, String> {
    launch(s, url)
}

// ===================== embedded (--wid) + JSON IPC =====================

#[derive(Debug, Clone)]
pub struct PlayerState {
    pub pause: bool,
    pub time_pos: f64,
    pub duration: f64,
    pub volume: f64,
    pub speed: f64,
    pub idle: bool,
    pub eof: bool,
    pub title: String,
    pub error: String, // pipe/launch failure; mpv IPC has no stable error prop
    pub aid: i64,      // -1 = no audio track selected
    pub mute: bool,
    pub paused_for_cache: bool,
    pub connected: bool, // IPC pipe connected / property events flowing
    pub path: String,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            pause: false,
            time_pos: 0.0,
            duration: 0.0,
            volume: 0.0,
            speed: 0.0,
            idle: false,
            eof: false,
            title: String::new(),
            error: String::new(),
            aid: -1,
            mute: false,
            paused_for_cache: false,
            connected: false,
            path: String::new(),
        }
    }
}

pub struct MpvHandle {
    pub state: Arc<Mutex<PlayerState>>,
    cmd_tx: mpsc::Sender<String>,
    child: Child, // kept so alive()/quit() can reap
}

pub fn ipc_pipe_path(pid: u32) -> String {
    format!(r"\\.\pipe\webcraft-mpv-{pid}")
}

static SEQ: AtomicU32 = AtomicU32::new(0);

pub fn embedded_args(
    s: &Settings,
    url: &str,
    wid: isize,
    pipe: &str,
) -> Result<Vec<String>, String> {
    let mut args = build_args(s, url)?;
    args.retain(|a| {
        let base = a.split('=').next().unwrap_or(a);
        !matches!(
            base,
            "--wid"
                | "--input-ipc-server"
                | "--really-quiet"
                | "--force-window"
                | "--idle"
                | "--terminal"
                | "--log-file"
                | "--volume"
                | "--mute"
        )
    });
    let emb = [
        format!("--wid={wid}"),
        format!("--input-ipc-server={pipe}"),
        format!(
            "--log-file={}",
            crate::platform::cache_dir()
                .join("mpv-embedded.log")
                .display()
        ),
        "--force-window=yes".into(),
        "--idle=yes".into(),
        "--keepaspect=yes".into(),
        "--cursor-autohide=no".into(),
        // Keep: egui owns keys.
        "--input-default-bindings=no".into(),
        "--osc=no".into(),
        "--osd-level=0".into(),
        // Config file could set volume/mute low; embedded must be config-independent.
        // build_args skips volume==100 assuming mpv default, so force value explicitly.
        format!("--volume={}", s.mpv_volume),
        "--mute=no".into(),
    ];
    // build_args puts terminal "--" + url last; insert before it so emb flags not treated as files
    if let Some(pos) = args.iter().position(|a| a == "--") {
        args.splice(pos..pos, emb);
    } else {
        args.extend(emb);
    }
    Ok(args)
}

pub fn spawn_embedded(s: &Settings, url: &str, wid: isize) -> Result<MpvHandle, String> {
    let exe = find_mpv(&s.mpv_path).ok_or_else(|| "mpv not found: set Options > mpv Path, place mpv.exe beside webcraft.exe, install mpv, or add it to PATH".to_string())?;
    let pipe = format!(
        "{}-{}",
        ipc_pipe_path(std::process::id()),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let args = embedded_args(s, url, wid, &pipe)?;
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut cmd);
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch mpv ({}): {e}", exe.display()))?;
    let state = Arc::new(Mutex::new(PlayerState {
        volume: 100.0,
        speed: 1.0,
        idle: true,
        ..PlayerState::default()
    }));
    let (tx, rx) = mpsc::channel::<String>();
    let st = state.clone();
    std::thread::spawn(move || ipc_io(pipe, rx, st));
    Ok(MpvHandle {
        state,
        cmd_tx: tx,
        child,
    })
}

fn ipc_io(pipe: String, rx: mpsc::Receiver<String>, state: Arc<Mutex<PlayerState>>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let f = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pipe)
        {
            Ok(f) => break Some(f),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break None,
        }
    };
    let Some(f) = f else {
        if let Ok(mut g) = state.lock() {
            g.error = "mpv IPC pipe connect timeout".into();
            g.idle = true;
            g.eof = true;
        }
        return;
    };
    let Ok(mut wf) = f.try_clone() else {
        if let Ok(mut g) = state.lock() {
            g.error = "mpv IPC pipe clone failed".into();
            g.idle = true;
            g.eof = true;
        }
        return;
    };
    for (id, name) in [
        (1, "pause"),
        (2, "time-pos"),
        (3, "duration"),
        (4, "volume"),
        (5, "speed"),
        (6, "eof-reached"),
        (7, "core-idle"),
        (8, "media-title"),
        (9, "aid"),
        (10, "mute"),
        (11, "paused-for-cache"),
        (12, "path"),
    ] {
        let _ = writeln!(wf, "{{\"command\":[\"observe_property\",{id},\"{name}\"]}}");
    }
    let _ = wf.flush();
    std::thread::spawn(move || {
        let mut w = wf;
        while let Ok(line) = rx.recv() {
            if w.write_all(line.as_bytes()).is_err()
                || w.write_all(b"\n").is_err()
                || w.flush().is_err()
            {
                break;
            }
        }
    });
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if v.get("event").and_then(|e| e.as_str()) != Some("property-change") {
                    continue;
                }
                let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
                if let Ok(mut g) = state.lock() {
                    g.connected = true;
                    match name {
                        "pause" => g.pause = data.as_bool().unwrap_or(false),
                        "time-pos" => g.time_pos = data.as_f64().unwrap_or(0.0),
                        "duration" => g.duration = data.as_f64().unwrap_or(0.0),
                        "volume" => g.volume = data.as_f64().unwrap_or(g.volume),
                        "speed" => g.speed = data.as_f64().unwrap_or(g.speed),
                        "eof-reached" => g.eof = data.as_bool().unwrap_or(false),
                        "core-idle" => g.idle = data.as_bool().unwrap_or(false),
                        "media-title" => g.title = data.as_str().unwrap_or("").to_string(),
                        // JSON null aid (no audio selected) -> -1.
                        "aid" => g.aid = data.as_i64().unwrap_or(-1),
                        "mute" => g.mute = data.as_bool().unwrap_or(false),
                        "paused-for-cache" => g.paused_for_cache = data.as_bool().unwrap_or(false),
                        "path" => g.path = data.as_str().unwrap_or("").to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    if let Ok(mut g) = state.lock() {
        g.idle = true;
        g.eof = true;
        g.connected = false;
    }
}

impl Drop for MpvHandle {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl MpvHandle {
    pub fn cmd(&self, v: serde_json::Value) {
        let _ = self.cmd_tx.send(v.to_string());
    }
    pub fn toggle_pause(&self) {
        self.cmd(serde_json::json!({"command":["cycle","pause"]}));
    }
    pub fn set_pause(&self, p: bool) {
        self.cmd(serde_json::json!({"command":["set_property","pause",p]}));
    }
    pub fn seek_rel(&self, secs: f64) {
        self.cmd(serde_json::json!({"command":["seek",secs,"relative"]}));
    }
    pub fn seek_abs(&self, secs: f64) {
        self.cmd(serde_json::json!({"command":["seek",secs,"absolute"]}));
    }
    pub fn set_volume(&self, v: f64) {
        self.cmd(serde_json::json!({"command":["set_property","volume",v.clamp(0.0, 100.0)]}));
    }
    pub fn set_speed(&self, v: f64) {
        self.cmd(serde_json::json!({"command":["set_property","speed",v]}));
    }
    pub fn loadfile(&self, url: &str) -> Result<(), String> {
        validate_url(url)?;
        self.cmd(serde_json::json!({"command":["loadfile",url,"replace"]}));
        Ok(())
    }
    pub fn stop(&self) {
        self.cmd(serde_json::json!({"command":["stop"]}));
    }
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    pub fn quit(mut self) {
        self.cmd(serde_json::json!({"command":["quit"]}));
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &Settings, url: &str) -> Vec<String> {
        build_args(s, url).unwrap()
    }

    #[test]
    fn defaults_only_cache_args_plus_url() {
        let s = Settings::default();
        assert_eq!(
            args(&s, "https://x.org/v.mp4"),
            vec![
                "--cache=yes",
                "--cache-secs=30",
                "--",
                "https://x.org/v.mp4"
            ]
        );
    }

    #[test]
    fn each_flag_mapping() {
        let mut s = Settings {
            mpv_fullscreen: true,
            mpv_keep_open: true,
            mpv_loop: true,
            mpv_volume: 40,
            mpv_speed: 1.5,
            mpv_start: "01:30".into(),
            mpv_hwdec: "auto-safe".into(),
            mpv_cache: false,
            mpv_cache_secs: 60,
            mpv_sub_auto: "fuzzy".into(),
            mpv_extra_args: "--ontop --mute=yes".into(),
            ..Default::default()
        };
        assert_eq!(
            args(&s, "https://x.org/v.mp4"),
            vec![
                "--fullscreen",
                "--keep-open=yes",
                "--loop-file=inf",
                "--volume=40",
                "--speed=1.5",
                "--start=01:30",
                "--hwdec=auto-safe",
                "--cache=no",
                "--cache-secs=60",
                "--sub-auto=fuzzy",
                "--ontop",
                "--mute=yes",
                "--",
                "https://x.org/v.mp4",
            ]
        );
        // Boundary/default suppression: volume 100 and speed 1.0 emit nothing.
        s.mpv_volume = 100;
        s.mpv_speed = 1.0;
        let a = args(&s, "https://x.org/v.mp4");
        assert!(!a.iter().any(|x| x.starts_with("--volume")));
        assert!(!a.iter().any(|x| x.starts_with("--speed")));
        s.mpv_volume = 0;
        assert!(args(&s, "https://x.org/v.mp4").contains(&"--volume=0".to_string()));
    }

    #[test]
    fn validation_errors() {
        let mut s = Settings {
            mpv_volume: 101,
            ..Default::default()
        };
        assert!(build_args(&s, "https://x.org/v.mp4").is_err());
        s.mpv_volume = 100;
        s.mpv_speed = 0.1;
        assert!(build_args(&s, "https://x.org/v.mp4").is_err());
        s.mpv_speed = 5.0;
        assert!(build_args(&s, "https://x.org/v.mp4").is_err());
        s.mpv_speed = 1.0;
        s.mpv_start = "abc;rm".into();
        assert!(build_args(&s, "https://x.org/v.mp4").is_err());
        s.mpv_start = "10:00".into();
        s.mpv_hwdec = "auto;safe".into();
        assert!(build_args(&s, "https://x.org/v.mp4").is_err());
        s.mpv_hwdec = String::new();
        s.mpv_sub_auto = "fuzzy all".into();
        assert!(build_args(&s, "https://x.org/v.mp4").is_err());
        // Dash-url, unknown scheme, control char rejected; scheme case-insensitive.
        s.mpv_sub_auto = String::new();
        assert!(build_args(&s, "--volume=1").is_err());
        assert!(build_args(&s, "-x").is_err());
        assert!(build_args(&s, "ftp://x.org/v.mp4").is_err());
        assert!(build_args(&s, "https://x/a\u{1}b.mp4").is_err());
        assert!(build_args(&s, "").is_err());
        assert!(build_args(&s, "HTTP://x.org/v.mp4").is_ok());
        assert!(build_args(&s, "Https://x.org/v.mp4").is_ok());
        // Allowed schemes and scheme-less local paths.
        assert!(build_args(&s, "rtmp://x.org/v").is_ok());
        assert!(build_args(&s, "rtsp://h/s").is_ok());
        assert!(build_args(&s, "file:///tmp/a.mp4").is_ok());
        assert!(build_args(&s, "x.org/v.mp4").is_ok());
        assert_eq!(validate_url("C:\\v\\a.mp4").unwrap(), "C:\\v\\a.mp4");
    }

    #[test]
    fn extra_args_split() {
        let s = Settings {
            mpv_extra_args: "  --ontop   --mute=yes  --no-border ".into(),
            ..Default::default()
        };
        let a = args(&s, "https://x.org/v.mp4");
        let dash = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(&a[dash - 3..dash], ["--ontop", "--mute=yes", "--no-border"]);
        assert_eq!(a.last().map(String::as_str), Some("https://x.org/v.mp4"));
    }

    #[test]
    fn player_state_defaults() {
        let st = PlayerState::default();
        assert_eq!(st.aid, -1);
        assert!(!st.mute && !st.paused_for_cache && !st.connected);
        assert!(st.path.is_empty());
    }

    #[test]
    fn pipe_path_format() {
        let p = ipc_pipe_path(42);
        assert!(p.starts_with(r"\\.\pipe\webcraft-mpv-"));
        assert!(p.ends_with("-42"));
    }

    #[test]
    fn embedded_args_last_wins_and_strips_conflicts() {
        let mut s = Settings::default();
        s.mpv_fullscreen = true;
        s.mpv_extra_args =
            "--wid=1 --input-ipc-server=x --really-quiet --force-window --osc=yes --volume=20 --mute=yes --log-file=C:/evil.log"
                .into();
        let a = embedded_args(&s, "https://x/v.mp4", 123, r"\\.\pipe\webcraft-mpv-1-0").unwrap();
        let wid: Vec<_> = a.iter().filter(|x| x.starts_with("--wid")).collect();
        assert_eq!(wid.len(), 1);
        assert_eq!(wid[0], "--wid=123");
        let dash = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[dash - 1], "--mute=no");
        assert_eq!(a.last().unwrap(), "https://x/v.mp4");
        assert!(a.contains(&"--force-window=yes".to_string()));
        assert_eq!(
            a.iter()
                .filter(|x| x.starts_with("--input-ipc-server"))
                .count(),
            1
        );
        assert!(!a.contains(&"--really-quiet".to_string()));
        assert_eq!(a[dash - 2], "--volume=100");
        assert_eq!(a[dash - 3], "--osd-level=0");
        assert_eq!(a[dash - 4], "--osc=no");
        // Forced volume/mute/log-file: single occurrence, last wins over settings/extra args.
        assert_eq!(a.iter().filter(|x| x.starts_with("--volume")).count(), 1);
        assert_eq!(a.iter().filter(|x| x.starts_with("--mute")).count(), 1);
        assert!(a
            .iter()
            .any(|x| x.starts_with("--log-file=") && x.ends_with("mpv-embedded.log")));
        assert_eq!(a.iter().filter(|x| x.starts_with("--log-file")).count(), 1);
    }
}
