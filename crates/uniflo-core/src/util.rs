//! Small helpers shared by adapters: timestamps, text extraction, home paths.

use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub fn home() -> PathBuf {
    std::env::var_os("UNIFLO_HOME").map(PathBuf::from).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("/"))
}

/// Parse an RFC 3339 string, or a unix number in seconds / milliseconds, to epoch ms.
pub fn ts(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => ts_str(s),
        Value::Number(n) => {
            let f = n.as_f64()?;
            Some(if f > 1e11 { f as i64 } else { (f * 1000.0) as i64 })
        }
        _ => None,
    }
}

pub fn ts_str(s: &str) -> Option<i64> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    // Naive "YYYY-MM-DD HH:MM:SS(.fff)" is treated as UTC.
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok().map(|d| d.and_utc().timestamp_millis())
}

pub fn file_mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub fn string_of(v: &Value, key: &str) -> Option<String> {
    str_of(v, key).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Concatenate the text of a message `content` that is either a string or an array of
/// blocks (`{type:"text",text}` / `{text}` / plain strings). Non-text blocks become `[type]`.
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                let piece = match it {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => {
                        if let Some(t) = o.get("text").and_then(Value::as_str) {
                            t.to_owned()
                        } else if let Some(t) = o.get("content") {
                            text_of(t)
                        } else if let Some(t) = o.get("type").and_then(Value::as_str) {
                            format!("[{t}]")
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                };
                if piece.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&piece);
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// JSON-encoded tool arguments arrive as strings in several harnesses; decode when possible.
pub fn json_arg(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| v.clone()),
        Value::Null => Value::Object(Default::default()),
        _ => v.clone(),
    }
}

/// One-line preview: collapse whitespace, cut to `max` chars.
pub fn preview(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(s.len()));
    let mut n = 0;
    let mut space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            n += 1;
            space = false;
        }
        if n >= max {
            out.push('…');
            break;
        }
        out.push(ch);
        n += 1;
    }
    out
}

/// Whether a process id is alive (signal 0 probe).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only the permission/existence check.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamps() {
        assert_eq!(ts(&json!("2026-10-02T12:00:00.123Z")), Some(1790942400123));
        assert_eq!(ts(&json!("2026-10-02T20:00:00+08:00")), Some(1790942400000));
        assert_eq!(ts(&json!(1790942400123i64)), Some(1790942400123));
        assert_eq!(ts(&json!(1790942400.5)), Some(1790942400500));
        assert_eq!(ts(&json!("2026-10-02 12:00:00")), Some(1790942400000));
        assert_eq!(ts(&json!("nope")), None);
    }

    #[test]
    fn text_extraction() {
        assert_eq!(text_of(&json!("a")), "a");
        assert_eq!(text_of(&json!([{"type":"text","text":"a"},{"type":"image"},"b",{"text":""}])), "a\n[image]\nb");
        assert_eq!(text_of(&json!([{"type":"tool_result","content":[{"type":"text","text":"x"}]}])), "x");
    }

    #[test]
    fn previews() {
        assert_eq!(preview("  hello \n\n world  ", 50), "hello world");
        assert_eq!(preview("abcdef", 3), "abc…");
        assert_eq!(preview("你好世界", 2), "你好…");
    }

    #[test]
    fn json_args() {
        assert_eq!(json_arg(&json!("{\"a\":1}")), json!({"a":1}));
        assert_eq!(json_arg(&json!("plain")), json!("plain"));
    }

    #[test]
    fn own_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }
}
