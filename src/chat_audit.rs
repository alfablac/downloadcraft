//! Append-only audit trail for built-in chatbot actions/conversations.
//! One JSON object per line (JSONL). Never panics: all I/O best-effort.
//! Secrets redacted before write; detail truncated to 500 chars.
//! Caller wires module with `pub mod chat_audit;` in `src/lib.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const KIND_MESSAGE_SENT: &str = "message_sent";
pub const KIND_REPLY_OK: &str = "reply_ok";
pub const KIND_REPLY_ERROR: &str = "reply_error";
pub const KIND_ACTION_PROPOSED: &str = "action_proposed";
pub const KIND_ACTION_CONFIRMED: &str = "action_confirmed";
pub const KIND_ACTION_DISMISSED: &str = "action_dismissed";
pub const KIND_ACTION_EXECUTED: &str = "action_executed";
pub const KIND_ACTION_FAILED: &str = "action_failed";
pub const KIND_READONLY_EXECUTED: &str = "readonly_executed";
pub const KIND_INVALID_ACTION: &str = "invalid_action";
pub const KIND_LOCAL_CONTEXT_OK: &str = "local_context_ok";
pub const KIND_LOCAL_CONTEXT_FAIL: &str = "local_context_fail";
pub const KIND_SESSION_CLEARED: &str = "session_cleared";
pub const KIND_HISTORY_EXPORTED: &str = "history_exported";

/// Rotate when file grows past this size.
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
/// Alias for `MAX_FILE_BYTES`.
pub const MAX_AUDIT_BYTES: u64 = MAX_FILE_BYTES;
/// Alias for `MAX_FILE_BYTES`.
pub const ROTATE_MAX_BYTES: u64 = MAX_FILE_BYTES;
/// Rotate keeps at most this many newest lines.
pub const MAX_LINES: usize = 2000;
/// Alias for `MAX_LINES`.
pub const MAX_AUDIT_LINES: usize = MAX_LINES;
/// Alias for `MAX_LINES`.
pub const KEEP_LINES: usize = MAX_LINES;
/// `detail` cap (chars, char-boundary safe) applied in `append`.
pub const MAX_DETAIL_CHARS: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts_ms: u64,
    pub session_id: String,
    pub kind: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub latency_ms: u64,
}

impl AuditEvent {
    pub fn new(session_id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            ts_ms: now_ms(),
            session_id: session_id.into(),
            kind: kind.into(),
            ..Self::default()
        }
    }

    /// Alias for `new`.
    pub fn event(session_id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self::new(session_id, kind)
    }

    /// Set all optional fields in one call.
    pub fn with(
        mut self,
        action: impl Into<String>,
        outcome: impl Into<String>,
        detail: impl Into<String>,
        model: impl Into<String>,
        latency_ms: u64,
    ) -> Self {
        self.action = action.into();
        self.outcome = outcome.into();
        self.detail = detail.into();
        self.model = model.into();
        self.latency_ms = latency_ms;
        self
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = action.into();
        self
    }
    pub fn with_outcome(mut self, outcome: impl Into<String>) -> Self {
        self.outcome = outcome.into();
        self
    }
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
    pub fn with_latency_ms(mut self, latency_ms: u64) -> Self {
        self.latency_ms = latency_ms;
        self
    }
}

/// Milliseconds since Unix epoch (0 on clock failure).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// New event stamped with current time.
pub fn event(session_id: &str, kind: &str) -> AuditEvent {
    AuditEvent::new(session_id, kind)
}

/// Alias for `event`.
pub fn new_event(session_id: &str, kind: &str) -> AuditEvent {
    AuditEvent::new(session_id, kind)
}

pub fn audit_path() -> PathBuf {
    crate::platform::config_dir().join("chat-audit.jsonl")
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
            if let Some(pos) = find_ci(b, b"bearer", p) {
                if pos == p {
                    let mut q = p + 6;
                    if q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                        while q < b.len() && (b[q] == b' ' || b[q] == b'\t') {
                            q += 1;
                        }
                        vstart = q;
                    }
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
/// `#fragment` (MEGA decryption keys) preserved.
pub fn redact(text: &str) -> String {
    let s = mask_bearer(text);
    let s = mask_sk(&s);
    let s = mask_userinfo(&s);
    let s = mask_assignments(&s);
    mask_query_params(&s)
}

/// Char-boundary-safe truncation to `max_chars` chars.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------- storage

/// Append one redacted JSON line. Creates parent dirs, best-effort
/// rotates afterwards. Never panics; all errors ignored.
pub fn append(path: &Path, event: &AuditEvent) {
    let mut clean = event.clone();
    clean.action = redact(&event.action);
    clean.detail = truncate_chars(&redact(&event.detail), MAX_DETAIL_CHARS);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let Ok(line) = serde_json::to_string(&clean) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
    rotate(path);
}

/// Append to `audit_path()`.
pub fn append_default(event: &AuditEvent) {
    append(&audit_path(), event);
}

/// Trim to newest `max_lines` lines and, when oversized, to `max_bytes`.
/// No-op when file missing, within both limits, or unreadable.
pub fn rotate_with_limits(path: &Path, max_bytes: u64, max_lines: usize) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    if meta.len() <= max_bytes && lines.len() <= max_lines {
        return; // small file: no-op
    }
    let keep_from = lines.len().saturating_sub(max_lines.max(1));
    let mut kept: Vec<&str> = lines[keep_from..].to_vec();
    if meta.len() > max_bytes {
        // Drop oldest until under byte cap (always keep newest line).
        let mut total: usize = kept.iter().map(|l| l.len() + 1).sum();
        while kept.len() > 1 && total > max_bytes as usize {
            total -= kept[0].len() + 1;
            kept.remove(0);
        }
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = std::fs::write(path, out);
}

/// Rotate with defaults: 512 KiB / newest 2000 lines.
pub fn rotate(path: &Path) {
    rotate_with_limits(path, MAX_FILE_BYTES, MAX_LINES);
}

/// Alias for `rotate`.
pub fn rotate_if_needed(path: &Path) {
    rotate(path);
}

/// Parse all valid lines; bad/missing data skipped, missing file = empty.
pub fn load(path: &Path) -> Vec<AuditEvent> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<AuditEvent>(l).ok())
        .collect()
}

/// Alias for `load`.
pub fn read_all(path: &Path) -> Vec<AuditEvent> {
    load(path)
}

/// Alias for `load`.
pub fn read_events(path: &Path) -> Vec<AuditEvent> {
    load(path)
}

/// Newest `limit` events (file order preserved); `limit == 0` = empty.
pub fn load_recent(path: &Path, limit: usize) -> Vec<AuditEvent> {
    if limit == 0 {
        return Vec::new();
    }
    let all = load(path);
    let from = all.len().saturating_sub(limit);
    all[from..].to_vec()
}

/// Alias for `load_recent`.
pub fn read_recent(path: &Path, limit: usize) -> Vec<AuditEvent> {
    load_recent(path, limit)
}

/// Events for one session (exact match).
pub fn load_session(path: &Path, session_id: &str) -> Vec<AuditEvent> {
    load(path)
        .into_iter()
        .filter(|e| e.session_id == session_id)
        .collect()
}

/// Alias for `load_session`.
pub fn load_for_session(path: &Path, session_id: &str) -> Vec<AuditEvent> {
    load_session(path, session_id)
}

/// Simple `*` wildcard match (any substring per `*`); no `*` = exact.
pub fn matches_kind(kind: &str, pattern: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return kind == pattern;
    }
    // Greedy glob: split on '*', anchor first/last parts.
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = kind;
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if idx == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if idx == parts.len() - 1 && !pattern.ends_with('*') {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(p) => rest = &rest[p + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// Alias for `matches_kind`.
pub fn kind_matches(kind: &str, pattern: &str) -> bool {
    matches_kind(kind, pattern)
}

pub fn filter_session(events: &[AuditEvent], session_id: &str) -> Vec<AuditEvent> {
    events
        .iter()
        .filter(|e| e.session_id == session_id)
        .cloned()
        .collect()
}

pub fn filter_kind(events: &[AuditEvent], kind_pattern: &str) -> Vec<AuditEvent> {
    events
        .iter()
        .filter(|e| matches_kind(&e.kind, kind_pattern))
        .cloned()
        .collect()
}

/// Filter slice: `session_id` empty/`"*"` = any (else exact),
/// `kind_pattern` empty/`"*"` = any (else exact/wildcard).
pub fn filter_events(
    events: &[AuditEvent],
    session_id: &str,
    kind_pattern: &str,
) -> Vec<AuditEvent> {
    events
        .iter()
        .filter(|e| session_id.is_empty() || session_id == "*" || e.session_id == session_id)
        .filter(|e| matches_kind(&e.kind, kind_pattern))
        .cloned()
        .collect()
}

/// Load + filter; newest `limit` of matches (`limit == 0` = all matches).
pub fn query(path: &Path, session_id: &str, kind_pattern: &str, limit: usize) -> Vec<AuditEvent> {
    let filtered = filter_events(&load(path), session_id, kind_pattern);
    if limit == 0 {
        return filtered;
    }
    let from = filtered.len().saturating_sub(limit);
    filtered[from..].to_vec()
}

/// Events whose kind matches pattern (exact or `*` wildcard).
pub fn load_kind(path: &Path, kind_pattern: &str) -> Vec<AuditEvent> {
    filter_kind(&load(path), kind_pattern)
}

/// One `kind: n` line per kind, sorted, plus total.
pub fn summary(events: &[AuditEvent]) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for e in events {
        *counts.entry(e.kind.as_str()).or_default() += 1;
    }
    let mut out: Vec<String> = counts.iter().map(|(k, n)| format!("{k}: {n}")).collect();
    out.push(format!("total: {}", events.len()));
    out.join("\n")
}

pub fn count(path: &Path) -> usize {
    load(path).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "webcraft-audit-{}-{name}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn constants_and_builder() {
        assert_eq!(KIND_MESSAGE_SENT, "message_sent");
        assert_eq!(KIND_REPLY_OK, "reply_ok");
        assert_eq!(KIND_REPLY_ERROR, "reply_error");
        assert_eq!(KIND_ACTION_PROPOSED, "action_proposed");
        assert_eq!(KIND_ACTION_CONFIRMED, "action_confirmed");
        assert_eq!(KIND_ACTION_DISMISSED, "action_dismissed");
        assert_eq!(KIND_ACTION_EXECUTED, "action_executed");
        assert_eq!(KIND_ACTION_FAILED, "action_failed");
        assert_eq!(KIND_READONLY_EXECUTED, "readonly_executed");
        assert_eq!(KIND_INVALID_ACTION, "invalid_action");
        assert_eq!(KIND_LOCAL_CONTEXT_OK, "local_context_ok");
        assert_eq!(KIND_LOCAL_CONTEXT_FAIL, "local_context_fail");
        assert_eq!(KIND_SESSION_CLEARED, "session_cleared");
        assert_eq!(KIND_HISTORY_EXPORTED, "history_exported");
        let e = event("s1", KIND_ACTION_EXECUTED).with("add_download", "ok", "d", "m", 42);
        assert_eq!(e.session_id, "s1");
        assert_eq!(e.action, "add_download");
        assert_eq!(e.latency_ms, 42);
        assert!(e.ts_ms > 0);
        // Serde defaults for omitted optional fields.
        let e2: AuditEvent =
            serde_json::from_str(r#"{"ts_ms":1,"session_id":"s","kind":"reply_ok"}"#).unwrap();
        assert_eq!(e2.action, "");
        assert_eq!(e2.latency_ms, 0);
        assert!(audit_path().ends_with("chat-audit.jsonl"));
    }

    #[test]
    fn redact_masks_secrets_keeps_mega_fragment() {
        let r = redact("key sk-abcdef1234567890 done Bearer tok123 password: hunter2 api_key=zzz");
        assert!(!r.contains("abcdef1234567890"), "{r}");
        assert!(!r.contains("tok123"), "{r}");
        assert!(!r.contains("hunter2"), "{r}");
        assert!(!r.contains("zzz"), "{r}");
        assert!(r.contains("sk-***"), "{r}");
        assert!(r.contains("Bearer ***"), "{r}");
        let mega = "add https://mega.nz/file/dXQT#ENgUuZqYxmxUWK5tB7ei-mhSurDLjtVzxAEvNdI1GzM";
        assert_eq!(redact(mega), mega);
        let u = redact("https://x/f.zip?token=secret&foo=bar&sig=abc#frag");
        assert!(!u.contains("secret") && !u.contains("abc"), "{u}");
        assert!(u.contains("foo=bar") && u.contains("#frag"), "{u}");
        let j = redact(r#"{"api_key":"supersecret","note":"ok"}"#);
        assert!(!j.contains("supersecret") && j.contains("ok"), "{j}");
        assert_eq!(
            redact("Authorization: Bearer abc123"),
            "Authorization: Bearer ***"
        );
        let ui = redact("https://user:passw0rd@h/f.zip");
        assert!(!ui.contains("passw0rd"), "{ui}");
    }

    #[test]
    fn append_load_filter_roundtrip() {
        let path = tmp("roundtrip").join("nested").join("a.jsonl");
        append(
            &path,
            &event("s1", KIND_MESSAGE_SENT).with("x", "", "token=leakme", "", 0),
        );
        append(
            &path,
            &event("s1", KIND_REPLY_OK).with("", "ok", "fine", "m", 7),
        );
        append(&path, &event("s2", KIND_ACTION_FAILED));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("leakme"), "{raw}");
        assert_eq!(raw.lines().count(), 3);
        let all = load(&path);
        assert_eq!(all.len(), 3);
        assert_eq!(load_session(&path, "s1").len(), 2);
        assert_eq!(load_recent(&path, 2).len(), 2);
        assert_eq!(load_recent(&path, 2)[0].kind, KIND_REPLY_OK);
        assert_eq!(query(&path, "s1", "reply_*", 0).len(), 1);
        assert_eq!(query(&path, "*", KIND_ACTION_FAILED, 0).len(), 1);
        assert!(matches_kind("action_executed", "action_*"));
        assert!(matches_kind("action_executed", "action_executed"));
        assert!(!matches_kind("action_executed", "reply_*"));
        assert_eq!(load(&tmp("missing")), Vec::<AuditEvent>::new());
        // Bad lines skipped.
        std::fs::write(
            &path,
            "not json\n{\"ts_ms\":1,\"session_id\":\"s\",\"kind\":\"reply_ok\"}\n",
        )
        .unwrap();
        assert_eq!(load(&path).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn append_truncates_detail_char_boundary() {
        let path = tmp("trunc");
        let long = "é".repeat(600); // 2-byte chars
        append(
            &path,
            &event("s", KIND_MESSAGE_SENT).with("", "", &long, "", 0),
        );
        let all = load(&path);
        assert_eq!(all[0].detail.chars().count(), 500);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rotate_small_noop_and_big_keeps_last() {
        let path = tmp("rotate-small");
        append(&path, &event("s", KIND_MESSAGE_SENT));
        let before = std::fs::read_to_string(&path).unwrap();
        rotate(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let big = tmp("rotate-big");
        let line = serde_json::to_string(&event("s", KIND_MESSAGE_SENT)).unwrap();
        let mut content = String::new();
        for i in 0..3000 {
            content.push_str(&line.replace(KIND_MESSAGE_SENT, &format!("k{i}")));
            // Pad past 512 KiB total.
            content.push_str(&" ".repeat(150));
            content.push('\n');
        }
        assert!(content.len() > MAX_FILE_BYTES as usize);
        std::fs::write(&big, &content).unwrap();
        rotate(&big);
        let after = std::fs::read_to_string(&big).unwrap();
        assert!(
            after.lines().count() <= MAX_LINES,
            "{}",
            after.lines().count()
        );
        assert!(after.contains("\"kind\":\"k2999\""), "newest kept");
        assert!(!after.contains("\"kind\":\"k0\""), "oldest dropped");
        let _ = std::fs::remove_file(&big);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn summary_counts_sorted_with_total() {
        let ev = vec![
            event("s", KIND_REPLY_OK),
            event("s", KIND_MESSAGE_SENT),
            event("s", KIND_REPLY_OK),
        ];
        assert_eq!(summary(&ev), "message_sent: 1\nreply_ok: 2\ntotal: 3");
        assert_eq!(summary(&[]), "total: 0");
    }
}
