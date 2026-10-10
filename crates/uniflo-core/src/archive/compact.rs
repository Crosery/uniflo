//! The compact transcript a cleanup keeps: the session snapshot on the first line, then every
//! event, as zstd-compressed JSONL. Long text is cut (`truncated: true`), inline binary dropped;
//! usage events are kept whole so token and cost totals survive the cleanup unchanged.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use uniflo_schema::{Body, Event, Session, clip};

/// User, assistant, reasoning and system text.
pub const TEXT_MAX: usize = 16 * 1024;
/// Tool call arguments and tool output.
pub const TOOL_MAX: usize = 2 * 1024;
/// A run this long of base64 characters is inline binary (data URLs, encoded images or files).
const BINARY_RUN: usize = 256;

/// Cut one event down to what an archive keeps.
pub fn compact(e: &mut Event) {
    let cut = match &mut e.body {
        Body::UserMessage { text, .. }
        | Body::AssistantMessage { text, .. }
        | Body::Reasoning { text }
        | Body::System { text, .. } => squeeze(text, TEXT_MAX),
        Body::ToolResult { output, .. } => squeeze(output, TOOL_MAX),
        Body::ToolCall { input, .. } => squeeze_json(input, TOOL_MAX),
        Body::TurnStart {} | Body::TurnEnd { .. } | Body::Usage(_) => false,
    };
    e.truncated |= cut;
}

fn squeeze(s: &mut String, max: usize) -> bool {
    let stripped = strip_binary(s);
    clip(s, max) | stripped
}

fn squeeze_json(v: &mut Value, max: usize) -> bool {
    let stripped = strip_json(v);
    if let Value::String(s) = v {
        return clip(s, max) | stripped;
    }
    let s = v.to_string();
    if s.len() <= max {
        return stripped;
    }
    let mut s = s;
    clip(&mut s, max);
    *v = Value::String(s);
    true
}

fn strip_json(v: &mut Value) -> bool {
    match v {
        Value::String(s) => strip_binary(s),
        Value::Array(a) => a.iter_mut().fold(false, |acc, x| strip_json(x) | acc),
        Value::Object(o) => o.values_mut().fold(false, |acc, x| strip_json(x) | acc),
        _ => false,
    }
}

fn is_b64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=' | b'-' | b'_')
}

/// Replace base64 runs (≥ [`BINARY_RUN`], mixing upper, lower and digits — so rules like
/// `=====` stay) with a placeholder. Returns whether anything was replaced.
fn strip_binary(s: &mut String) -> bool {
    let b = s.as_bytes();
    let mut out: Option<String> = None;
    let (mut i, mut last) = (0, 0);
    while i < b.len() {
        if !is_b64(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let (mut up, mut low, mut dig) = (false, false, false);
        while i < b.len() && is_b64(b[i]) {
            up |= b[i].is_ascii_uppercase();
            low |= b[i].is_ascii_lowercase();
            dig |= b[i].is_ascii_digit();
            i += 1;
        }
        if i - start >= BINARY_RUN && up && low && dig {
            let o = out.get_or_insert_with(String::new);
            o.push_str(&s[last..start]);
            o.push_str(&format!("[binary omitted: {} bytes]", i - start));
            last = i;
        }
    }
    let Some(mut o) = out else { return false };
    o.push_str(&s[last..]);
    *s = o;
    true
}

/// Serialize and compress (one zstd frame).
pub fn encode(session: &Session, events: &[Event]) -> Result<Vec<u8>> {
    let mut raw = serde_json::to_vec(session)?;
    raw.push(b'\n');
    for e in events {
        serde_json::to_writer(&mut raw, e)?;
        raw.push(b'\n');
    }
    Ok(ruzstd::encoding::compress_to_vec(raw.as_slice(), ruzstd::encoding::CompressionLevel::Fastest))
}

pub fn decode(bytes: &[u8]) -> Result<(Session, Vec<Event>)> {
    let mut raw = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(bytes)
        .map_err(|e| anyhow!("zstd: {e}"))?
        .read_to_end(&mut raw)
        .context("zstd")?;
    let mut lines = raw.split(|b| *b == b'\n').filter(|l| !l.is_empty());
    let first = lines.next().ok_or_else(|| anyhow!("empty archive"))?;
    let session: Session = serde_json::from_slice(first).context("archive session line")?;
    let events = lines.map(serde_json::from_slice).collect::<Result<Vec<Event>, _>>().context("archive event")?;
    Ok((session, events))
}

pub fn read(path: &Path) -> Result<(Session, Vec<Event>)> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    decode(&bytes).with_context(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uniflo_schema::{Status, Usage};

    fn ev(id: &str, body: Body) -> Event {
        Event { id: id.into(), session: "t:1".into(), ts: 1, pos: Some(3), partial: false, truncated: false, body }
    }

    #[test]
    fn cuts_text_tools_and_binary_but_keeps_usage() {
        let png = format!("data:image/png;base64,{}", "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB".repeat(40));
        let mut t = ev("a", Body::AssistantMessage { text: format!("see {png} done"), model: None });
        compact(&mut t);
        let Body::AssistantMessage { text, .. } = &t.body else { panic!() };
        assert_eq!(text, "see data:image/png;base64,[binary omitted: 1280 bytes] done");
        assert!(t.truncated);

        let mut long = ev("u", Body::UserMessage { text: "字".repeat(TEXT_MAX), synthetic: false });
        compact(&mut long);
        let Body::UserMessage { text, .. } = &long.body else { panic!() };
        assert!(text.len() <= TEXT_MAX && long.truncated);

        let mut rule = ev("s", Body::System { subtype: "x".into(), text: "=".repeat(400) });
        compact(&mut rule);
        assert!(!rule.truncated, "a separator line is not binary");

        let mut call = ev(
            "c",
            Body::ToolCall { call_id: "1".into(), name: "Write".into(), input: json!({"content": "x ".repeat(4000)}) },
        );
        compact(&mut call);
        let Body::ToolCall { input: Value::String(s), name, .. } = &call.body else { panic!() };
        assert!(s.len() <= TOOL_MAX && name == "Write" && call.truncated);

        let mut out =
            ev("r", Body::ToolResult { call_id: "1".into(), name: None, output: "o".repeat(5000), is_error: false });
        compact(&mut out);
        assert!(matches!(&out.body, Body::ToolResult { output, .. } if output.len() == TOOL_MAX));

        let u = Usage { input: 9, output: 8, model: Some("m".into()), cost_usd: Some(0.1), ..Default::default() };
        let mut usage = ev("x", Body::Usage(u.clone()));
        compact(&mut usage);
        assert_eq!((usage.body, usage.truncated), (Body::Usage(u), false));
    }

    #[test]
    fn roundtrip() {
        let s = Session {
            key: "t:1".into(),
            harness: "t".into(),
            id: "1".into(),
            parent: None,
            title: Some("标题".into()),
            cwd: None,
            model: None,
            preview: None,
            source: "/a".into(),
            started_at: Some(1),
            updated_at: 2,
            status: Status::Idle,
            status_since: 2,
            status_reason: None,
            pid: None,
            usage: None,
            archived: true,
        };
        let evs = vec![ev("a", Body::Reasoning { text: "r".into() }), ev("b", Body::TurnEnd { reason: None })];
        let bytes = encode(&s, &evs).unwrap();
        assert_eq!(decode(&bytes).unwrap(), (s, evs));
        assert!(decode(b"nope").is_err());
    }
}
