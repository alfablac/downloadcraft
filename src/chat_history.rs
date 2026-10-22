//! Persistent chat history store for built-in chatbot.
//! Caller wires module with `pub mod chat_history;` in `src/lib.rs`.
//! Secrets redacted on save and load; history pruned to bounded size.
//! Never panics on I/O or parse: missing/corrupt file loads as empty.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MAX_SESSIONS: usize = 20;
pub const MAX_MESSAGES_PER_SESSION: usize = 200;
pub const MAX_MESSAGE_CHARS: usize = 8000;
pub const MAX_TITLE_CHARS: usize = 48;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredMsg {
    #[serde(default)]
    pub user: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub ts_ms: u64,
    #[serde(default)]
    pub model: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatSession {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub messages: Vec<StoredMsg>,
}

pub fn history_path() -> PathBuf {
    crate::platform::config_dir().join("chat-history.json")
}

pub fn load(path: &Path) -> Vec<ChatSession> {
    let Ok(txt) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut sessions: Vec<ChatSession> = serde_json::from_str(&txt).unwrap_or_default();
    redact_all(&mut sessions);
    prune(&mut sessions);
    sessions
}

/// Atomic-ish save like `main.rs::save_state`: write `.tmp` sibling,
/// remove dest, rename; fall back to direct write on any failure.
pub fn save(path: &Path, sessions: &[ChatSession]) -> io::Result<()> {
    let mut clean = sessions.to_vec();
    redact_all(&mut clean);
    prune(&mut clean);
    let txt = serde_json::to_string_pretty(&clean)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    let replaced = std::fs::write(&tmp, &txt).is_ok() && {
        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path).is_ok()
    };
    if !replaced {
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(path, txt)?;
    }
    Ok(())
}

fn redact_all(sessions: &mut [ChatSession]) {
    for s in sessions.iter_mut() {
        for m in s.messages.iter_mut() {
            m.text = redact_secrets(&m.text);
        }
    }
}

/// New empty session; id = `now_ms` hex (model::new_id style, self-contained).
pub fn new_session(now_ms: u64) -> ChatSession {
    ChatSession {
        id: format!("{now_ms:x}"),
        title: "New conversation".to_string(),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        messages: Vec::new(),
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Case-insensitive substring search: (session_id, message_index, snippet).
pub fn search<'a>(sessions: &'a [ChatSession], query: &str) -> Vec<(&'a str, usize, String)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for s in sessions {
        for (i, m) in s.messages.iter().enumerate() {
            let lower = m.text.to_lowercase();
            if let Some(pos) = lower.find(&q) {
                let start = lower[..pos].chars().count().saturating_sub(40);
                let snippet: String = m
                    .text
                    .chars()
                    .skip(start)
                    .take(120)
                    .collect::<String>()
                    .replace(['\n', '\r'], " ");
                out.push((s.id.as_str(), i, snippet));
            }
        }
    }
    out
}

/// Markdown export, secrets redacted.
pub fn export_markdown(session: &ChatSession) -> String {
    let mut out = format!("# {}\n", redact_secrets(&session.title));
    for m in &session.messages {
        out.push_str(&format!(
            "\n## {} — {}\n\n{}\n",
            if m.user { "You" } else { "Assistant" },
            m.ts_ms,
            redact_secrets(&m.text)
        ));
    }
    out
}

/// Title from first user text: first line, whitespace collapsed,
/// max 48 chars with ellipsis; fallback `New conversation`.
pub fn derive_title(first_user_text: &str) -> String {
    let first = first_user_text.lines().next().unwrap_or("");
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "New conversation".to_string();
    }
    if collapsed.chars().count() <= MAX_TITLE_CHARS {
        return collapsed;
    }
    let keep: String = collapsed
        .chars()
        .take(MAX_TITLE_CHARS.saturating_sub(1))
        .collect();
    format!("{keep}…")
}

/// Cap 20 sessions (most recently updated first), 200 newest
/// messages/session, each message text at 8000 chars (char-boundary safe).
pub fn prune(sessions: &mut Vec<ChatSession>) {
    sessions.sort_by_key(|a| std::cmp::Reverse(a.updated_at_ms));
    sessions.truncate(MAX_SESSIONS);
    for s in sessions.iter_mut() {
        if s.messages.len() > MAX_MESSAGES_PER_SESSION {
            let drop = s.messages.len() - MAX_MESSAGES_PER_SESSION;
            s.messages.drain(..drop);
        }
        for m in s.messages.iter_mut() {
            if m.text.chars().count() > MAX_MESSAGE_CHARS {
                m.text = m.text.chars().take(MAX_MESSAGE_CHARS).collect();
            }
        }
    }
}

// ---------------------------------------------------------------- redact

const ASSIGN_KEYS: &[&str] = &[
    "x-api-key",
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "access-token",
    "refresh_token",
    "refresh-token",
    "id_token",
    "auth_token",
    "session_token",
    "client_secret",
    "app_secret",
    "secret_key",
    "private_key",
    "authorization",
    "password",
    "passwd",
    "secret",
    "token",
    "pwd",
];

fn norm_key(k: &str) -> String {
    k.trim()
        .trim_matches(['"', '\''])
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn is_sensitive_key(k: &str) -> bool {
    let n = norm_key(k);
    matches!(
        n.as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "session_token"
            | "api_key"
            | "apikey"
            | "x_api_key"
            | "password"
            | "passwd"
            | "pwd"
            | "secret"
            | "client_secret"
            | "app_secret"
            | "secret_key"
            | "private_key"
            | "authorization"
            | "auth"
            | "key"
            | "sig"
            | "signature"
    ) || n.ends_with("_token")
        || n.ends_with("_secret")
        || n.ends_with("_password")
        || n.ends_with("_api_key")
}

fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn is_value_delim(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t'
            | b'\r'
            | b'\n'
            | b'&'
            | b'#'
            | b'"'
            | b'\''
            | b','
            | b';'
            | b')'
            | b']'
            | b'}'
            | b'<'
            | b'>'
    )
}

fn mask_bearer(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = find_ci(b, b"bearer", i) {
        let before_ok = pos == 0 || !b[pos - 1].is_ascii_alphanumeric();
        let after = pos + 6;
        let sep_ok = after < b.len() && (b[after] == b' ' || b[after] == b'\t');
        if !before_ok || !sep_ok {
            out.push_str(&text[i..after.min(text.len())]);
            i = after;
            continue;
        }
        let mut j = after;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let mut k = j;
        while k < b.len() && !is_value_delim(b[k]) {
            k += 1;
        }
        if k == j {
            out.push_str(&text[i..after]);
            i = after;
            continue;
        }
        out.push_str(&text[i..j]);
        out.push_str("***");
        i = k;
    }
    out.push_str(&text[i..]);
    out
}

fn mask_sk(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = find_ci(b, b"sk-", i) {
        let before_ok = pos == 0 || !b[pos - 1].is_ascii_alphanumeric();
        let mut k = pos + 3;
        while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'-' || b[k] == b'_') {
            k += 1;
        }
        if !before_ok || k == pos + 3 {
            let end = (pos + 3).min(text.len());
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        out.push_str(&text[i..pos + 3]);
        out.push_str("***");
        i = k;
    }
    out.push_str(&text[i..]);
    out
}

fn mask_userinfo(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = find_ci(b, b"://", i) {
        let start = pos + 3;
        // '@' must come before any path/query/fragment/space delimiter.
        let mut at = None;
        let mut j = start;
        while j < b.len() {
            if b[j] == b'@' {
                at = Some(j);
                break;
            }
            if matches!(b[j], b'/' | b'?' | b'#' | b' ' | b'\t' | b'"' | b'\'') {
                break;
            }
            j += 1;
        }
        let Some(at) = at else {
            out.push_str(&text[i..start.min(text.len())]);
            i = start;
            continue;
        };
        let Some(colon) = (start..at).find(|&x| b[x] == b':') else {
            out.push_str(&text[i..start]);
            i = start;
            continue;
        };
        out.push_str(&text[i..colon + 1]);
        out.push_str("***");
        i = at;
    }
    out.push_str(&text[i..]);
    out
}

/// Mask `key: value` / `key=value` (incl. quoted JSON forms) for secret keys.
fn mask_assignments(text: &str) -> String {
    let b = text.as_bytes();
    let mut keys: Vec<&str> = ASSIGN_KEYS.to_vec();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'outer: while i < b.len() {
        for key in &keys {
            let kb = key.as_bytes();
            if i + kb.len() > b.len() || !b[i..i + kb.len()].eq_ignore_ascii_case(kb) {
                continue;
            }
            let before_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            if !before_ok {
                continue;
            }
            let mut j = i + kb.len();
            // Optional closing quote of a JSON key: "api_key": ...
            if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                j += 1;
            }
            let mut p = j;
            while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                p += 1;
            }
            if p >= b.len() || (b[p] != b':' && b[p] != b'=') {
                continue;
            }
            p += 1;
            while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                p += 1;
            }
            let quote = if p < b.len() && (b[p] == b'"' || b[p] == b'\'') {
                let q = b[p];
                p += 1;
                Some(q)
            } else {
                None
            };
            // `Authorization: Bearer <tok>`: keep Bearer, mask token only.
            let mut vstart = p;
            if find_ci(b, b"bearer", p) == Some(p) {
                let mut q = p + 6;
                if q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                    while q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                        q += 1;
                    }
                    vstart = q;
                }
            }
            let mut vend = vstart;
            if let Some(q) = quote {
                while vend < b.len() && b[vend] != q {
                    vend += 1;
                }
            } else {
                while vend < b.len() && !is_value_delim(b[vend]) {
                    vend += 1;
                }
            }
            if vend == vstart {
                continue;
            }
            out.push_str(&text[i..vstart]);
            out.push_str("***");
            i = vend;
            continue 'outer;
        }
        // Push one char (UTF-8 safe step).
        let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Mask sensitive query-param values only; fragment (`#...`, MEGA keys) kept.
fn mask_query_params(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'?' {
            let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        let mut end = i + 1;
        while end < b.len()
            && !matches!(
                b[end],
                b'#' | b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'' | b'<' | b'>'
            )
        {
            end += 1;
        }
        out.push('?');
        for (n, part) in text[i + 1..end].split('&').enumerate() {
            if n > 0 {
                out.push('&');
            }
            match part.split_once('=') {
                Some((k, _v)) if is_sensitive_key(k) => {
                    out.push_str(k);
                    out.push_str("=***");
                }
                _ => out.push_str(part),
            }
        }
        i = end;
    }
    out
}

/// Mask secrets: `sk-…`, `Bearer …`, `token/password/api_key/secret`
/// assignments and URL query values. Non-sensitive query params and
/// `#fragment` (MEGA decryption keys) preserved. Pure, linear scans.
pub fn redact_secrets(text: &str) -> String {
    let s = mask_bearer(text);
    let s = mask_sk(&s);
    let s = mask_userinfo(&s);
    let s = mask_assignments(&s);
    mask_query_params(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "webcraft-chat-history-{}-{name}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn msg(text: &str) -> StoredMsg {
        StoredMsg {
            user: true,
            text: text.to_string(),
            ts_ms: 1,
            model: String::new(),
        }
    }

    #[test]
    fn serde_forward_compat_defaults() {
        let m: StoredMsg = serde_json::from_str(r#"{"user":true}"#).unwrap();
        assert_eq!(
            m,
            StoredMsg {
                user: true,
                text: String::new(),
                ts_ms: 0,
                model: String::new()
            }
        );
        let s: ChatSession = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(s, ChatSession::default());
        assert!(history_path().ends_with("chat-history.json"));
    }

    #[test]
    fn new_session_and_title() {
        let s = new_session(0x1234);
        assert_eq!(s.id, "1234");
        assert_eq!(s.created_at_ms, 0x1234);
        assert_eq!(s.updated_at_ms, 0x1234);
        assert_eq!(s.title, "New conversation");
        assert!(s.messages.is_empty());
        assert_eq!(derive_title(""), "New conversation");
        assert_eq!(derive_title("  \n second line"), "New conversation");
        assert_eq!(derive_title("hello   world\nnext line"), "hello world");
        let long = "word ".repeat(30);
        let t = derive_title(&long);
        assert_eq!(t.chars().count(), MAX_TITLE_CHARS, "{t}");
        assert!(t.ends_with('…'), "{t}");
        // Char-boundary: multibyte chars not split.
        let t = derive_title(&"é".repeat(60));
        assert_eq!(t.chars().count(), MAX_TITLE_CHARS, "{t}");
    }

    #[test]
    fn redact_masks_secrets_keeps_mega_fragment_and_plain_params() {
        let r = redact_secrets(
            "key sk-abcdef1234567890 done Bearer tok123 password: hunter2 api_key=zzz",
        );
        assert!(!r.contains("abcdef1234567890"), "{r}");
        assert!(!r.contains("tok123"), "{r}");
        assert!(!r.contains("hunter2"), "{r}");
        assert!(!r.contains("zzz"), "{r}");
        assert!(r.contains("sk-***"), "{r}");
        assert!(r.contains("Bearer ***"), "{r}");
        let mega = "add https://mega.nz/file/dXQT#ENgUuZqYxmxUWK5tB7ei-mhSurDLjtVzxAEvNdI1GzM";
        assert_eq!(redact_secrets(mega), mega);
        let u = redact_secrets("https://x/f.zip?token=secret&foo=bar&sig=abc#frag");
        assert!(!u.contains("secret") && !u.contains("abc"), "{u}");
        assert!(u.contains("foo=bar") && u.contains("#frag"), "{u}");
        let u2 = redact_secrets("https://x/f.zip?Token=SECRET&API_KEY=k2&page=2");
        assert!(!u2.contains("SECRET") && !u2.contains("k2"), "{u2}");
        assert!(u2.contains("page=2"), "{u2}");
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=name";
        assert_eq!(redact_secrets(magnet), magnet);
        let j = redact_secrets(r#"{"api_key":"supersecret","note":"ok"}"#);
        assert!(!j.contains("supersecret") && j.contains("ok"), "{j}");
        assert_eq!(
            redact_secrets("Authorization: Bearer abc123"),
            "Authorization: Bearer ***"
        );
        assert_eq!(redact_secrets("bearer abc123"), "bearer ***");
        let ui = redact_secrets("https://user:passw0rd@h/f.zip");
        assert!(!ui.contains("passw0rd"), "{ui}");
        assert_eq!(redact_secrets("hello world"), "hello world");
        assert_eq!(redact_secrets(""), "");
    }

    #[test]
    fn save_load_roundtrip_redacts_and_missing_corrupt_empty() {
        let path = tmp("roundtrip").join("nested").join("h.json");
        let mut s = new_session(100);
        s.title = derive_title("first question");
        s.messages = vec![msg("hi sk-abcdef1234567890"), msg("plain reply")];
        save(&path, &[s.clone()]).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("abcdef1234567890"), "{raw}");
        let loaded = load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, s.id);
        assert_eq!(loaded[0].messages.len(), 2);
        assert_eq!(loaded[0].messages[0].text, "hi sk-***");
        assert_eq!(loaded[0].messages[1], s.messages[1]);

        assert_eq!(load(&tmp("missing")), Vec::<ChatSession>::new());
        let bad = tmp("corrupt");
        std::fs::write(&bad, "not json {").unwrap();
        assert_eq!(load(&bad), Vec::<ChatSession>::new());
        std::fs::write(&bad, r#"{"not":"a vec"}"#).unwrap();
        assert_eq!(load(&bad), Vec::<ChatSession>::new());
        let _ = std::fs::remove_file(&bad);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn prune_caps_sessions_messages_and_text() {
        let mut sessions: Vec<ChatSession> = (0..25u64)
            .map(|i| {
                let mut s = new_session(i);
                s.updated_at_ms = i;
                s.messages = (0..210).map(|j| msg(&format!("m{j}"))).collect();
                s.messages.push(StoredMsg {
                    text: "é".repeat(9000),
                    ..StoredMsg::default()
                });
                s
            })
            .collect();
        prune(&mut sessions);
        assert_eq!(sessions.len(), MAX_SESSIONS);
        // Most recently updated kept, newest first.
        assert_eq!(sessions[0].updated_at_ms, 24);
        assert_eq!(sessions[MAX_SESSIONS - 1].updated_at_ms, 5);
        for s in &sessions {
            assert_eq!(s.messages.len(), MAX_MESSAGES_PER_SESSION);
        }
        // Newest messages kept (m0..m10 dropped from front; long last msg capped).
        assert_eq!(sessions[0].messages[0].text, "m11");
        let last = &sessions[0].messages[MAX_MESSAGES_PER_SESSION - 1];
        assert_eq!(last.text.chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn search_and_export() {
        let mut s = new_session(1);
        s.messages = vec![
            StoredMsg {
                user: true,
                text: "clima em Xique-Xique".into(),
                ..Default::default()
            },
            StoredMsg {
                text: "token=abc123 sunny".into(),
                ..Default::default()
            },
        ];
        let binding = [s.clone()];
        let hits = search(&binding, "XIQUE");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 0);
        assert!(search(&binding, "").is_empty());
        let md = export_markdown(&s);
        assert!(md.contains("clima em Xique-Xique"));
        assert!(!md.contains("abc123"), "{md}");
    }
}
