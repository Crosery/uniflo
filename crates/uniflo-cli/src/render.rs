//! Terminal rendering for sessions and events.

use std::io::IsTerminal;
use uniflo_core::util::{now_ms, preview};
use uniflo_schema::{Body, Event, Session, Status};

pub struct Style {
    color: bool,
}

impl Style {
    pub fn detect() -> Style {
        Style { color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_owned() }
    }
}

pub fn age(ms: i64) -> String {
    let d = (now_ms() - ms).max(0) / 1000;
    match d {
        0..60 => format!("{d}s"),
        60..3600 => format!("{}m", d / 60),
        3600..86400 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

fn pad(s: &str, w: usize) -> String {
    let p = preview(s, w);
    let n = p.chars().count();
    if n < w { format!("{p}{}", " ".repeat(w - n)) } else { p }
}

pub fn session_line(st: &Style, s: &Session) -> String {
    let dot = match s.status {
        Status::Work => st.paint("32", "●"),
        Status::Idle => st.paint("2", "○"),
    };
    let label = s.title.as_deref().or(s.preview.as_deref()).unwrap_or("(untitled)");
    let cwd = s.cwd.as_deref().map(short_path).unwrap_or_default();
    let live = if s.pid.is_some() { st.paint("36", "⚡") } else { " ".into() };
    format!(
        "{dot}{live}{} {} {} {} {}",
        st.paint("1", &pad(&s.harness, 11)),
        pad(&age(s.updated_at), 4),
        pad(label, 60),
        st.paint("2", &pad(&cwd, 28)),
        st.paint("2", &s.key)
    )
}

pub fn session_tsv(s: &Session) -> String {
    let clean = |x: &str| x.replace(['\t', '\n'], " ");
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}",
        s.key,
        s.harness,
        s.status.as_str(),
        age(s.updated_at),
        clean(s.title.as_deref().or(s.preview.as_deref()).unwrap_or("")),
        clean(s.cwd.as_deref().unwrap_or(""))
    )
}

fn short_path(p: &str) -> String {
    match uniflo_core::util::home().to_str() {
        Some(h) if p.starts_with(h) => format!("~{}", &p[h.len()..]),
        _ => p.to_owned(),
    }
}

fn clock(ms: i64) -> String {
    if ms <= 0 {
        return "--:--:--".into();
    }
    chrono_like(ms)
}

/// Local HH:MM:SS via libc (no tz database dependency).
fn chrono_like(ms: i64) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed struct we own.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub fn event_line(st: &Style, e: &Event, width: usize) -> String {
    let (tag, color, text) = match &e.body {
        Body::UserMessage { text, synthetic } => (if *synthetic { "user*" } else { "user" }, "1;34", text.clone()),
        Body::AssistantMessage { text, .. } => ("assistant", "1;32", text.clone()),
        Body::Reasoning { text } => ("thinking", "2", text.clone()),
        Body::ToolCall { name, input, .. } => ("tool →", "33", format!("{name} {}", input_summary(input))),
        Body::ToolResult { output, is_error, name, .. } => {
            let n = name.as_deref().unwrap_or("");
            (if *is_error { "tool ✗" } else { "tool ←" }, if *is_error { "31" } else { "33" }, format!("{n} {output}"))
        }
        Body::TurnStart {} => ("turn ▶", "2", String::new()),
        Body::TurnEnd { reason } => ("turn ■", "2", reason.clone().unwrap_or_default()),
        Body::Usage(u) => {
            ("usage", "2", format!("in {} out {} cache {}/{}", u.input, u.output, u.cache_read, u.cache_write))
        }
        Body::System { subtype, text } => ("system", "35", format!("{subtype} {text}")),
    };
    let partial = if e.partial { "…" } else { "" };
    format!("{} {} {}{partial}", st.paint("2", &clock(e.ts)), st.paint(color, &pad(tag, 9)), preview(&text, width))
}

fn input_summary(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(o) => {
            for k in [
                "command",
                "cmd",
                "file_path",
                "filePath",
                "path",
                "pattern",
                "query",
                "search_term",
                "searchTerm",
                "q",
                "url",
                "description",
                "prompt",
            ] {
                if let Some(s) = o.get(k).and_then(|x| x.as_str()) {
                    return s.to_owned();
                }
            }
            if let Some(action) = o.get("action").and_then(serde_json::Value::as_object) {
                if let Some(s) = action.get("query").and_then(|x| x.as_str()) {
                    return s.to_owned();
                }
                if let Some(arr) = action.get("queries").and_then(serde_json::Value::as_array)
                    && let Some(s) = arr.first().and_then(|x| x.as_str())
                {
                    return s.to_owned();
                }
            }
            v.to_string()
        }
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
