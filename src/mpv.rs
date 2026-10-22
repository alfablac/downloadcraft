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

/// "#RRGGBB" hex color (subtitle style fields).
pub fn is_hex_color(v: &str) -> bool {
    v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// One audio/subtitle/video track from mpv `track-list` property.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: i64,
    pub kind: String, // "audio" | "sub" | "video"
    pub title: String,
    pub lang: String,
    pub codec: String,
    pub selected: bool,
    pub external_filename: String,
}

impl TrackInfo {
    /// Combo label: lang/title/codec, fallback "Track <id>".
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.lang.is_empty() {
            parts.push(&self.lang);
        }
        if !self.title.is_empty() && self.title != self.lang {
            parts.push(&self.title);
        }
        if parts.is_empty() && !self.codec.is_empty() {
            parts.push(&self.codec);
        }
        if !self.external_filename.is_empty() {
            parts.push(&self.external_filename);
        }
        if parts.is_empty() {
            format!("Track {}", self.id)
        } else {
            parts.join(" · ")
        }
    }
}

/// Parse mpv `track-list` JSON value into tracks (pure, tested).
pub fn parse_tracks(data: &serde_json::Value) -> Vec<TrackInfo> {
    let Some(arr) = data.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            Some(TrackInfo {
                id: t.get("id")?.as_i64()?,
                kind: t.get("type")?.as_str()?.to_string(),
                title: t
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                lang: t
                    .get("lang")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                codec: t
                    .get("codec")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                selected: t.get("selected").and_then(|v| v.as_bool()).unwrap_or(false),
                external_filename: t
                    .get("external-filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
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
    // Subtitle style: only non-default values emitted (mpv defaults kept).
    if s.mpv_sub_size != 55 {
        if !(1..=200).contains(&s.mpv_sub_size) {
            return Err(format!(
                "mpv subtitle size out of range (1..=200): {}",
                s.mpv_sub_size
            ));
        }
        a.push(format!("--sub-font-size={}", s.mpv_sub_size));
    }
    if s.mpv_sub_pos != 100 {
        if s.mpv_sub_pos > 100 {
            return Err(format!(
                "mpv subtitle position out of range (0..=100): {}",
                s.mpv_sub_pos
            ));
        }
        a.push(format!("--sub-pos={}", s.mpv_sub_pos));
    }
    for (val, flag, def) in [
        (&s.mpv_sub_color, "--sub-color", "#FFFFFF"),
        (&s.mpv_sub_shadow_color, "--sub-back-color", "#000000"),
    ] {
        let v = val.trim();
        if is_hex_color(v) && !v.eq_ignore_ascii_case(def) {
            a.push(format!("{flag}={v}"));
        }
    }
    if s.mpv_sub_bold {
        a.push("--sub-bold=yes".into());
    }
    if s.mpv_sub_ass_override {
        a.push("--sub-ass-override=yes".into());
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
    pub use super::{
        embedded_args, embedded_args_h, header_fields, ipc_pipe_path, spawn_embedded,
        spawn_embedded_h, MpvHandle, PlayerState,
    };
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
    pub sid: i64,      // -1 = no subtitle track selected / unknown
    /// All tracks (audio/sub/video) from `track-list`.
    pub tracks: Vec<TrackInfo>,
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
            sid: -1,
            tracks: Vec::new(),
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

/// mpv `http-header-fields` value from (name, value) pairs. mpv format:
/// comma-separated "Name: value". Reject CR/LF/comma breaking format
/// (values with those chars cannot nest); empty = no arg.
pub fn header_fields(headers: &[(String, String)]) -> Result<Option<String>, String> {
    if headers.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for (n, v) in headers {
        if n.is_empty() || n.chars().any(|c| c.is_control() || c == ':' || c == ',') {
            return Err(format!("invalid stream header name: {n}"));
        }
        if v.chars().any(|c| c.is_control() || c == ',') {
            return Err(format!("invalid stream header value for {n}"));
        }
        parts.push(format!("{n}: {v}"));
    }
    Ok(Some(parts.join(",")))
}

pub fn embedded_args(
    s: &Settings,
    url: &str,
    wid: isize,
    pipe: &str,
) -> Result<Vec<String>, String> {
    embedded_args_h(s, url, wid, pipe, &[])
}

pub fn embedded_args_h(
    s: &Settings,
    url: &str,
    wid: isize,
    pipe: &str,
    headers: &[(String, String)],
) -> Result<Vec<String>, String> {
    let header_arg = header_fields(headers)?
        .map(|h| format!("--http-header-fields={h}"));
    let mut args = build_args(s, url)?;
    if let Some(h) = header_arg {
        args.insert(1, h);
    }
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

pub fn spawn_embedded_h(
    s: &Settings,
    url: &str,
    wid: isize,
    headers: &[(String, String)],
) -> Result<MpvHandle, String> {
    spawn_embedded_impl(s, url, wid, headers)
}

pub fn spawn_embedded(s: &Settings, url: &str, wid: isize) -> Result<MpvHandle, String> {
    spawn_embedded_impl(s, url, wid, &[])
}

fn spawn_embedded_impl(
    s: &Settings,
    url: &str,
    wid: isize,
    headers: &[(String, String)],
) -> Result<MpvHandle, String> {
    let exe = find_mpv(&s.mpv_path).ok_or_else(|| "mpv not found: set Options > mpv Path, place mpv.exe beside webcraft.exe, install mpv, or add it to PATH".to_string())?;
    let pipe = format!(
        "{}-{}",
        ipc_pipe_path(std::process::id()),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let args = embedded_args_h(s, url, wid, &pipe, headers)?;
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
        (13, "sid"),
        (14, "track-list"),
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
                        "sid" => g.sid = data.as_i64().unwrap_or(-1),
                        "track-list" => g.tracks = parse_tracks(&data),
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
    /// Switch audio track (`aid` property).
    pub fn set_aid(&self, id: i64) {
        self.cmd(serde_json::json!({"command":["set_property","aid",id]}));
    }
    /// Switch subtitle track; None = off.
    pub fn set_sid(&self, id: Option<i64>) {
        match id {
            Some(id) => self.cmd(serde_json::json!({"command":["set_property","sid",id]})),
            None => self.cmd(serde_json::json!({"command":["set_property","sid","no"]})),
        }
    }
    /// Set extra HTTP headers for next/current stream loads.
    pub fn set_http_headers(&self, headers: &[(String, String)]) {
        if let Ok(Some(v)) = header_fields(headers) {
            self.cmd(serde_json::json!({"command":["set_property","http-header-fields",v]}));
        } else if headers.is_empty() {
            self.cmd(serde_json::json!({"command":["set_property","http-header-fields",""]}));
        }
    }
    /// Load file with per-file HTTP headers (headers set before load so
    /// first request carries auth; headerless file clears old auth).
    pub fn loadfile_h(&self, url: &str, headers: &[(String, String)]) -> Result<(), String> {
        self.set_http_headers(headers);
        self.loadfile(url)
    }
    /// Load external subtitle file live (`sub-add ... select`).
    pub fn sub_add(&self, path: &str) {
        self.cmd(serde_json::json!({"command":["sub-add",path,"select"]}));
    }
    /// Live subtitle style property (`sub-font-size`, `sub-color`, ...).
    pub fn set_sub_prop(&self, name: &str, value: serde_json::Value) {
        self.cmd(serde_json::json!({"command":["set_property",name,value]}));
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
    fn parse_tracks_filters_and_labels() {
        let v = serde_json::json!([
            {"id":1,"type":"video","codec":"h264","selected":true},
            {"id":2,"type":"audio","lang":"eng","title":"Stereo","codec":"aac","selected":true},
            {"id":3,"type":"audio","lang":"jpn","codec":"flac"},
            {"id":4,"type":"sub","lang":"eng","title":"Full","codec":"ass"},
            {"id":5,"type":"sub","external-filename":"movie.en.srt"},
            {"type":"audio","lang":"bad-no-id"}
        ]);
        let t = parse_tracks(&v);
        assert_eq!(t.len(), 5);
        assert_eq!(t[1].kind, "audio");
        assert_eq!(t[1].label(), "eng · Stereo");
        assert_eq!(t[3].label(), "eng · Full");
        assert_eq!(t[4].label(), "movie.en.srt");
        assert_eq!(t[0].label(), "h264");
        assert_eq!(parse_tracks(&serde_json::json!(null)).len(), 0);
        assert_eq!(parse_tracks(&serde_json::json!("x")).len(), 0);
    }

    #[test]
    fn header_fields_joins_and_rejects_breakers() {
        assert_eq!(header_fields(&[]), Ok(None));
        let h = vec![
            ("Authorization".to_string(), "Bearer tok".to_string()),
            ("X-Api-Key".to_string(), "k1".to_string()),
        ];
        assert_eq!(
            header_fields(&h),
            Ok(Some("Authorization: Bearer tok,X-Api-Key: k1".into()))
        );
        assert!(header_fields(&[("Bad,Name".into(), "v".into())]).is_err());
        assert!(header_fields(&[("X".into(), "a\nb".into())]).is_err());
        assert!(header_fields(&[("X".into(), "a,b".into())]).is_err());
    }

    #[test]
    fn embedded_args_carry_stream_headers() {
        let s = Settings::default();
        let h = vec![("Authorization".to_string(), "Bearer tok".to_string())];
        let a = embedded_args_h(&s, "https://x/v.mkv", 7, "pipe", &h).unwrap();
        assert!(a.contains(&"--http-header-fields=Authorization: Bearer tok".to_string()));
        let a0 = embedded_args(&s, "https://x/v.mkv", 7, "pipe").unwrap();
        assert!(!a0.iter().any(|x| x.starts_with("--http-header-fields")));
    }

    #[test]
    fn build_args_sub_style_only_when_non_default() {
        let s = Settings::default();
        let a = build_args(&s, "http://x/v.mkv").unwrap();
        for f in [
            "--sub-font-size",
            "--sub-pos",
            "--sub-color",
            "--sub-back-color",
            "--sub-bold",
            "--sub-ass-override",
        ] {
            assert!(
                !a.iter().any(|x| x.starts_with(f)),
                "unexpected {f} in {a:?}"
            );
        }
        let s = Settings {
            mpv_sub_size: 70,
            mpv_sub_pos: 90,
            mpv_sub_color: "#FFCC00".into(),
            mpv_sub_shadow_color: "#112233".into(),
            mpv_sub_bold: true,
            mpv_sub_ass_override: true,
            ..Default::default()
        };
        let a = build_args(&s, "http://x/v.mkv").unwrap();
        for f in [
            "--sub-font-size=70",
            "--sub-pos=90",
            "--sub-color=#FFCC00",
            "--sub-back-color=#112233",
            "--sub-bold=yes",
            "--sub-ass-override=yes",
        ] {
            assert!(a.contains(&f.to_string()), "missing {f} in {a:?}");
        }
        // Default text color suppressed, changed shadow emitted.
        let s2 = Settings {
            mpv_sub_shadow_color: "#445566".into(),
            ..Default::default()
        };
        let a2 = build_args(&s2, "http://x/v.mkv").unwrap();
        assert!(!a2.iter().any(|x| x.starts_with("--sub-color")));
        assert!(a2.contains(&"--sub-back-color=#445566".to_string()));
        // Bad ranges rejected.
        let s3 = Settings {
            mpv_sub_size: 999,
            ..Default::default()
        };
        assert!(build_args(&s3, "http://x/v.mkv").is_err());
        let s3 = Settings {
            mpv_sub_pos: 101,
            ..Default::default()
        };
        assert!(build_args(&s3, "http://x/v.mkv").is_err());
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
        let mut s = Settings {
            mpv_fullscreen: true,
            ..Default::default()
        };
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
