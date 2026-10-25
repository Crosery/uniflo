//! Google Antigravity (IDE + CLI) step transcripts:
//! `~/.gemini/antigravity{,-cli}/brain/<conversation>/.system_generated/logs/transcript.jsonl`.
//!
//! Each line is a step `{step_index, type, source, status, created_at, content…}`.
//! `PLANNER_RESPONSE` steps carry the model's text/thinking/tool calls (without call ids);
//! the following tool steps (`GENERIC`, `RUN_COMMAND`, `VIEW_FILE`, …) are their results,
//! matched in order. A step may be re-written as its status moves `RUNNING` → `DONE`.
//! Sibling files under `~/.gemini/antigravity{,-cli}/` supply title (`annotations/<id>.pbtxt`,
//! `conversation_summaries.db`) and workspace cwd (`conversations/<id>.db`,
//! `conversation_summaries.db`, `cache/last_conversations.json`).

use crate::common::{FileSig, Sidecar, WithSidecars, file_sig, under_any};
use crate::sqlite::{open_ro, wal_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uniflo_core::procs::{Proc, ProcCache};
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, text_of, ts, ts_str};
use uniflo_core::{Cx, HarnessInfo, LineDecoder, LiveSession, MetaPatch, Record, SourceId};
use uniflo_schema::Body;

pub struct Antigravity {
    pub roots: Vec<PathBuf>,
    procs: Option<ProcCache>,
    summaries: Mutex<HashMap<PathBuf, (FileSig, Arc<SummaryIndex>)>>,
}

#[derive(Default)]
struct SummaryIndex {
    titles: HashMap<String, String>,
    cwds: HashMap<String, String>,
    parents: HashMap<String, String>,
}

const PROC_TTL: Duration = Duration::from_secs(3);

impl Antigravity {
    pub fn new(roots: Vec<PathBuf>, procs: Option<ProcCache>) -> Self {
        Antigravity { roots, procs, summaries: Mutex::new(HashMap::new()) }
    }

    fn summary_index(&self, base: &Path) -> Arc<SummaryIndex> {
        let sdb = base.join("conversation_summaries.db");
        let lc = base.join("cache/last_conversations.json");
        let sig = file_sig(&[sdb.clone(), wal_of(&sdb), lc.clone()]);
        let mut g = self.summaries.lock().unwrap();
        if let Some((prev, idx)) = g.get(base)
            && *prev == sig
        {
            return idx.clone();
        }
        let mut idx = SummaryIndex::default();
        if let Ok(text) = std::fs::read_to_string(&lc)
            && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text)
        {
            for (cwd, id) in map {
                if let Some(id) = id.as_str().filter(|s| !s.is_empty())
                    && !cwd.is_empty()
                {
                    idx.cwds.entry(id.to_owned()).or_insert(cwd);
                }
            }
        }
        if sdb.is_file()
            && let Ok(conn) = open_ro(&sdb)
            && let Ok(mut st) = conn.prepare(
                "SELECT conversation_id, title, workspace_uris, parent_conversation_id FROM conversation_summaries",
            )
            && let Ok(rows) = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0).unwrap_or_default(),
                    r.get::<_, String>(1).unwrap_or_default(),
                    r.get::<_, String>(2).unwrap_or_default(),
                    r.get::<_, String>(3).unwrap_or_default(),
                ))
            })
        {
            for (id, title, uris, parent) in rows.flatten() {
                if id.is_empty() {
                    continue;
                }
                let title = title.trim();
                if !title.is_empty() {
                    idx.titles.insert(id.clone(), title.to_owned());
                }
                if let Some(cwd) = parse_workspace_uris(&uris) {
                    idx.cwds.insert(id.clone(), cwd);
                }
                let parent = parent.trim();
                if !parent.is_empty() {
                    idx.parents.insert(id, parent.to_owned());
                }
            }
        }
        let arc = Arc::new(idx);
        g.insert(base.to_path_buf(), (sig, arc.clone()));
        arc
    }

    fn cwd_of(&self, base: &Path, id: &str) -> Option<String> {
        let cdb = base.join("conversations").join(format!("{id}.db"));
        if let Some(cwd) = db_cwd(&cdb) {
            return Some(cwd);
        }
        self.summary_index(base).cwds.get(id).cloned()
    }

    fn has_session(&self, id: &str) -> bool {
        self.roots.iter().any(|r| r.join(id).join(".system_generated/logs/transcript.jsonl").is_file())
    }

    fn assign(&self, mut list: Vec<Proc>) -> Vec<LiveSession> {
        let mut out = Vec::new();
        let mut claimed = HashSet::new();
        let mut claim = |out: &mut Vec<LiveSession>, id: String, pid: u32| {
            if claimed.insert(id.clone()) {
                out.push(LiveSession { id, pid, status: None });
            }
        };
        let mut unresolved = Vec::new();
        for p in list.drain(..) {
            let mut exact: Vec<String> = Vec::new();
            if let Some(id) = p.arg_after("--conversation").filter(|id| self.has_session(id)) {
                exact.push(id.to_owned());
            }
            for f in &p.files {
                if let Some(sid) = self.is_source(f).then(|| self.identify(f)).flatten() {
                    exact.push(sid.id);
                } else if let Some(id) = id_from_open_file(f, &self.roots) {
                    exact.push(id);
                }
            }
            if exact.is_empty() {
                unresolved.push(p);
                continue;
            }
            for id in exact {
                claim(&mut out, id, p.pid);
            }
        }
        unresolved.sort_by_key(|p| std::cmp::Reverse(p.start_ms));
        for p in unresolved {
            let Some(cwd) = p.cwd.as_ref().and_then(|c| c.to_str()) else { continue };
            let mut candidates: Vec<(String, i64)> = Vec::new();
            for root in &self.roots {
                let Some(base) = root.parent() else { continue };
                let Ok(rd) = std::fs::read_dir(root) else { continue };
                for entry in rd.flatten() {
                    let Some(id) = entry.file_name().to_str().map(str::to_owned) else { continue };
                    let t = entry.path().join(".system_generated/logs/transcript.jsonl");
                    let Ok(md) = std::fs::metadata(&t) else { continue };
                    let mtime = file_mtime_ms(&md);
                    if mtime >= p.start_ms - 5_000 && self.cwd_of(base, &id).as_deref() == Some(cwd) {
                        candidates.push((id, mtime));
                    }
                }
            }
            candidates.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
            if let Some((id, _)) = candidates.into_iter().find(|(id, _)| !out.iter().any(|l| l.id == *id)) {
                claim(&mut out, id, p.pid);
            }
        }
        out
    }
}

pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(antigravity())]
}

pub fn antigravity() -> WithSidecars<Antigravity> {
    let h = home();
    WithSidecars::new(Antigravity::new(
        vec![h.join(".gemini/antigravity/brain"), h.join(".gemini/antigravity-cli/brain")],
        Some(ProcCache::new(PROC_TTL, is_antigravity).with_files(is_antigravity_file)),
    ))
}

const SUBCOMMANDS: &[&str] = &[
    "agent",
    "agents",
    "changelog",
    "help",
    "install",
    "mcp",
    "mic-serve",
    "models",
    "plugin",
    "plugins",
    "remote-control",
    "update",
];

fn is_antigravity(args: &str) -> bool {
    if args.contains("--bg-updater") {
        return false;
    }
    let mut it = args.split_whitespace();
    let Some(first) = it.next() else { return false };
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first).to_ascii_lowercase();
    let stem = base.strip_suffix(".exe").unwrap_or(&base);
    if stem != "agy" && stem != "antigravity" {
        return false;
    }
    for tok in it {
        if tok.starts_with('-') {
            continue;
        }
        return !SUBCOMMANDS.contains(&tok);
    }
    true
}

fn is_antigravity_file(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else { return false };
    if name == "transcript.jsonl" || name == "transcript_full.jsonl" {
        return true;
    }
    (name.ends_with(".db") || name.ends_with(".db-wal") || name.ends_with(".lock"))
        && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == "conversations")
}

fn id_from_open_file(p: &Path, roots: &[PathBuf]) -> Option<String> {
    let name = p.file_name()?.to_str()?;
    if name == "transcript_full.jsonl" {
        let t = p.with_file_name("transcript.jsonl");
        if under_any(&t, roots) {
            return p.parent()?.parent()?.parent()?.file_name()?.to_str().map(str::to_owned);
        }
        return None;
    }
    let dir = p.parent()?;
    if dir.file_name()? != "conversations" {
        return None;
    }
    let base = dir.parent()?;
    let brain = base.join("brain");
    if !roots.iter().any(|r| r == &brain) {
        return None;
    }
    let id =
        name.strip_suffix(".db-wal").or_else(|| name.strip_suffix(".db")).or_else(|| name.strip_suffix(".lock"))?;
    brain.join(id).join(".system_generated/logs/transcript.jsonl").is_file().then(|| id.to_owned())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Links {
    /// Tool calls waiting for their result step: (call_id, name).
    pending: VecDeque<(String, String)>,
    /// Highest planner step whose calls were queued (re-written steps must not re-queue).
    planner_max: i64,
    /// Recently assigned result steps → (step, call_id, name), so re-written results keep their link.
    assigned: VecDeque<(i64, String, String)>,
    /// Whether a fallback cwd was already emitted from tool call arguments.
    cwd_emitted: bool,
}

const SYSTEM_STEPS: &[&str] =
    &["SYSTEM_MESSAGE", "CHECKPOINT", "CONVERSATION_HISTORY", "DIRECTORY_RULES", "EPHEMERAL_MESSAGE"];

impl LineDecoder for Antigravity {
    type State = Links;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "antigravity", name: "Antigravity" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        p.file_name().is_some_and(|n| n == "transcript.jsonl")
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == "logs")
            && under_any(p, &self.roots)
    }

    /// Only the transcript: the rest of brain/<id>/ holds the user's artifacts and uploads.
    fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> {
        uniflo_core::cleanup::targets::file(src)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        // brain/<id>/.system_generated/logs/transcript.jsonl
        let id = p.parent()?.parent()?.parent()?.file_name()?.to_str()?.to_owned();
        Some(SourceId { id, parent: None })
    }

    fn max_depth(&self) -> usize {
        4
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        self.procs.as_ref().map(|p| self.assign(p.get()))
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, Links>) {
        let step = v.get("step_index").and_then(Value::as_i64).unwrap_or(-1);
        let t = v.get("created_at").and_then(ts).unwrap_or(0);
        let ty = str_of(v, "type").unwrap_or("");
        let status = str_of(v, "status").unwrap_or("");
        let running = status == "RUNNING";
        let content = text_of(v.get("content").unwrap_or(&Value::Null));
        let sid = format!("s{step}");
        match ty {
            "USER_INPUT" => {
                let text = clean_user_input(&content);
                cx.emit(sid, t, Body::UserMessage { text, synthetic: false });
            }
            "PLANNER_RESPONSE" => {
                if let Some(th) = v.get("thinking").map(text_of).filter(|s| !s.is_empty()) {
                    cx.emit(format!("{sid}:r"), t, Body::Reasoning { text: th }).partial = running;
                }
                if !content.is_empty() {
                    cx.emit(format!("{sid}:a"), t, Body::AssistantMessage { text: content, model: None }).partial =
                        running;
                }
                let calls = v.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                let fresh = step > cx.state.planner_max;
                for (i, c) in calls.iter().enumerate() {
                    let call_id = format!("{sid}:{i}");
                    let name = str_of(c, "name").unwrap_or("").to_owned();
                    if fresh {
                        cx.state.pending.push_back((call_id.clone(), name.clone()));
                    }
                    let input = json_arg(c.get("args").unwrap_or(&Value::Null));
                    if !cx.state.cwd_emitted
                        && let Some(dir) = ["Cwd", "DirectoryPath", "SearchPath"]
                            .into_iter()
                            .find_map(|k| string_of(&input, k))
                            .filter(|d| !d.contains("/.gemini/") && !d.contains("\\.gemini\\"))
                    {
                        cx.meta().cwd = Some(dir);
                        cx.state.cwd_emitted = true;
                    }
                    cx.emit(format!("{sid}:c{i}"), t, Body::ToolCall { call_id, name, input }).partial = running;
                }
                if fresh {
                    cx.state.planner_max = step;
                }
                if calls.is_empty() && status == "DONE" {
                    cx.emit(format!("{sid}:end"), t, Body::TurnEnd { reason: Some("done".into()) });
                }
            }
            "ERROR_MESSAGE" => {
                let text = v.get("error").map(text_of).filter(|s| !s.is_empty()).unwrap_or(content);
                cx.emit(sid, t, Body::System { subtype: "error".into(), text });
            }
            ty if SYSTEM_STEPS.contains(&ty) => {
                cx.emit(sid, t, Body::System { subtype: ty.to_ascii_lowercase(), text: content });
            }
            "" => cx.unknown("type="),
            // Tool execution result (`GENERIC`, `RUN_COMMAND`, `VIEW_FILE`, …).
            ty => {
                let (call_id, linked_name) = link(cx.state, step);
                let (res_ts, output) = parse_tool_output(&content, t);
                let failed = status == "ERROR" || v.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0);
                let name = linked_name.or_else(|| (ty != "GENERIC").then(|| ty.to_ascii_lowercase()));
                let body = Body::ToolResult { call_id, name, output, is_error: failed };
                cx.emit(sid, res_ts, body).partial = running;
            }
        }
    }
}

impl Sidecar for Antigravity {
    fn transcript_of(&self, path: &Path) -> Option<PathBuf> {
        let name = path.file_name()?.to_str()?;
        let dir = path.parent()?;
        let dir_name = dir.file_name()?.to_str()?;
        let base = dir.parent()?;
        let id = match dir_name {
            "annotations" => name.strip_suffix(".pbtxt")?,
            "conversations" => name.strip_suffix(".db-wal").or_else(|| name.strip_suffix(".db"))?,
            _ => return None,
        };
        let t = base.join("brain").join(id).join(".system_generated/logs/transcript.jsonl");
        (self.is_source(&t) && t.is_file()).then_some(t)
    }

    fn sidecars(&self, src: &Path) -> Vec<PathBuf> {
        let Some((base, id)) = base_and_id(src) else { return Vec::new() };
        let cdb = base.join("conversations").join(format!("{id}.db"));
        let sdb = base.join("conversation_summaries.db");
        vec![
            base.join("annotations").join(format!("{id}.pbtxt")),
            cdb.clone(),
            wal_of(&cdb),
            sdb.clone(),
            wal_of(&sdb),
            base.join("cache/last_conversations.json"),
        ]
    }

    fn sidecar(&self, src: &Path, _state: &mut Links) -> Vec<Record> {
        let Some((base, id)) = base_and_id(src) else { return Vec::new() };
        let idx = self.summary_index(base);
        let pbtxt = base.join("annotations").join(format!("{id}.pbtxt"));
        let title = pbtxt_title(&pbtxt).map(|t| (3, t)).or_else(|| idx.titles.get(id).cloned().map(|t| (2, t)));
        let cwd = self.cwd_of(base, id);
        let parent = idx.parents.get(id).cloned();
        let patch = MetaPatch { parent, title, cwd, ..Default::default() };
        if patch.is_empty() { Vec::new() } else { vec![Record::Meta(patch)] }
    }
}

fn base_and_id(src: &Path) -> Option<(&Path, &str)> {
    let brain_dir = src.parent()?.parent()?.parent()?;
    let id = brain_dir.file_name()?.to_str()?;
    let base = brain_dir.parent()?.parent()?;
    Some((base, id))
}

fn pbtxt_title(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("title:") else { continue };
        let val = rest.trim();
        let parsed = serde_json::from_str::<String>(val)
            .ok()
            .or_else(|| val.strip_prefix('"').and_then(|s| s.strip_suffix('"')).map(str::to_owned))?;
        let trimmed = parsed.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

fn db_cwd(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let conn = open_ro(path).ok()?;
    let data: Vec<u8> = conn.query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get(0)).ok()?;
    extract_file_uri(&data)
}

fn extract_file_uri(data: &[u8]) -> Option<String> {
    let needle = b"file://";
    let idx = data.windows(needle.len()).position(|w| w == needle)?;
    let mut end = idx;
    while end < data.len() && (0x20..0x7f).contains(&data[end]) {
        end += 1;
    }
    if idx > 0 {
        let len = data[idx - 1] as usize;
        if len > needle.len() && idx + len <= end {
            end = idx + len;
        }
    }
    let uri = std::str::from_utf8(&data[idx..end]).ok()?;
    uri_to_path(uri)
}

fn parse_workspace_uris(raw: &str) -> Option<String> {
    let uris: Vec<String> = serde_json::from_str(raw).ok()?;
    uris.into_iter().find_map(|u| uri_to_path(&u))
}

fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?.trim();
    if rest.is_empty() {
        return None;
    }
    // Windows `file:///C:/foo` → `C:/foo`.
    let bytes = rest.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(rest[1..].to_owned());
    }
    Some(rest.to_owned())
}

/// Strip Antigravity's `<USER_REQUEST>` wrapper and trailing `<ADDITIONAL_METADATA>` /
/// `<USER_SETTINGS_CHANGE>` blocks from `USER_INPUT` content.
fn clean_user_input(raw: &str) -> String {
    let mut main = raw;
    for tag in
        ["\n<ADDITIONAL_METADATA>", "<ADDITIONAL_METADATA>", "\n<USER_SETTINGS_CHANGE>", "<USER_SETTINGS_CHANGE>"]
    {
        if let Some((before, _)) = main.split_once(tag) {
            main = before;
        }
    }
    let main = main.trim();
    if let Some(rest) = main.strip_prefix("<USER_REQUEST>") {
        return rest.strip_suffix("</USER_REQUEST>").unwrap_or(rest).trim().to_owned();
    }
    main.to_owned()
}

/// Parse leading `Created At:` / `Completed At:` lines from tool result output.
fn parse_tool_output(raw: &str, fallback_ts: i64) -> (i64, String) {
    let Some(after_created) = raw.strip_prefix("Created At:") else {
        return (fallback_ts, raw.to_owned());
    };
    let rest = after_created.split_once('\n').map_or("", |(_, r)| r);
    let (completed_ts, rest) = if let Some(after_completed) = rest.strip_prefix("Completed At:") {
        let (line, rem) = after_completed.split_once('\n').unwrap_or((after_completed, ""));
        (ts_str(line.trim()).unwrap_or(fallback_ts), rem)
    } else {
        (fallback_ts, rest)
    };
    let body = rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n')).unwrap_or(rest);
    (completed_ts, body.to_owned())
}

fn link(st: &mut Links, step: i64) -> (String, Option<String>) {
    if let Some((_, id, name)) = st.assigned.iter().find(|(s, _, _)| *s == step) {
        return (id.clone(), (!name.is_empty()).then(|| name.clone()));
    }
    let (id, name) = st.pending.pop_front().unwrap_or_else(|| (format!("s{step}"), String::new()));
    st.assigned.push_back((step, id.clone(), name.clone()));
    if st.assigned.len() > 64 {
        st.assigned.pop_front();
    }
    (id, (!name.is_empty()).then_some(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_core::{Adapter, HistoryQuery};
    use uniflo_schema::Status;

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    fn adapter(fx: &Fixture) -> WithSidecars<Antigravity> {
        WithSidecars::new(Antigravity::new(vec![fx.root().join("brain")], None))
    }

    #[test]
    fn steps_link_calls_to_results_in_order() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let st = |i: i64, ty: &str, status: &str, extra: Value| {
            let mut o =
                json!({"step_index":i,"source":"MODEL","type":ty,"status":status,"created_at":"2026-08-24T14:18:26Z"});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            l(o)
        };
        let mut s = st(
            0,
            "USER_INPUT",
            "DONE",
            json!({
                "source":"USER_EXPLICIT",
                "content":"<USER_REQUEST>\ncheck disk\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>"
            }),
        );
        s += &st(
            1,
            "PLANNER_RESPONSE",
            "DONE",
            json!({"thinking":"use df","tool_calls":[{"name":"run_command","args":{"CommandLine":"df -h","Cwd":"/work/app"}},{"name":"view_file","args":{"path":"/x"}}]}),
        );
        s += &st(2, "RUN_COMMAND", "RUNNING", json!({"content":""}));
        s += &st(2, "RUN_COMMAND", "DONE", json!({"content":"ok","exit_code":0}));
        s += &st(
            3,
            "GENERIC",
            "ERROR",
            json!({"content":"Created At: 2026-08-24T14:18:26Z\nCompleted At: 2026-08-24T14:18:30Z\n\nmissing"}),
        );
        s += &st(4, "PLANNER_RESPONSE", "DONE", json!({"content":"Disk is fine"}));
        fx.write("annotations/conv-1.pbtxt", "last_user_view_time {\n}\ntitle: \"Check disk usage\"\n");
        let p = fx.write("brain/conv-1/.system_generated/logs/transcript.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.id, "conv-1");
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Check disk usage"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/work/app"));
        assert_eq!(r.preview.as_deref(), Some("check disk"));
        let h = a.history(&p, "conv-1", &HistoryQuery { before: None, limit: 100 }).unwrap();
        assert_eq!(
            kinds(&h),
            vec![
                "user_message",
                "reasoning",
                "tool_call",
                "tool_call",
                "tool_result",
                "tool_result",
                "assistant_message",
                "turn_end"
            ]
        );
        assert!(matches!(&h[0].body, Body::UserMessage { text, synthetic: false } if text == "check disk"));
        assert!(
            matches!(&h[4].body, Body::ToolResult { call_id, name: Some(n), is_error: false, output } if call_id == "s1:0" && n == "run_command" && output == "ok")
        );
        assert!(!h[4].partial, "DONE rewrite replaced RUNNING");
        assert!(
            matches!(&h[5].body, Body::ToolResult { call_id, name: Some(n), is_error: true, output } if call_id == "s1:1" && n == "view_file" && output == "missing")
        );
        assert_eq!(h[5].ts, 1787581110000, "Completed At timestamp used");
        assert!(
            !a.inner
                .decoder
                .is_source(&fx.root().join("brain/conv-1/.system_generated/logs/chunks/transcript/00000000.jsonl"))
        );
    }

    #[test]
    fn generic_tool_result_keeps_turn_in_tool_result_state() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let st = |i: i64, ty: &str, extra: Value| {
            let mut o =
                json!({"step_index":i,"source":"MODEL","type":ty,"status":"DONE","created_at":"2026-08-24T14:18:26Z"});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            l(o)
        };
        let mut s =
            st(0, "USER_INPUT", json!({"source":"USER_EXPLICIT","content":"<USER_REQUEST>build</USER_REQUEST>"}));
        s += &st(1, "PLANNER_RESPONSE", json!({"tool_calls":[{"name":"list_dir","args":{"DirectoryPath":"/proj"}}]}));
        s += &st(
            2,
            "GENERIC",
            json!({"content":"Created At: 2026-08-24T14:18:27Z\nCompleted At: 2026-08-24T14:18:28Z\n\nfiles"}),
        );
        let p = fx.write("brain/conv-2/.system_generated/logs/transcript.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
        let mut tracker = uniflo_core::status::StatusTracker::default();
        for e in &r.events {
            tracker.observe(e);
        }
        assert_eq!(tracker.reason, "tool_result", "must not trigger 90s assistant_message settle window");
    }

    #[test]
    fn process_matcher_and_session_assignment() {
        assert!(is_antigravity("/opt/homebrew/bin/agy"));
        assert!(is_antigravity("/Users/x/.local/lib/agy-update-guard/known-good/agy --conversation=c1"));
        assert!(!is_antigravity("/opt/homebrew/bin/agy --bg-updater --app_data_dir=antigravity-cli"));
        assert!(!is_antigravity("agy mcp list"));
        assert!(!is_antigravity("agy update"));

        let fx = Fixture::new();
        let a = adapter(&fx);
        let s = l(
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-08-24T14:18:26Z","content":"hi"}),
        );
        fx.write("brain/c-arg/.system_generated/logs/transcript.jsonl", &s);
        fx.write("brain/c-db/.system_generated/logs/transcript.jsonl", &s);
        fx.write("brain/c-cwd/.system_generated/logs/transcript.jsonl", &s);
        let db = fx.write("conversations/c-db.db", "");
        fx.write("cache/last_conversations.json", r#"{"/work/p":"c-cwd"}"#);

        let now = uniflo_core::util::now_ms();
        let procs = vec![
            Proc {
                pid: 10,
                start_ms: now - 5_000,
                args: "agy --conversation=c-arg".into(),
                argv: vec!["agy".into(), "--conversation=c-arg".into()],
                cwd: None,
                files: vec![],
            },
            Proc {
                pid: 11,
                start_ms: now - 5_000,
                args: "agy".into(),
                argv: vec!["agy".into()],
                cwd: None,
                files: vec![db],
            },
            Proc {
                pid: 12,
                start_ms: now - 5_000,
                args: "agy".into(),
                argv: vec!["agy".into()],
                cwd: Some(PathBuf::from("/work/p")),
                files: vec![],
            },
        ];
        let mut got: Vec<(u32, String)> = a.inner.decoder.assign(procs).into_iter().map(|l| (l.pid, l.id)).collect();
        got.sort();
        assert_eq!(got, vec![(10, "c-arg".into()), (11, "c-db".into()), (12, "c-cwd".into())]);
    }
}
