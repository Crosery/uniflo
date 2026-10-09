//! dsh (DeepSeek Harness) transcripts.
//!
//! Layout: `~/.dsh/sessions/<cwd-slug>/<id>/session.jsonl.zstd` (older builds name it
//! `session.v4.jsonl.zstd`; same content). JSONL records are appended in zstd frames:
//! one frame per writer flush, holding one or more whole newline-terminated records
//! (verified over the whole local corpus: every frame ends on a newline, records never
//! span frames). A reader follows growth frame-by-frame without decompressing history;
//! the compressed byte offset of the last complete frame is the cursor.
//!
//! Event model: `turn/start`/`turn/end{completed|aborted|interrupted|error}` bracket a
//! turn; `user/message` carries the human text (`source.kind == "user"`) or harness
//! injections (plugin context, skill catalogs, runtime snapshots → synthetic); the final
//! assistant text/reasoning/usage lands in `assistant/message` (the `*-chunks` /
//! `assistant/chunk` streaming records are ignored); `tool/call` + `tool/result` pair on
//! `callId` (the duplicate tool-call blocks inside the assistant message are dropped —
//! the standalone records are authoritative). Session metadata lives in the first record
//! (`session`: id/cwd/createdAt/parentSession), the current model in `request/context`.
//!
//! Liveness: every session directory holds `session.lock`, held open by the harness
//! process while the session is open — `lsof` maps lock files to pids exactly (Windows
//! has no `lsof`; there `live()` reports nothing and status falls back to event rules).

use crate::common::under_any;
use anyhow::Result;
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor as BufCursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uniflo_core::HarnessInfo;
use uniflo_core::adapter::{
    Adapter, Batch, Cursor as SrcCursor, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record,
};
use uniflo_core::procs::ProcCache;
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, text_of, ts};
use uniflo_schema::{Body, Event, Usage, session_key};

/// Summary scan decodes this much from the start of the compressed file.
const HEAD_BYTES: usize = 256 * 1024;
/// …and this much from the end.
const TAIL_BYTES: usize = 256 * 1024;
/// Appends larger than this are re-summarized instead of replayed.
const CATCHUP_MAX: u64 = 16 * 1024 * 1024;
/// Backward history window growth: chunk, then cap.
const REV_CHUNK: usize = 256 * 1024;
const MAX_WINDOW: usize = 64 * 1024 * 1024;
/// Cap for the persisted callId → tool-name map.
const NAMES_CAP: usize = 512;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const ID: &str = "dsh";

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(Dsh {
        root: home().join(".dsh/sessions"),
        procs: ProcCache::new(PROC_TTL, is_dsh).with_files(is_lock),
    })]
}

pub struct Dsh {
    root: PathBuf,
    procs: ProcCache,
}

#[cfg(test)]
impl Dsh {
    fn with_root(root: PathBuf) -> Self {
        Dsh { root, procs: ProcCache::new(PROC_TTL, is_dsh).with_files(is_lock) }
    }
}

const PROC_TTL: Duration = Duration::from_secs(3);

/// dsh CLI (`…/bin/dsh web`, bun global `…/@deepseek-ai/dsh…`), the TUI package, or the
/// desktop app (any process of the `DeepSeek Harness.app` bundle).
#[cfg(not(target_os = "windows"))]
fn is_dsh(args: &str) -> bool {
    args.contains("@deepseek-ai/dsh")
        || args.contains("/dsh-tui/")
        || args.contains("DeepSeek Harness.app/Contents/")
        || args.contains("/bin/dsh")
}

#[cfg(target_os = "windows")]
fn is_dsh(args: &str) -> bool {
    let a = args.to_ascii_lowercase().replace('\\', "/");
    a.contains("@deepseek-ai/dsh")
        || a.contains("/dsh-tui/")
        || a.contains("deepseek harness")
        || a.contains("/bin/dsh")
}

fn is_lock(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n == "session.lock")
}

fn is_transcript(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n == "session.jsonl.zstd" || n == "session.v4.jsonl.zstd")
}

/// Per-source decode state carried in the cursor.
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct State {
    /// Native session id from the header record (authoritative; dir names match it).
    id: Option<String>,
    /// callId → tool name, for naming results (bounded; a miss is a nicer `[tool]`).
    names: HashMap<String, String>,
}

/// One decoded frame: (decompressed body — one or more whole JSONL records —,
/// compressed bytes consumed including this frame).
struct Frame {
    body: Vec<u8>,
    consumed: usize,
}

/// Decode exactly one zstd frame (or skippable frame) at the head of `buf`.
/// `None` = mis-aligned / truncated / invalid. A skippable frame yields an empty body.
fn decode_frame(dec: &mut FrameDecoder, buf: &[u8]) -> Option<Frame> {
    if buf.len() < 4 {
        return None;
    }
    let m = u32::from_le_bytes(buf[..4].try_into().unwrap());
    if (0x184D_2A50..=0x184D_2A5F).contains(&m) {
        if buf.len() < 8 {
            return None;
        }
        let skip = 8 + u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        return Some(Frame { body: Vec::new(), consumed: skip.min(buf.len()) });
    }
    if m != u32::from_le_bytes(ZSTD_MAGIC) {
        return None;
    }
    let mut cur = BufCursor::new(buf);
    if dec.reset(&mut cur).is_err() {
        return None;
    }
    if !dec.decode_blocks(&mut cur, BlockDecodingStrategy::All).unwrap_or(false) {
        return None;
    }
    let body = dec.collect()?;
    let consumed = dec.bytes_read_from_source() as usize;
    // Guard against a corrupt frame claiming to read past the buffer.
    if consumed == 0 || consumed > buf.len() {
        return None;
    }
    Some(Frame { body, consumed })
}

/// First magic-frame start at or after `from`.
fn next_frame_pos(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?.windows(4).position(|w| w == ZSTD_MAGIC).map(|i| from + i)
}

/// Last magic-frame start strictly before `until`.
fn last_frame_pos(buf: &[u8], until: usize) -> Option<usize> {
    buf.get(..until)?.windows(4).rposition(|w| w == ZSTD_MAGIC)
}

/// The newline-terminated records of a frame body (empty fragments skipped).
fn records(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    body.split(|&b| b == b'\n').map(trim).filter(|l| !l.is_empty())
}

/// Decode every frame from `pos` to the end of `buf`, calling `f(record, frame_start,
/// index_in_frame)` per record. Returns the offset just past the last frame consumed:
/// a truncated trailing frame stops the walk at its start (it may complete later and
/// must be re-read), while garbage bytes before a real next frame are resynced over.
/// A body not newline-terminated keeps its last fragment (dsh flushes whole records,
/// so a closed frame never splits a record; the fragment still parses as a record).
fn forward_frames<F: FnMut(&[u8], usize, usize)>(buf: &[u8], pos: usize, mut f: F) -> usize {
    let mut dec = FrameDecoder::new();
    let mut p = match next_frame_pos(buf, pos) {
        Some(p) => p,
        None => return buf.len(),
    };
    while p < buf.len() {
        let frame = match decode_frame(&mut dec, &buf[p..]) {
            Some(fr) => Some(fr),
            // A false-positive magic inside a block's payload must not be consumed as a
            // frame start: only trust the next candidate if that one actually decodes.
            None => match next_frame_pos(buf, p + 4) {
                Some(np) => decode_frame(&mut dec, &buf[np..]).inspect(|_| p = np),
                None => None,
            },
        };
        let Some(fr) = frame else { break };
        for (i, rec) in records(&fr.body).enumerate() {
            f(rec, p, i);
        }
        p += fr.consumed;
    }
    p
}

/// Collect frame bodies backward from `end` (a frame start) until `limit` events are
/// estimated, growing the scan window as needed. A candidate boundary is only trusted
/// when the bytes `[p, boundary)` decode to exactly one frame of that size.
fn rev_walk(bytes: &[u8], end: usize, limit: usize) -> Vec<(usize, Vec<u8>)> {
    let mut dec = FrameDecoder::new();
    let mut out: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut est = 0usize;
    let mut boundary = end;
    let mut hi = end;
    let mut window = REV_CHUNK;
    let mut lo = end.saturating_sub(window);
    loop {
        if est >= limit || boundary == 0 {
            break;
        }
        let Some(rel) = bytes[lo..hi].windows(4).rposition(|w| w == ZSTD_MAGIC) else {
            if lo == 0 {
                break;
            }
            window = (window * 2).min(MAX_WINDOW);
            hi = lo;
            lo = lo.saturating_sub(window);
            continue;
        };
        let p = lo + rel;
        hi = p;
        if let Some(fr) = decode_frame(&mut dec, &bytes[p..boundary])
            && fr.consumed == boundary - p
        {
            boundary = p;
            if !fr.body.is_empty() {
                est += probe_body(&fr.body);
                out.push((p, fr.body));
            }
        }
    }
    out.reverse();
    out
}

/// First frame boundary at or after `from` whose frame decodes and ends exactly at a
/// magic start or EOF: a compressed payload can contain the 4-byte magic, so the tail
/// window of a summary scan may start inside a frame and needs a stronger check than
/// "some magic decodes".
fn align_frames(bytes: &[u8], from: usize) -> usize {
    let mut dec = FrameDecoder::new();
    let mut p = from;
    while let Some(m) = next_frame_pos(bytes, p) {
        if let Some(fr) = decode_frame(&mut dec, &bytes[m..]) {
            let end = m + fr.consumed;
            if end >= bytes.len() || bytes[end..].starts_with(&ZSTD_MAGIC) {
                return m;
            }
        }
        p = m + 4;
    }
    bytes.len()
}

impl Adapter for Dsh {
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "DeepSeek Harness" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        if is_transcript(path) {
            return Some(path.to_path_buf());
        }
        if is_lock(path) {
            let dir = path.parent()?;
            for name in ["session.jsonl.zstd", "session.v4.jsonl.zstd"] {
                let t = dir.join(name);
                if t.is_file() {
                    return Some(t);
                }
            }
        }
        None
    }

    fn discover(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return out;
        }
        uniflo_core::adapter::walk(&self.root, 2, &is_transcript, &mut out);
        out
    }

    fn read(&self, src: &Path, cursor: Option<&SrcCursor>) -> Result<ReadOutput> {
        decode_file(src, cursor, false)
    }

    /// Every record in file order with the metadata inline: `request/context` carries the
    /// model, which a replay through `history()` (events only) would lose for each step.
    fn read_all(&self, src: &Path, _sessions: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<SrcCursor> {
        let out = decode_file(src, None, true)?;
        for (id, r) in out.batch.items {
            sink(&id, r);
        }
        Ok(out.cursor)
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let key = session_key(ID, session_id);
        let bytes = std::fs::read(src)?;
        let limit = q.limit.max(1);
        let end = q.before.map_or(bytes.len(), |b| (b as usize).min(bytes.len()));
        let mut bodies = rev_walk(&bytes, end, limit);
        if bodies.is_empty() && q.before.is_none() {
            // A torn trailing frame sits at the tail: its start is still a valid
            // boundary for the frames before it.
            if let Some(p) = last_frame_pos(&bytes, end) {
                bodies = rev_walk(&bytes, p, limit);
            }
        }
        let mut st = State::default();
        let mut events = Vec::new();
        for (pos, body) in bodies {
            for (i, rec) in records(&body).enumerate() {
                let Some(v) = parse_record(rec) else { continue };
                let mut d = EventD::default();
                decode(&v, pos, i, &mut d, &mut st);
                for mut e in d.out {
                    e.session = key.clone();
                    events.push(e);
                }
            }
        }
        Ok(dedupe(events))
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for p in self.procs.get() {
            for lock in &p.files {
                // …/sessions/<slug>/<id>/session.lock → <id> (dir name equals header id).
                let Some(id) = lock.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if !under_any(lock, std::slice::from_ref(&self.root)) || !seen.insert(id.clone()) {
                    continue;
                }
                out.push(LiveSession { id, pid: p.pid, status: None });
            }
        }
        Some(out)
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

/// `<root>/<slug>/<id>/session.jsonl.zstd` → `id`, the fallback session id before a
/// header is decoded (dsh names session dirs after the id; verified against the store).
/// Read from `cursor` (or the whole file). A first read samples head + tail unless `whole`.
fn decode_file(src: &Path, cursor: Option<&SrcCursor>, whole: bool) -> Result<ReadOutput> {
    let md = std::fs::metadata(src)?;
    let (size, mtime) = (md.len(), file_mtime_ms(&md));
    let mut st: State = cursor.and_then(|c| serde_json::from_value(c.state.clone()).ok()).unwrap_or_default();
    let mut out = ReadOutput::default();
    let (bytes, start): (Vec<u8>, usize) = match cursor {
        Some(c) if size < c.offset => {
            out.reset = true;
            (std::fs::read(src)?, 0)
        }
        Some(c) => {
            let mut f = std::fs::File::open(src)?;
            f.seek(std::io::SeekFrom::Start(c.offset))?;
            let mut tail = Vec::new();
            f.read_to_end(&mut tail)?;
            if tail.len() as u64 > CATCHUP_MAX { (std::fs::read(src)?, 0) } else { (tail, c.offset as usize) }
        }
        None => (std::fs::read(src)?, 0),
    };
    out.summary = start == 0;
    let mut batch = Batch::default();
    let consumed = if start == 0 && !whole && bytes.len() > HEAD_BYTES + TAIL_BYTES {
        // Head sample, then the tail window; the middle is never replayed (history
        // decodes it from disk on demand).
        let head_end = forward_frames(&bytes[..HEAD_BYTES], 0, |r, p, i| batch_record(&mut st, &mut batch, r, p, i));
        let floor = align_frames(&bytes, head_end.max(bytes.len() - TAIL_BYTES));
        floor + forward_frames(&bytes[floor..], 0, |r, p, i| batch_record(&mut st, &mut batch, r, floor + p, i))
    } else {
        forward_frames(&bytes, 0, |r, p, i| batch_record(&mut st, &mut batch, r, start + p, i))
    };
    batch.bytes = consumed as u64;
    // The header record is the first line; back-fill the session id slot and stamp
    // every event so forwarded events carry their session key.
    if let Some(id) = st.id.clone().or_else(|| dir_id(src)) {
        let key = session_key(ID, &id);
        for (slot, rec) in &mut batch.items {
            *slot = id.clone();
            if let Record::Event(e) = rec {
                e.session = key.clone();
            }
        }
    }
    out.batch = batch;
    out.cursor = SrcCursor {
        offset: (start + consumed).min(size as usize) as u64,
        size,
        mtime_ms: mtime,
        state: serde_json::to_value(&st).unwrap_or_default(),
    };
    Ok(out)
}

fn dir_id(src: &Path) -> Option<String> {
    src.parent()?.file_name().map(|n| n.to_string_lossy().into_owned())
}

fn parse_record(raw: &[u8]) -> Option<Value> {
    serde_json::from_slice(raw).ok()
}

/// Decode one frame record into `batch`, appending events/meta/unknowns.
fn batch_record(st: &mut State, batch: &mut Batch, raw: &[u8], pos: usize, idx: usize) {
    let Some(v) = parse_record(raw) else {
        batch.bad_lines += 1;
        return;
    };
    let id = st.id.clone().unwrap_or_default();
    let mut d = EventD::default();
    decode(&v, pos, idx, &mut d, st);
    for e in d.out {
        batch.items.push((id.clone(), Record::Event(e)));
    }
    if let Some(m) = d.meta {
        batch.items.push((id, Record::Meta(m)));
    }
    batch.unknown.extend(d.unknown);
}

fn trim(raw: &[u8]) -> &[u8] {
    let mut e = raw.len();
    while e > 0 && matches!(raw[e - 1], b'\n' | b'\r') {
        e -= 1;
    }
    &raw[..e]
}

#[derive(Default)]
struct EventD {
    out: Vec<Event>,
    meta: Option<MetaPatch>,
    unknown: Vec<String>,
}

impl EventD {
    fn emit(&mut self, id: impl Into<String>, t: i64, pos: usize, body: Body) {
        self.out.push(Event {
            id: id.into(),
            session: String::new(),
            ts: t,
            pos: Some(pos as u64),
            partial: false,
            truncated: false,
            body,
        });
    }
    fn meta(&mut self) -> &mut MetaPatch {
        self.meta.get_or_insert_with(MetaPatch::default)
    }
    fn unknown(&mut self, t: &str) {
        self.unknown.push(format!("type={t}"));
    }
}

/// history(): how many events one frame body will yield (paging estimate, like the
/// JSONL driver's per-line probe).
fn probe_body(body: &[u8]) -> usize {
    let mut st = State::default();
    records(body)
        .enumerate()
        .map(|(i, rec)| match parse_record(rec) {
            Some(v) => {
                let mut d = EventD::default();
                decode(&v, 0, i, &mut d, &mut st);
                d.out.len()
            }
            None => 0,
        })
        .sum()
}

/// Map one dsh record onto normalized events. `pos` is the compressed frame offset,
/// `idx` the record's position inside the frame (ids for records without a native id).
fn decode(v: &Value, pos: usize, idx: usize, ev: &mut EventD, st: &mut State) {
    let oid = || format!("o{pos}#{idx}");
    let Some(t) = str_of(v, "type") else {
        ev.unknown("");
        return;
    };
    let time = ts(v.get("time").unwrap_or(&Value::Null)).unwrap_or(0);
    let d = v.get("data").unwrap_or(&Value::Null);
    let seq = v.get("seq").and_then(Value::as_u64);
    match t {
        "session" => {
            if let Some(sid) = string_of(v, "id") {
                st.id = Some(sid);
            }
            let m = ev.meta();
            m.cwd = string_of(v, "cwd");
            m.started_at = ts(v.get("createdAt").unwrap_or(&Value::Null));
            m.parent = string_of(v, "parentSession");
        }
        "turn/start" => {
            let n = d.get("turn").and_then(Value::as_u64).unwrap_or(0);
            ev.emit(format!("t{n}:start"), time, pos, Body::TurnStart {});
        }
        "turn/end" => {
            let n = d.get("turn").and_then(Value::as_u64).unwrap_or(0);
            let kind = d.get("reason").and_then(|r| r.get("kind")).and_then(Value::as_str).unwrap_or("completed");
            let reason = match kind {
                "completed" => "complete",
                other => other,
            };
            ev.emit(format!("t{n}:end"), time, pos, Body::TurnEnd { reason: Some(reason.into()) });
        }
        "user/message" => {
            let mid = string_of(d, "id").or_else(|| seq.map(|s| format!("s{s}"))).unwrap_or_else(oid);
            let synthetic = d.get("source").and_then(|s| s.get("kind")).and_then(Value::as_str) != Some("user");
            let text = text_of(d.get("content").unwrap_or(&Value::Null));
            if text.is_empty() {
                return;
            }
            ev.emit(mid, time, pos, Body::UserMessage { text, synthetic });
        }
        "assistant/message" => {
            let msg = d.get("message").unwrap_or(&Value::Null);
            let mid = string_of(msg, "id").or_else(|| seq.map(|s| format!("s{s}"))).unwrap_or_else(oid);
            let blocks = msg.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut r = 0usize;
            let mut texts = String::new();
            for b in &blocks {
                match str_of(b, "type").unwrap_or("") {
                    "reasoning" => {
                        if let Some(s) = str_of(b, "text")
                            && !s.is_empty()
                        {
                            ev.emit(format!("{mid}:r{r}"), time, pos, Body::Reasoning { text: s.to_owned() });
                        }
                        r += 1;
                    }
                    "text" => {
                        if let Some(s) = str_of(b, "text") {
                            if !texts.is_empty() {
                                texts.push('\n');
                            }
                            texts.push_str(s);
                        }
                    }
                    // tool-call blocks duplicate the standalone `tool/call` records;
                    // image blocks carry no text.
                    "tool-call" | "image" => {}
                    other => ev.unknown(other),
                }
            }
            if !texts.is_empty() {
                ev.emit(mid.clone(), time, pos, Body::AssistantMessage { text: texts, model: None });
            }
            if let Some(u) = d.get("usage")
                && let Some(body) = usage(u)
            {
                ev.emit(format!("{mid}:usage"), time, pos, body);
            }
        }
        "tool/call" => {
            let cid = str_of(d, "callId").unwrap_or_default().to_owned();
            let name = str_of(d, "name").unwrap_or("").to_owned();
            if !cid.is_empty() && st.names.len() < NAMES_CAP {
                st.names.insert(cid.clone(), name.clone());
            }
            let input = json_arg(d.get("arguments").unwrap_or(&Value::Null));
            ev.emit(cid.clone(), time, pos, Body::ToolCall { call_id: cid, name, input });
        }
        "tool/result" => {
            let msg = d.get("message").unwrap_or(&Value::Null);
            let cid = string_of(msg, "toolCallId")
                .or_else(|| msg.get("source").and_then(|s| string_of(s, "callId")))
                .unwrap_or_default();
            if cid.is_empty() {
                return;
            }
            let name = st.names.get(&cid).cloned();
            let content = msg.get("content").unwrap_or(&Value::Null);
            // Results sometimes wrap blocks in a `tool-result` envelope.
            let inner = match content.as_array().and_then(|a| a.first()) {
                Some(b) if str_of(b, "type") == Some("tool-result") => b.get("content").unwrap_or(&Value::Null),
                _ => content,
            };
            let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false);
            ev.emit(
                format!("{cid}:res"),
                time,
                pos,
                Body::ToolResult { call_id: cid, name, output: text_of(inner), is_error },
            );
        }
        "session/title" => {
            if let Some(title) = string_of(d, "title") {
                let kind = d.get("source").and_then(|s| s.get("kind")).and_then(Value::as_str).unwrap_or("fallback");
                let rank: u8 = match kind {
                    "user" => 3,
                    "provider" => 2,
                    _ => 1,
                };
                ev.meta().title = Some((rank, title));
            }
        }
        "request/context" => {
            if let Some(model) = string_of(d, "model") {
                ev.meta().model = Some(model);
            }
        }
        "command/run" => {
            let cid = str_of(d, "commandId").unwrap_or("cmd");
            let name = str_of(d, "name").unwrap_or("");
            let args = str_of(d, "args").unwrap_or("");
            let text = format!("/{name} {args}").trim().to_owned();
            let synthetic = d.get("source").and_then(|s| s.get("kind")).and_then(Value::as_str) != Some("user");
            ev.emit(format!("{cid}:cmd"), time, pos, Body::UserMessage { text, synthetic });
        }
        "system/message" | "developer/message" => {
            let sub = if t == "system/message" { "system" } else { "developer" };
            let text = text_of(d.get("message").unwrap_or(&Value::Null).get("content").unwrap_or(&Value::Null));
            ev.emit(oid(), time, pos, Body::System { subtype: sub.into(), text });
        }
        "compaction/summary" => {
            let text = text_of(d.get("summary").unwrap_or(&Value::Null));
            ev.emit(oid(), time, pos, Body::System { subtype: "compaction".into(), text });
        }
        "session/title-llm-request"
        | "session/end-seed"
        | "request/header"
        | "sandbox/mode"
        | "approval/policy"
        | "approval/asked"
        | "approval/decided"
        | "permission/preset"
        | "agent-preset/selected"
        | "step/start"
        | "step/end"
        | "agent/inbox/spliced"
        | "assistant/chunk"
        | "assistant/attempt"
        | "reasoning-chunks"
        | "text-chunks"
        | "tool-call-chunks"
        | "tool/code-dispatch"
        | "tool/code-dispatch-start"
        | "llm/retry"
        | "llm/retry-started"
        | "compaction/start"
        | "compaction/end"
        | "compaction/prune"
        | "todo/write"
        | "goal/change"
        | "plan/mode"
        | "model/selection"
        | "team/task"
        | "team/message/queued"
        | "team/message/delivered"
        | "team/member"
        | "subagent/catalog"
        | "subagent/descriptor"
        | "workspace/changes"
        | "deliverables/presented"
        | "command/done"
        | "web/deepseek-search-llm-request" => {}
        other => ev.unknown(other),
    }
}

fn usage(u: &Value) -> Option<Body> {
    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (input, output, cache_read, cache_write, reasoning) =
        (g("inputTokens"), g("outputTokens"), g("cacheReadTokens"), g("cacheWriteTokens"), g("reasoningTokens"));
    if input + output + cache_read + cache_write + reasoning == 0 {
        return None;
    }
    Some(Body::Usage(Usage { input, output, cache_read, cache_write, reasoning, ..Default::default() }))
}

/// Keep one event per id, first position, latest content (mirrors the JSONL driver).
fn dedupe(records: Vec<Event>) -> Vec<Event> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Event> = HashMap::new();
    for e in records {
        if !map.contains_key(&e.id) {
            order.push(e.id.clone());
        }
        map.insert(e.id.clone(), e);
    }
    order.into_iter().filter_map(|id| map.remove(&id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};
    use serde_json::json;
    use std::io::Write;
    use uniflo_schema::Status;

    /// dsh appends one frame per flush; each fixture line becomes its own frame here.
    fn frame(line: &Value) -> Vec<u8> {
        compress_to_vec(format!("{line}\n").as_bytes(), CompressionLevel::Fastest)
    }

    /// Several records flushed together into one frame.
    fn multi_frame(lines: &[Value]) -> Vec<u8> {
        let body: Vec<u8> = lines.iter().flat_map(|l| format!("{l}\n").into_bytes()).collect();
        compress_to_vec(&body[..], CompressionLevel::Fastest)
    }

    fn frames(lines: &[Value]) -> Vec<u8> {
        let mut buf = Vec::new();
        for l in lines {
            buf.extend(frame(l));
        }
        buf
    }

    fn header(id: &str) -> Value {
        json!({"type":"session","version":4,"id":id,"createdAt":1790696191428i64,"cwd":"/w","parentSession":"p1"})
    }

    fn rec(t: &str, time: i64, data: Value) -> Value {
        json!({"type":t,"seq":1,"time":time,"data":data})
    }

    fn dsh(fx: &Fixture) -> Dsh {
        Dsh::with_root(fx.root().join("sessions"))
    }

    fn session(fx: &Fixture, name: &str, bytes: &[u8]) -> PathBuf {
        let p = fx.root().join("sessions").join("--w--").join("s1").join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// ~400 KB that zstd cannot shrink much (keeps a record out of the head / tail sample).
    fn noise(seed: u64) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut x = seed;
        (0..400_000)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                A[(x >> 58) as usize] as char
            })
            .collect()
    }

    #[test]
    fn read_all_keeps_mid_file_model_for_usage() {
        let fx = Fixture::new();
        let pad = |seed| {
            rec(
                "system/message",
                1,
                json!({"message":{"role":"system","content":[{"type":"text","text":noise(seed)}]}}),
            )
        };
        let lines = vec![
            header("s1"),
            pad(1),
            rec("request/context", 2, json!({"provider":"p","model":"m-mid","contextWindow":1000})),
            rec(
                "assistant/message",
                3,
                json!({"turn":1,"step":1,"message":{"role":"assistant","id":"a1","content":[]},
                "usage":{"inputTokens":7,"outputTokens":3,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}),
            ),
            pad(2),
        ];
        let p = session(&fx, "session.jsonl.zstd", &frames(&lines));
        let d = dsh(&fx);
        assert!(std::fs::metadata(&p).unwrap().len() as usize > HEAD_BYTES + TAIL_BYTES + 100_000);
        let summary = d.read(&p, None).unwrap();
        let sampled_model = summary.batch.items.iter().any(|(_, r)| matches!(r, Record::Meta(m) if m.model.is_some()));
        assert!(!sampled_model, "fixture keeps request/context out of the head + tail sample");

        let mut seen = Vec::new();
        let cursor = d
            .read_all(&p, &["s1".into()], &mut |id, r| {
                assert_eq!(id, "s1");
                match r {
                    Record::Meta(m) if m.model.is_some() => seen.push(format!("model={}", m.model.unwrap())),
                    Record::Event(e) if matches!(e.body, Body::Usage(_)) => seen.push("usage".into()),
                    _ => {}
                }
            })
            .unwrap();
        assert_eq!(seen, ["model=m-mid", "usage"]);
        assert_eq!(cursor.offset, std::fs::metadata(&p).unwrap().len());
    }

    fn full_turn() -> Vec<Value> {
        vec![
            header("s1"),
            rec(
                "system/message",
                1,
                json!({"message":{"role":"system","content":[{"type":"text","text":"instructions"}]}}),
            ),
            rec("turn/start", 2, json!({"turn":1})),
            rec(
                "user/message",
                3,
                json!({"id":"u1","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"do the thing"}]}),
            ),
            rec(
                "user/message",
                3,
                json!({"id":"u2","role":"user","source":{"kind":"plugin"},"content":[{"type":"text","text":"plugin context"}]}),
            ),
            rec(
                "assistant/message",
                4,
                json!({"turn":1,"step":1,"message":{"role":"assistant","id":"a1","content":[
                    {"type":"reasoning","text":"thinking"},
                    {"type":"tool-call","id":"c1","name":"Bash","arguments":"{\"command\":\"ls\"}"},
                    {"type":"text","text":"running now"}
                ]},"usage":{"inputTokens":7,"outputTokens":3,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}),
            ),
            rec(
                "tool/call",
                5,
                json!({"turn":1,"step":1,"callId":"c1","name":"Bash","arguments":"{\"command\":\"ls\"}"}),
            ),
            rec(
                "tool/result",
                6,
                json!({"turn":1,"step":1,"message":{"role":"assistant","source":{"kind":"tool","callId":"c1"},"toolCallId":"c1","isError":false,"content":[{"type":"tool-result","toolCallId":"c1","content":[{"type":"text","text":"a.txt"}]}]}}),
            ),
            rec("command/run", 7, json!({"commandId":"cmd-1","name":"plan","args":"","source":{"kind":"user"}})),
            rec("session/title", 7, json!({"title":"Auto title","source":{"kind":"fallback"}})),
            rec("session/title", 8, json!({"title":"My name","source":{"kind":"user"}})),
            rec("request/context", 9, json!({"provider":"p","model":"deepseek-chat","contextWindow":200000})),
            rec(
                "compaction/summary",
                10,
                json!({"compactionId":"k1","summary":[{"type":"text","text":"compact notes"}]}),
            ),
            rec("assistant/chunk", 10, json!({"chunk":{"type":"text-delta","text":"half"}})),
            rec("step/start", 10, json!({"turn":1,"step":2})),
            rec("turn/end", 11, json!({"turn":1,"reason":{"kind":"completed"}})),
        ]
    }

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let p = session(&fx, "session.jsonl.zstd", &frames(&full_turn()));
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec![
                "system",
                "turn_start",
                "user_message",
                "user_message",
                "reasoning",
                "assistant_message",
                "usage",
                "tool_call",
                "tool_result",
                "user_message",
                "system",
                "turn_end"
            ]
        );
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert_eq!(r.bad_lines, 0);
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(r.meta.started_at, Some(1790696191428));
        assert_eq!(r.meta.parent.as_deref(), Some("p1"));
        assert_eq!(r.meta.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(r.meta.title.as_deref(), Some("My name"));
        assert_eq!(r.meta.title_rank, 3);
        assert_eq!(r.preview.as_deref(), Some("do the thing"));
        assert!(r.events.iter().all(|e| e.session == "dsh:s1"));
        let ids: Vec<&str> = r.events.iter().map(|e| e.id.as_str()).collect();
        // system/compaction records carry no native id: stable pos-derived ids.
        assert!(ids[0].starts_with('o') && ids[10].starts_with('o'), "{ids:?}");
        assert_eq!(&ids[1..3], ["t1:start", "u1"]);
        assert_eq!(&ids[3..10], ["u2", "a1:r0", "a1", "a1:usage", "c1", "c1:res", "cmd-1:cmd"]);
        assert_eq!(ids[11], "t1:end");
        assert!(matches!(&r.events[2].body, Body::UserMessage { text, synthetic: false } if text == "do the thing"));
        assert!(matches!(&r.events[3].body, Body::UserMessage { text, synthetic: true } if text == "plugin context"));
        assert!(
            matches!(&r.events[7].body, Body::ToolCall { call_id, name, input } if call_id == "c1" && name == "Bash" && input["command"] == "ls")
        );
        assert!(
            matches!(&r.events[8].body, Body::ToolResult { call_id, name, output, is_error } if call_id == "c1" && name.as_deref() == Some("Bash") && output == "a.txt" && !is_error)
        );
        assert!(matches!(&r.events[9].body, Body::UserMessage { text, synthetic: false } if text == "/plan"));
        assert!(
            matches!(&r.events[10].body, Body::System { subtype, text } if subtype == "compaction" && text == "compact notes")
        );
        assert!(matches!(&r.events[11].body, Body::TurnEnd { reason: Some(r) } if r == "complete"));
    }

    #[test]
    fn multi_record_frames_decode_without_bad_lines() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let lines = full_turn();
        let mut buf = multi_frame(&lines[0..2]);
        buf.extend(multi_frame(&lines[2..13]));
        buf.extend(frame(&lines[13]));
        buf.extend(multi_frame(&lines[14..]));
        let p = session(&fx, "session.jsonl.zstd", &buf);
        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events).len(), 12);
        assert_eq!(r.bad_lines, 0);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        // The two system-ish records share one frame: their fallback ids differ by index.
        assert_ne!(r.events[0].id, r.events[10].id);
    }

    #[test]
    fn v4_filename_is_accepted() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let p = session(&fx, "session.v4.jsonl.zstd", &frames(&full_turn()));
        assert_eq!(a.source_for(&p).as_deref(), Some(p.as_path()));
        let r = fx.index(&a, &p);
        assert_eq!(r.id, "s1");
        assert_eq!(kinds(&r.events).len(), 12);
        assert_eq!(a.discover().len(), 1);
    }

    #[test]
    fn follow_yields_only_appended_frames() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let p = session(&fx, "session.jsonl.zstd", &frames(&full_turn()));
        let first = a.read(&p, None).unwrap();
        assert!(first.summary);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&multi_frame(&[
            rec("turn/start", 12, json!({"turn":2})),
            rec("user/message", 13, json!({"id":"u9","role":"user","source":{"kind":"user"},"content":[{"type":"text","text":"next step"}]})),
        ]))
        .unwrap();
        drop(f);
        let second = a.read(&p, Some(&first.cursor)).unwrap();
        assert!(!second.summary);
        assert!(!second.reset);
        assert_eq!(second.batch.items.len(), 2);
        assert_eq!(second.batch.bad_lines, 0);
        assert!(second.batch.unknown.is_empty());
        let evs: Vec<&Event> = second
            .batch
            .items
            .iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                _ => None,
            })
            .collect();
        assert!(matches!(&evs[1].body, Body::UserMessage { text, synthetic: false } if text == "next step"));
        assert!(evs.iter().all(|e| e.session == "dsh:s1"));
        assert_eq!(evs[0].id, "t2:start");
        assert!(evs[0].pos.unwrap() >= first.cursor.offset);
        assert_eq!(second.cursor.offset, std::fs::metadata(&p).unwrap().len());
        let st: State = serde_json::from_value(second.cursor.state.clone()).unwrap();
        assert_eq!(st.id.as_deref(), Some("s1"));
        assert_eq!(st.names.get("c1").map(String::as_str), Some("Bash"));
        let third = a.read(&p, Some(&second.cursor)).unwrap();
        assert!(third.batch.items.is_empty());
        assert_eq!(third.batch.bytes, 0);
    }

    #[test]
    fn torn_trailing_frame_is_retried_not_skipped() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let mut lines = full_turn();
        lines.push(rec("turn/start", 12, json!({"turn":2})));
        let p = session(&fx, "session.jsonl.zstd", &frames(&lines[..lines.len() - 1]));
        let base = a.read(&p, None).unwrap();
        let torn = frame(lines.last().unwrap());
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(&torn[..torn.len() - 3]).unwrap();
        let partial = a.read(&p, Some(&base.cursor)).unwrap();
        assert_eq!(partial.batch.items.len(), 0, "torn frame must not be consumed");
        assert_eq!(partial.cursor.offset, base.cursor.offset);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(&torn[torn.len() - 3..]).unwrap();
        let fixed = a.read(&p, Some(&partial.cursor)).unwrap();
        assert_eq!(fixed.batch.items.len(), 1);
        assert!(matches!(&fixed.batch.items[0], (_, Record::Event(e)) if e.id == "t2:start"));
    }

    #[test]
    fn rewritten_source_resets() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let p = session(&fx, "session.jsonl.zstd", &frames(&full_turn()));
        let first = a.read(&p, None).unwrap();
        std::fs::write(&p, frames(&[header("s1")])).unwrap();
        let second = a.read(&p, Some(&first.cursor)).unwrap();
        assert!(second.reset);
        assert!(second.summary);
    }

    #[test]
    fn history_pages_backward_with_ids_and_sessions() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let mut lines = vec![header("s1")];
        for t in 1..=10i64 {
            lines.push(rec("turn/start", 100 + t, json!({"turn":t})));
            lines.push(rec(
                "user/message",
                100 + t,
                json!({"id":format!("u{t}"),"role":"user","source":{"kind":"user"},"content":[{"type":"text","text":format!("q{t}")}]}),
            ));
            lines.push(rec(
                "turn/end",
                100 + t,
                json!({"turn":t,"reason":{"kind":"aborted","reason":{"kind":"user"}}}),
            ));
        }
        let p = session(&fx, "session.jsonl.zstd", &frames(&lines));
        let page = a.history(&p, "s1", &HistoryQuery { before: None, limit: 4 }).unwrap();
        assert!(page.len() >= 3, "{page:?}");
        let ids: Vec<&str> = page.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"u10"), "{ids:?}");
        assert!(page.iter().all(|e| e.session == "dsh:s1"));
        assert!(page.windows(2).all(|w| w[0].pos.unwrap() <= w[1].pos.unwrap()));
        assert!(matches!(&page[page.len() - 1].body, Body::TurnEnd { reason: Some(r) } if r == "aborted"));
        let earlier = a.history(&p, "s1", &HistoryQuery { before: page[0].pos, limit: 4 }).unwrap();
        assert!(earlier.iter().all(|e| e.pos < page[0].pos));
        assert!(earlier.iter().any(|e| e.id == "u9"), "{:?}", earlier.iter().map(|e| &e.id).collect::<Vec<_>>());
    }

    #[test]
    fn unknown_types_are_reported() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let p = session(&fx, "session.jsonl.zstd", &frames(&[header("s1"), rec("weird/new", 2, json!({}))]));
        let r = fx.index(&a, &p);
        assert_eq!(r.unknown, vec!["type=weird/new".to_string()]);
    }

    #[test]
    fn open_turn_without_end_reads_as_work() {
        let fx = Fixture::new();
        let a = dsh(&fx);
        let lines = full_turn();
        let p = session(&fx, "session.jsonl.zstd", &frames(&lines[..lines.len() - 1]));
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
    }
}
