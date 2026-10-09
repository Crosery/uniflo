//! Generic driver for "one file = one session" transcript formats.
//!
//! A harness only implements [`LineDecoder`] (record → normalized records); this module
//! provides discovery, head+tail summary scans, byte-offset following, partial-line
//! safety, rewrite detection and backward paging for history.

use crate::adapter::{
    Adapter, Batch, Cursor, HarnessInfo, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record, walk,
};
use crate::util::file_mtime_ms;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use uniflo_schema::{Body, Event, session_key};

/// Summary scan reads this much from the start (grown until one full line fits).
pub const HEAD_BYTES: u64 = 256 * 1024;
/// …and this much from the end (grown until one full line fits).
pub const TAIL_BYTES: u64 = 256 * 1024;
/// Appends larger than this since the cursor are summarized instead of replayed.
pub const CATCHUP_MAX: u64 = 16 * 1024 * 1024;
const MAX_WINDOW: u64 = 64 * 1024 * 1024;
const REV_CHUNK: u64 = 128 * 1024;
/// Window of one [`Adapter::read_all`] step.
const SCAN_CHUNK: u64 = 4 * 1024 * 1024;

/// Native identity of the session a file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceId {
    pub id: String,
    pub parent: Option<String>,
}

pub trait LineDecoder: Send + Sync + 'static {
    /// Per-source decoding state carried across lines (e.g. pending tool calls).
    type State: Default + Serialize + DeserializeOwned + Send;

    fn info(&self) -> HarnessInfo;
    fn roots(&self) -> Vec<PathBuf>;
    fn is_source(&self, path: &Path) -> bool;
    fn identify(&self, path: &Path) -> Option<SourceId>;
    fn decode(&self, record: &Value, cx: &mut Cx<'_, Self::State>);

    /// End of a contiguous run of records (one read window): emit what is still open, e.g. a
    /// message streamed as fragments. `more`: the window is a history page ending where a
    /// newer page begins, so nothing open continues past it and it is complete; otherwise
    /// emit it as `partial`, and its completed version reuses the id.
    fn finish(&self, _cx: &mut Cx<'_, Self::State>, _more: bool) {}

    /// Whether a history page may begin at `record`, `prev` being the record before it:
    /// decoding from there with fresh state must give the events a whole-file decode gives.
    /// Decoders whose events span records (merged fragments, turn numbering) say `false`
    /// inside such a span; pages then reach back to the previous boundary.
    fn page_start(&self, _prev: &Value, _record: &Value) -> bool {
        true
    }

    /// The file is one JSON document rewritten in place instead of appended lines.
    fn whole_file(&self, _path: &Path) -> bool {
        false
    }
    fn max_depth(&self) -> usize {
        8
    }
    fn live(&self) -> Option<Vec<LiveSession>> {
        None
    }
    fn live_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    /// See [`Adapter::cleanup_targets`]; `None` (default) = cleanup unsupported.
    fn cleanup_targets(&self, _src: &Path) -> Option<Vec<PathBuf>> {
        None
    }
}

/// Decode context handed to [`LineDecoder::decode`] for one record.
pub struct Cx<'a, S> {
    /// Session key (`harness:id`).
    pub key: &'a str,
    /// Source file being decoded.
    pub src: &'a Path,
    /// Byte offset of the record (or ordinal for whole-file sources).
    pub pos: u64,
    pub state: &'a mut S,
    sink: &'a mut Sink,
    n: u32,
}

impl<S> Cx<'_, S> {
    /// Emit an event with a decoder-chosen stable id.
    pub fn emit(&mut self, id: impl Into<String>, ts: i64, body: Body) -> &mut Event {
        self.sink.records.push(Record::Event(Event {
            id: id.into(),
            session: self.key.to_owned(),
            ts,
            pos: Some(self.pos),
            partial: false,
            truncated: false,
            body,
        }));
        match self.sink.records.last_mut() {
            Some(Record::Event(e)) => e,
            _ => unreachable!(),
        }
    }

    /// Emit an event whose id is derived from its byte position (stable for append-only files).
    pub fn emit_at(&mut self, ts: i64, body: Body) -> &mut Event {
        let id = format!("o{}.{}", self.pos, self.n);
        self.n += 1;
        self.emit(id, ts, body)
    }

    pub fn meta(&mut self) -> &mut MetaPatch {
        &mut self.sink.meta
    }

    pub fn unknown(&mut self, what: impl Into<String>) {
        self.sink.unknown.push(what.into());
    }
}

#[derive(Default)]
struct Sink {
    records: Vec<Record>,
    unknown: Vec<String>,
    meta: MetaPatch,
    bad: u64,
}

impl Sink {
    fn flush_meta(&mut self) {
        if !self.meta.is_empty() {
            self.records.push(Record::Meta(std::mem::take(&mut self.meta)));
        }
    }

    fn into_batch(mut self, id: &str, bytes: u64) -> Batch {
        self.flush_meta();
        Batch {
            items: self.records.into_iter().map(|r| (id.to_owned(), r)).collect(),
            unknown: self.unknown,
            bad_lines: self.bad,
            bytes,
        }
    }
}

/// Where decoded records come from.
#[derive(Clone, Copy)]
struct At<'a> {
    src: &'a Path,
    key: &'a str,
}

pub struct JsonlAdapter<D: LineDecoder> {
    pub decoder: D,
}

impl<D: LineDecoder> JsonlAdapter<D> {
    pub fn new(decoder: D) -> Self {
        Self { decoder }
    }

    fn decode_value(&self, v: &Value, at: At<'_>, pos: u64, state: &mut D::State, sink: &mut Sink) {
        let mut cx = Cx { key: at.key, src: at.src, pos, state, sink, n: 0 };
        self.decoder.decode(v, &mut cx);
        sink.flush_meta();
    }

    /// Decode every complete line in `buf` (which starts at file offset `base`).
    /// Returns bytes consumed; an unterminated tail is consumed only if it is a full JSON value.
    fn decode_lines(&self, buf: &[u8], base: u64, at: At<'_>, state: &mut D::State, sink: &mut Sink, eof: bool) -> u64 {
        let mut start = 0usize;
        for nl in memchr::memchr_iter(b'\n', buf) {
            self.decode_line(&buf[start..nl], base + start as u64, at, state, sink);
            start = nl + 1;
        }
        if eof && start < buf.len() {
            let rest = trim_line(&buf[start..]);
            if rest.first() == Some(&b'{')
                && let Ok(v) = serde_json::from_slice::<Value>(rest)
            {
                self.decode_value(&v, at, base + start as u64, state, sink);
                start = buf.len();
            }
        }
        if start > 0 {
            self.finish(at, base + start as u64, state, sink, false);
        }
        start as u64
    }

    fn finish(&self, at: At<'_>, pos: u64, state: &mut D::State, sink: &mut Sink, more: bool) {
        let mut cx = Cx { key: at.key, src: at.src, pos, state, sink, n: 0 };
        self.decoder.finish(&mut cx, more);
        sink.flush_meta();
    }

    /// [`LineDecoder::page_start`] on raw lines; an unparsable line is a boundary.
    fn page_start(&self, prev: &[u8], line: &[u8]) -> bool {
        let parse = |l: &[u8]| serde_json::from_slice::<Value>(trim_line(l)).ok();
        match (parse(prev), parse(line)) {
            (Some(p), Some(l)) => self.decoder.page_start(&p, &l),
            _ => true,
        }
    }

    fn decode_line(&self, line: &[u8], pos: u64, at: At<'_>, state: &mut D::State, sink: &mut Sink) {
        let line = trim_line(line);
        if line.is_empty() {
            return;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(v) => self.decode_value(&v, at, pos, state, sink),
            Err(_) => sink.bad += 1,
        }
    }

    /// Returns (bytes consumed up to, decoder state, bytes actually read).
    fn summary(&self, src: &Path, size: u64, key: &str, sink: &mut Sink) -> Result<(u64, D::State, u64)> {
        let mut f = File::open(src)?;
        let head = read_window_fwd(&mut f, 0, size, HEAD_BYTES)?;
        let mut state = D::State::default();
        let head_end = self.decode_lines(&head, 0, At { src, key }, &mut state, sink, head.len() as u64 == size);
        if head.len() as u64 >= size {
            return Ok((head_end, state, head.len() as u64));
        }
        let (tail_base, tail) = read_window_back(&mut f, head_end, size, TAIL_BYTES)?;
        let mut tstate = D::State::default();
        let used = self.decode_lines(&tail, tail_base, At { src, key }, &mut tstate, sink, true);
        Ok((tail_base + used, tstate, (head.len() + tail.len()) as u64))
    }

    fn read_whole(
        &self,
        src: &Path,
        cursor: Option<&Cursor>,
        key: &str,
        id: &str,
        size: u64,
        mtime: i64,
    ) -> Result<ReadOutput> {
        let bytes = std::fs::read(src)?;
        let mut sink = Sink::default();
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => {
                let mut state = D::State::default();
                self.decode_value(&v, At { src, key }, 0, &mut state, &mut sink);
            }
            Err(_) => sink.bad += 1,
        }
        number_positions(&mut sink.records);
        let mut seen: HashMap<String, u64> = cursor
            .and_then(|c| serde_json::from_value(c.state.get("h").cloned().unwrap_or_default()).ok())
            .unwrap_or_default();
        let follow = cursor.is_some();
        if follow {
            // Only changed or new events are news; metadata is cheap to re-apply.
            sink.records.retain(|r| match r {
                Record::Event(e) => {
                    let h = hash_event(e);
                    seen.insert(e.id.clone(), h) != Some(h)
                }
                Record::Meta(_) => true,
            });
        } else {
            for r in &sink.records {
                if let Record::Event(e) = r {
                    seen.insert(e.id.clone(), hash_event(e));
                }
            }
        }
        Ok(ReadOutput {
            cursor: Cursor { offset: size, size, mtime_ms: mtime, state: serde_json::json!({ "h": seen }) },
            batch: sink.into_batch(id, bytes.len() as u64),
            summary: !follow,
            reset: false,
        })
    }
}

impl<D: LineDecoder> Adapter for JsonlAdapter<D> {
    fn info(&self) -> HarnessInfo {
        self.decoder.info()
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.decoder.roots().into_iter().filter(|p| p.is_dir()).collect()
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        self.decoder.is_source(path).then(|| path.to_path_buf())
    }

    fn discover(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for r in self.roots() {
            walk(&r, self.decoder.max_depth(), &|p| self.decoder.is_source(p), &mut out);
        }
        out
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let md = std::fs::metadata(src).with_context(|| format!("stat {}", src.display()))?;
        let (size, mtime) = (md.len(), file_mtime_ms(&md));
        let sid =
            self.decoder.identify(src).ok_or_else(|| anyhow!("not a {} source: {}", self.info().id, src.display()))?;
        let key = session_key(self.info().id, &sid.id);
        if self.decoder.whole_file(src) {
            let mut out = self.read_whole(src, cursor, &key, &sid.id, size, mtime)?;
            prepend_parent(&mut out.batch, &sid);
            return Ok(out);
        }

        let mut sink = Sink::default();
        let mut out = ReadOutput::default();
        let bytes;
        match cursor {
            Some(c) if size >= c.offset && size - c.offset <= CATCHUP_MAX => {
                let mut state: D::State = serde_json::from_value(c.state.clone()).unwrap_or_default();
                let mut f = File::open(src)?;
                let buf = read_range(&mut f, c.offset, size)?;
                bytes = buf.len() as u64;
                let used = self.decode_lines(&buf, c.offset, At { src, key: &key }, &mut state, &mut sink, true);
                out.cursor = Cursor { offset: c.offset + used, size, mtime_ms: mtime, state: to_state(&state) };
            }
            other => {
                out.reset = matches!(other, Some(c) if size < c.offset);
                out.summary = true;
                let (end, state, read) = self.summary(src, size, &key, &mut sink)?;
                bytes = read;
                out.cursor = Cursor { offset: end, size, mtime_ms: mtime, state: to_state(&state) };
            }
        }
        out.batch = sink.into_batch(&sid.id, bytes);
        prepend_parent(&mut out.batch, &sid);
        Ok(out)
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let key = session_key(self.info().id, session_id);
        let limit = q.limit.max(1);
        if self.decoder.whole_file(src) {
            let out = self.read_whole(src, None, &key, session_id, 0, 0)?;
            let mut evs: Vec<Event> = out
                .batch
                .items
                .into_iter()
                .filter_map(|(_, r)| match r {
                    Record::Event(e) => Some(e),
                    _ => None,
                })
                .filter(|e| q.before.is_none_or(|b| e.pos.unwrap_or(0) < b))
                .collect();
            let skip = evs.len().saturating_sub(limit);
            return Ok(evs.split_off(skip));
        }

        let mut f = File::open(src)?;
        let size = f.metadata()?.len();
        let end = q.before.map_or(size, |b| b.min(size));
        let mut rev = RevLines::new(&mut f, end);
        // Newest first. Past `limit`, keep going back until the oldest line can start a page.
        let mut lines: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut estimate = 0usize;
        while let Some((pos, line)) = rev.next_line()? {
            if estimate >= limit && lines.last().is_none_or(|(_, newer)| self.page_start(&line, newer)) {
                break;
            }
            // Per line with fresh state; fragments still open at its end are not counted.
            let mut probe = Sink::default();
            let mut st = D::State::default();
            self.decode_line(&line, pos, At { src, key: &key }, &mut st, &mut probe);
            estimate += probe.records.iter().filter(|r| matches!(r, Record::Event(_))).count();
            lines.push((pos, line));
        }
        lines.reverse();
        let mut sink = Sink::default();
        let mut state = D::State::default();
        for (pos, line) in &lines {
            self.decode_line(line, *pos, At { src, key: &key }, &mut state, &mut sink);
        }
        self.finish(At { src, key: &key }, end, &mut state, &mut sink, end < size);
        // A merged message sits at its first fragment though it is emitted when it closes;
        // ordered by position, the first event's `pos` is where the next older page ends.
        let mut evs = dedupe_events(sink.records);
        evs.sort_by_key(|e| e.pos);
        Ok(evs)
    }

    fn read_all(&self, src: &Path, _sessions: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let md = std::fs::metadata(src).with_context(|| format!("stat {}", src.display()))?;
        let (size, mtime) = (md.len(), file_mtime_ms(&md));
        let sid =
            self.decoder.identify(src).ok_or_else(|| anyhow!("not a {} source: {}", self.info().id, src.display()))?;
        let key = session_key(self.info().id, &sid.id);
        if self.decoder.whole_file(src) {
            let mut out = self.read_whole(src, None, &key, &sid.id, size, mtime)?;
            prepend_parent(&mut out.batch, &sid);
            for (id, r) in out.batch.items {
                sink(&id, r);
            }
            return Ok(out.cursor);
        }
        if let Some(p) = &sid.parent {
            sink(&sid.id, Record::Meta(MetaPatch { parent: Some(p.clone()), ..Default::default() }));
        }
        // Bounded windows keep memory flat on multi-GB transcripts; a window grows only when a
        // single line does not fit.
        let mut f = File::open(src)?;
        let mut state = D::State::default();
        let (mut offset, mut want) = (0u64, SCAN_CHUNK);
        while offset < size {
            let end = (offset + want).min(size);
            let buf = read_range(&mut f, offset, end)?;
            let eof = end >= size;
            let mut part = Sink::default();
            let used = self.decode_lines(&buf, offset, At { src, key: &key }, &mut state, &mut part, eof);
            for r in part.records {
                sink(&sid.id, r);
            }
            if used == 0 {
                if eof {
                    break;
                }
                want = want.saturating_mul(2);
                continue;
            }
            offset += used;
            want = SCAN_CHUNK;
            if eof {
                break;
            }
        }
        Ok(Cursor { offset, size, mtime_ms: mtime, state: to_state(&state) })
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        self.decoder.live()
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        self.decoder.live_roots().into_iter().filter(|p| p.is_dir()).collect()
    }

    fn cleanup_targets(&self, src: &Path, _id: &str) -> Option<Vec<PathBuf>> {
        self.decoder.cleanup_targets(src)
    }
}

fn to_state<S: Serialize>(s: &S) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

fn prepend_parent(batch: &mut Batch, sid: &SourceId) {
    if let Some(p) = &sid.parent {
        let meta = MetaPatch { parent: Some(p.clone()), ..Default::default() };
        batch.items.insert(0, (sid.id.clone(), Record::Meta(meta)));
    }
}

fn number_positions(records: &mut [Record]) {
    let mut i = 0;
    for r in records {
        if let Record::Event(e) = r {
            i += 1;
            e.pos = Some(i);
        }
    }
}

fn hash_event(e: &Event) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(e).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// Records and unknown discriminators produced by [`decode_record`].
#[derive(Debug, Default)]
pub struct Decoded {
    pub records: Vec<Record>,
    pub unknown: Vec<String>,
}

/// Feed one record that does not come from a JSONL file (e.g. a database row holding the
/// same entry format) through `decoder`; `pos` is the caller's ordinal for it.
pub fn decode_record<D: LineDecoder>(
    decoder: &D,
    key: &str,
    src: &Path,
    pos: u64,
    v: &Value,
    state: &mut D::State,
) -> Decoded {
    let mut sink = Sink::default();
    let mut cx = Cx { key, src, pos, state, sink: &mut sink, n: 0 };
    decoder.decode(v, &mut cx);
    sink.flush_meta();
    Decoded { records: sink.records, unknown: sink.unknown }
}

/// Keep one event per id: first position, latest content (in-place streaming updates).
pub fn dedupe_events(records: Vec<Record>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut idx: HashMap<String, usize> = HashMap::new();
    for r in records {
        let Record::Event(e) = r else { continue };
        match idx.get(&e.id) {
            Some(&i) => {
                let pos = out[i].pos;
                out[i] = e;
                out[i].pos = pos;
            }
            None => {
                idx.insert(e.id.clone(), out.len());
                out.push(e);
            }
        }
    }
    out
}

fn trim_line(mut l: &[u8]) -> &[u8] {
    while let [rest @ .., b'\r' | b' ' | b'\t'] = l {
        l = rest;
    }
    l
}

fn read_range(f: &mut File, start: u64, end: u64) -> Result<Vec<u8>> {
    let len = end.saturating_sub(start) as usize;
    let mut buf = vec![0u8; len];
    f.seek(SeekFrom::Start(start))?;
    let mut got = 0;
    while got < len {
        let n = f.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

/// Read from `start`, growing the window until it holds at least one newline (or EOF / cap).
fn read_window_fwd(f: &mut File, start: u64, size: u64, initial: u64) -> Result<Vec<u8>> {
    let mut want = initial;
    loop {
        let end = (start + want).min(size);
        let buf = read_range(f, start, end)?;
        if end >= size || memchr::memchr(b'\n', &buf).is_some() || want >= MAX_WINDOW {
            return Ok(buf);
        }
        want *= 2;
    }
}

/// Read the last `initial` bytes (never before `floor`), aligned to a line start.
/// Grows the window when a single line is longer than it.
fn read_window_back(f: &mut File, floor: u64, size: u64, initial: u64) -> Result<(u64, Vec<u8>)> {
    let mut want = initial;
    loop {
        let start = size.saturating_sub(want).max(floor);
        if start == floor {
            return Ok((start, read_range(f, start, size)?));
        }
        // Include one byte before `start` to know whether `start` is already a line start.
        let buf = read_range(f, start - 1, size)?;
        if let Some(i) = memchr::memchr(b'\n', &buf) {
            let base = start - 1 + i as u64 + 1;
            let skip = i + 1;
            if skip < buf.len() || base == size {
                return Ok((base, buf[skip..].to_vec()));
            }
        }
        if want >= MAX_WINDOW {
            // One gigantic trailing line: give up on it, resume from the end.
            return Ok((size, Vec::new()));
        }
        want *= 2;
    }
}

/// Iterates lines backwards from `end` (exclusive), yielding `(line_start_offset, bytes)`.
pub struct RevLines<'f> {
    f: &'f mut File,
    buf: Vec<u8>,
    buf_start: u64,
}

impl<'f> RevLines<'f> {
    pub fn new(f: &'f mut File, end: u64) -> Self {
        Self { f, buf: Vec::new(), buf_start: end }
    }

    pub fn next_line(&mut self) -> std::io::Result<Option<(u64, Vec<u8>)>> {
        loop {
            let body = match self.buf.last() {
                Some(b'\n') => self.buf.len() - 1,
                _ => self.buf.len(),
            };
            if let Some(i) = memchr::memrchr(b'\n', &self.buf[..body]) {
                let line = self.buf[i + 1..body].to_vec();
                let pos = self.buf_start + i as u64 + 1;
                self.buf.truncate(i + 1);
                return Ok(Some((pos, line)));
            }
            if self.buf_start == 0 {
                if body == 0 {
                    return Ok(None);
                }
                let line = self.buf[..body].to_vec();
                self.buf.clear();
                return Ok(Some((0, line)));
            }
            let chunk = REV_CHUNK.max(self.buf.len() as u64);
            let new_start = self.buf_start.saturating_sub(chunk);
            let mut head = read_range(self.f, new_start, self.buf_start).map_err(std::io::Error::other)?;
            head.extend_from_slice(&self.buf);
            self.buf = head;
            self.buf_start = new_start;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::Write;
    use uniflo_schema::Status;

    /// Minimal decoder: `{"t":"u"|"a"|"end"|"meta", "id", "x", "ts"}`; state counts lines.
    struct Toy {
        root: PathBuf,
    }

    #[derive(Default, Serialize, Deserialize)]
    struct Count(u64);

    impl LineDecoder for Toy {
        type State = Count;
        fn info(&self) -> HarnessInfo {
            HarnessInfo { id: "toy", name: "Toy" }
        }
        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }
        fn is_source(&self, p: &Path) -> bool {
            p.extension().is_some_and(|e| e == "jsonl" || e == "json")
        }
        fn identify(&self, p: &Path) -> Option<SourceId> {
            let stem = p.file_stem()?.to_str()?.to_owned();
            let parent = p.parent()?.file_name()?.to_str().filter(|s| s.starts_with("p-")).map(|s| s[2..].to_owned());
            Some(SourceId { id: stem, parent })
        }
        fn whole_file(&self, p: &Path) -> bool {
            p.extension().is_some_and(|e| e == "json")
        }
        fn decode(&self, v: &Value, cx: &mut Cx<'_, Count>) {
            if let Some(msgs) = v.get("messages").and_then(Value::as_array) {
                for m in msgs {
                    self.decode(m, cx);
                }
                return;
            }
            cx.state.0 += 1;
            let ts = v.get("ts").and_then(Value::as_i64).unwrap_or(0);
            let x = v.get("x").and_then(Value::as_str).unwrap_or("").to_owned();
            let id = v.get("id").and_then(Value::as_str).map(str::to_owned);
            let body = match v.get("t").and_then(Value::as_str) {
                Some("u") => Body::UserMessage { text: x, synthetic: false },
                Some("a") => Body::AssistantMessage { text: x, model: None },
                Some("end") => Body::TurnEnd { reason: None },
                Some("meta") => {
                    cx.meta().cwd = Some(x);
                    return;
                }
                other => {
                    cx.unknown(format!("t={other:?}"));
                    return;
                }
            };
            match id {
                Some(id) => cx.emit(id, ts, body),
                None => cx.emit_at(ts, body),
            };
        }
    }

    fn line(t: &str, x: &str, ts: i64) -> String {
        format!("{{\"t\":\"{t}\",\"x\":\"{x}\",\"ts\":{ts}}}\n")
    }

    fn events(out: &ReadOutput) -> Vec<&Event> {
        out.batch
            .items
            .iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    fn setup() -> (tempfile::TempDir, JsonlAdapter<Toy>, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let a = JsonlAdapter::new(Toy { root: dir.path().to_path_buf() });
        let p = dir.path().join("s1.jsonl");
        (dir, a, p)
    }

    #[test]
    fn follow_handles_partial_lines_and_appends() {
        let (_d, a, p) = setup();
        std::fs::write(&p, format!("{}{}", line("meta", "/w", 1), line("u", "hi", 2))).unwrap();
        let out = a.read(&p, None).unwrap();
        assert!(out.summary);
        assert_eq!(events(&out).len(), 1);
        assert!(out.batch.items.iter().any(|(_, r)| matches!(r, Record::Meta(m) if m.cwd.as_deref() == Some("/w"))));
        let c1 = out.cursor.clone();
        assert_eq!(c1.offset, std::fs::metadata(&p).unwrap().len());
        assert_eq!(c1.state, serde_json::json!(2));

        // A writer flushes half a line: nothing is consumed yet.
        let full = line("a", "yo", 3);
        let (h1, h2) = full.split_at(7);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(h1.as_bytes()).unwrap();
        let out = a.read(&p, Some(&c1)).unwrap();
        assert!(!out.summary && events(&out).is_empty() && out.batch.bad_lines == 0);
        assert_eq!(out.cursor.offset, c1.offset);

        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(h2.as_bytes()).unwrap();
        let out = a.read(&p, Some(&out.cursor)).unwrap();
        let evs = events(&out);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].pos, Some(c1.offset));
        assert_eq!(evs[0].id, format!("o{}.0", c1.offset));
        assert_eq!(out.cursor.state, serde_json::json!(3), "state carried across reads");
    }

    #[test]
    fn unterminated_complete_object_is_consumed() {
        let (_d, a, p) = setup();
        std::fs::write(&p, "{\"t\":\"u\",\"x\":\"a\",\"ts\":1}").unwrap();
        let out = a.read(&p, None).unwrap();
        assert_eq!(events(&out).len(), 1);
        assert_eq!(out.cursor.offset, std::fs::metadata(&p).unwrap().len());
    }

    #[test]
    fn rewrite_triggers_reset() {
        let (_d, a, p) = setup();
        std::fs::write(&p, line("u", "aaaaaaaaaaaaaaaaaaaa", 1)).unwrap();
        let c = a.read(&p, None).unwrap().cursor;
        std::fs::write(&p, line("u", "b", 2)).unwrap();
        let out = a.read(&p, Some(&c)).unwrap();
        assert!(out.reset && out.summary);
        assert_eq!(events(&out).len(), 1);
    }

    #[test]
    fn summary_reads_head_and_tail_of_big_files() {
        let (_d, a, p) = setup();
        let mut f = File::create(&p).unwrap();
        f.write_all(line("u", "first", 1).as_bytes()).unwrap();
        let pad = "x".repeat(1000);
        for i in 0..2000 {
            f.write_all(line("a", &pad, 10 + i).as_bytes()).unwrap();
        }
        f.write_all(line("end", "", 99_999).as_bytes()).unwrap();
        drop(f);
        let out = a.read(&p, None).unwrap();
        let evs = events(&out);
        assert!(evs.len() < 1000, "middle skipped: {}", evs.len());
        assert!(matches!(&evs[0].body, Body::UserMessage { text, .. } if text == "first"));
        assert!(matches!(evs.last().unwrap().body, Body::TurnEnd { .. }));
        assert_eq!(out.cursor.offset, std::fs::metadata(&p).unwrap().len());
        assert_eq!(out.batch.bad_lines, 0, "tail window must start on a line boundary");
        let mut t = crate::status::StatusTracker::default();
        for e in evs {
            t.observe(e);
        }
        assert_eq!(t.status, Status::Idle);
    }

    #[test]
    fn summary_survives_lines_longer_than_windows() {
        let (_d, a, p) = setup();
        let huge = "y".repeat((HEAD_BYTES + TAIL_BYTES) as usize);
        std::fs::write(&p, format!("{}{}{}", line("u", &huge, 1), line("a", &huge, 2), line("end", "", 3))).unwrap();
        let out = a.read(&p, None).unwrap();
        let kinds: Vec<_> = events(&out).iter().map(|e| e.body.kind()).collect();
        assert_eq!(kinds.first(), Some(&"user_message"));
        assert_eq!(kinds.last(), Some(&"turn_end"));
        assert_eq!(out.batch.bad_lines, 0);
    }

    #[test]
    fn history_pages_backwards_without_gaps() {
        let (_d, a, p) = setup();
        let mut s = String::new();
        for i in 0..50 {
            s.push_str(&line("u", &format!("m{i}"), i));
        }
        std::fs::write(&p, s).unwrap();
        let mut before = None;
        let mut seen = Vec::new();
        loop {
            let page = a.history(&p, "s1", &HistoryQuery { before, limit: 7 }).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.windows(2).all(|w| w[0].pos < w[1].pos), "chronological");
            before = page[0].pos;
            for e in page.into_iter().rev() {
                if let Body::UserMessage { text, .. } = e.body {
                    seen.push(text);
                }
            }
        }
        seen.reverse();
        assert_eq!(seen, (0..50).map(|i| format!("m{i}")).collect::<Vec<_>>());
    }

    #[test]
    fn history_dedupes_upserts_keeping_first_position_latest_content() {
        let (_d, a, p) = setup();
        let s = "{\"t\":\"a\",\"id\":\"m1\",\"x\":\"par\",\"ts\":1}\n{\"t\":\"u\",\"id\":\"m2\",\"x\":\"q\",\"ts\":2}\n{\"t\":\"a\",\"id\":\"m1\",\"x\":\"partial done\",\"ts\":3}\n";
        std::fs::write(&p, s).unwrap();
        let h = a.history(&p, "s1", &HistoryQuery { before: None, limit: 10 }).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].id, "m1");
        assert!(matches!(&h[0].body, Body::AssistantMessage { text, .. } if text == "partial done"));
        assert_eq!(h[0].pos, Some(0));
    }

    #[test]
    fn whole_file_sources_emit_only_changes_on_follow() {
        let (d, a, _) = setup();
        let p = d.path().join("w.json");
        std::fs::write(&p, r#"{"messages":[{"t":"u","id":"1","x":"a","ts":1}]}"#).unwrap();
        let out = a.read(&p, None).unwrap();
        assert_eq!(events(&out).len(), 1);
        std::fs::write(&p, r#"{"messages":[{"t":"u","id":"1","x":"a","ts":1},{"t":"a","id":"2","x":"b","ts":2}]}"#)
            .unwrap();
        let out2 = a.read(&p, Some(&out.cursor)).unwrap();
        let evs = events(&out2);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].id, "2");
        let h = a.history(&p, "w", &HistoryQuery { before: Some(2), limit: 5 }).unwrap();
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn identity_parent_and_discovery() {
        let (d, a, _) = setup();
        let sub = d.path().join("p-root1");
        std::fs::create_dir_all(&sub).unwrap();
        let child = sub.join("kid.jsonl");
        std::fs::write(&child, line("u", "x", 1)).unwrap();
        std::fs::write(d.path().join("ignore.txt"), "").unwrap();
        let found = a.discover();
        assert_eq!(found, vec![child.clone()]);
        let out = a.read(&child, None).unwrap();
        assert!(
            matches!(&out.batch.items[0], (id, Record::Meta(m)) if id == "kid" && m.parent.as_deref() == Some("root1"))
        );
    }

    #[test]
    fn bad_lines_and_unknown_are_counted() {
        let (_d, a, p) = setup();
        std::fs::write(&p, "not json\n{\"t\":\"zzz\"}\n").unwrap();
        let out = a.read(&p, None).unwrap();
        assert_eq!(out.batch.bad_lines, 1);
        assert_eq!(out.batch.unknown, vec!["t=Some(\"zzz\")".to_string()]);
    }

    #[test]
    fn read_all_streams_every_line_and_resumes() {
        let (_d, a, p) = setup();
        let mut s = line("meta", "/w", 0);
        let long = "z".repeat((SCAN_CHUNK + 10) as usize);
        s.push_str(&line("u", &long, 1));
        for i in 0..2000 {
            s.push_str(&line("a", &"x".repeat(5000), 10 + i));
        }
        std::fs::write(&p, &s).unwrap();
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{\"t\":\"u\",\"x\":\"ha").unwrap();
        let mut n = 0;
        let mut metas = 0;
        let c = a
            .read_all(&p, &[], &mut |id, r| {
                assert_eq!(id, "s1");
                match r {
                    Record::Event(_) => n += 1,
                    Record::Meta(_) => metas += 1,
                }
            })
            .unwrap();
        assert_eq!((n, metas), (2001, 1), "middle of a big file is read, not sampled");
        assert_eq!(c.offset, s.len() as u64, "torn last line left for the next read");
        assert_eq!(c.state, serde_json::json!(2002));
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"llo\",\"ts\":9}\n").unwrap();
        let out = a.read(&p, Some(&c)).unwrap();
        assert!(!out.summary);
        assert_eq!(events(&out).len(), 1);
    }

    #[test]
    fn rev_lines_handles_no_trailing_newline_and_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "a\r\nbb\n\nccc").unwrap();
        let mut f = File::open(&p).unwrap();
        let mut r = RevLines::new(&mut f, 10);
        let mut got = Vec::new();
        while let Some((pos, l)) = r.next_line().unwrap() {
            got.push((pos, String::from_utf8(l).unwrap()));
        }
        assert_eq!(got, vec![(7, "ccc".into()), (6, "".into()), (3, "bb".into()), (0, "a\r".into())]);
    }

    /// Fragments `{"f":"…"}` accumulate in state until `{"t":"end"}`; open text is flushed partial.
    /// The message sits at its first fragment; `{"tick":1}` is an event inside a run. Pages
    /// start only after an end.
    struct Frag;

    impl LineDecoder for Frag {
        type State = Option<(u64, String)>;
        fn info(&self) -> HarnessInfo {
            HarnessInfo { id: "frag", name: "Frag" }
        }
        fn roots(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn is_source(&self, _: &Path) -> bool {
            true
        }
        fn identify(&self, p: &Path) -> Option<SourceId> {
            Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
        }
        fn decode(&self, v: &Value, cx: &mut Cx<'_, Self::State>) {
            match v.get("f").and_then(Value::as_str) {
                Some(f) => cx.state.get_or_insert_with(|| (cx.pos, String::new())).1.push_str(f),
                None if v.get("tick").is_some() => {
                    cx.emit_at(0, Body::System { subtype: "tick".into(), text: String::new() });
                }
                None => {
                    if let Some((at, text)) = cx.state.take() {
                        cx.emit(format!("m{at}"), 0, Body::AssistantMessage { text, model: None }).pos = Some(at);
                    }
                    cx.emit_at(0, Body::TurnEnd { reason: None });
                }
            }
        }
        fn finish(&self, cx: &mut Cx<'_, Self::State>, more: bool) {
            if let Some((at, text)) = cx.state.clone() {
                let e = cx.emit(format!("m{at}"), 0, Body::AssistantMessage { text, model: None });
                (e.pos, e.partial) = (Some(at), !more);
            }
        }
        fn page_start(&self, prev: &Value, _record: &Value) -> bool {
            prev.get("t").is_some()
        }
    }

    #[test]
    fn finish_flushes_open_fragments_as_partial_then_completes_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let a = JsonlAdapter::new(Frag);
        let p = dir.path().join("f.jsonl");
        std::fs::write(&p, "{\"f\":\"he\"}\n{\"f\":\"llo \"}\n").unwrap();
        let out = a.read(&p, None).unwrap();
        let evs = events(&out);
        assert_eq!(evs.len(), 1);
        assert!(evs[0].partial);
        assert!(matches!(&evs[0].body, Body::AssistantMessage { text, .. } if text == "hello "));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"{\"f\":\"you\"}\n{\"t\":\"end\"}\n")
            .unwrap();
        let out2 = a.read(&p, Some(&out.cursor)).unwrap();
        let evs2 = events(&out2);
        assert_eq!(evs2.len(), 2);
        assert_eq!(evs2[0].id, evs[0].id, "completed version replaces the partial one");
        assert!(!evs2[0].partial);
        assert!(matches!(&evs2[0].body, Body::AssistantMessage { text, .. } if text == "hello you"));
        let h = a.history(&p, "f", &HistoryQuery { before: None, limit: 10 }).unwrap();
        assert_eq!(h.iter().map(|e| e.body.kind()).collect::<Vec<_>>(), ["assistant_message", "turn_end"]);
    }

    #[test]
    fn history_pages_never_start_inside_a_fragment_run() {
        let dir = tempfile::tempdir().unwrap();
        let a = JsonlAdapter::new(Frag);
        let p = dir.path().join("f.jsonl");
        let mut s = String::new();
        for turn in ["a", "b"] {
            for i in 0..40 {
                s += &format!("{{\"f\":\"{turn}{i} \"}}\n");
                if i % 10 == 9 {
                    s += "{\"tick\":1}\n";
                }
            }
            s += "{\"t\":\"end\"}\n";
        }
        for i in 0..3 {
            s += &format!("{{\"f\":\"c{i} \"}}\n");
        }
        std::fs::write(&p, &s).unwrap();
        let out = a.read(&p, None).unwrap();
        let full: Vec<Event> = events(&out).into_iter().cloned().collect();
        assert_eq!(full.len(), 13);
        // Page backwards with a limit far below the fragment count; every page boundary is an
        // event position, as clients and the full-text indexer use it.
        let mut paged: Vec<Event> = Vec::new();
        let mut before = None;
        for _ in 0..10 {
            let page = a.history(&p, "f", &HistoryQuery { before, limit: 2 }).unwrap();
            let Some(first) = page.first() else { break };
            assert!(before.is_none_or(|b| first.pos.unwrap() < b), "pages move backwards");
            before = first.pos;
            paged.splice(0..0, page);
        }
        let view = |evs: &[Event]| -> std::collections::BTreeMap<String, (bool, String)> {
            evs.iter()
                .map(|e| {
                    let text = match &e.body {
                        Body::AssistantMessage { text, .. } => text.clone(),
                        _ => String::new(),
                    };
                    (e.id.clone(), (e.partial, text))
                })
                .collect()
        };
        assert_eq!(paged.len(), full.len(), "no event twice");
        assert_eq!(view(&paged), view(&full), "same ids, texts and partial flags as one whole read");
        assert!(paged.windows(2).all(|w| w[0].pos <= w[1].pos));
        let (partial, a) = &view(&full)["m0"];
        assert!(!partial && a.starts_with("a0 a1 ") && a.ends_with("a39 "));
    }

    #[test]
    fn decode_record_feeds_values_outside_files() {
        let toy = Toy { root: PathBuf::new() };
        let mut st = Count::default();
        let v: Value = serde_json::from_str(&line("u", "hi", 5)).unwrap();
        let d = decode_record(&toy, "toy:x", Path::new("/db"), 42, &v, &mut st);
        assert!(matches!(&d.records[..], [Record::Event(e)] if e.pos == Some(42) && e.session == "toy:x" && e.ts == 5));
        let bad: Value = serde_json::json!({"t":"zzz"});
        let d2 = decode_record(&toy, "toy:x", Path::new("/db"), 43, &bad, &mut st);
        assert!(d2.records.is_empty());
        assert_eq!(d2.unknown, vec!["t=Some(\"zzz\")".to_string()]);
        assert_eq!(st.0, 2);
    }
}
