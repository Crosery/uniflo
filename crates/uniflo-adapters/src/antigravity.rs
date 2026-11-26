//! Google Antigravity (IDE + CLI) step transcripts:
//! `~/.gemini/antigravity{,-cli}/brain/<conversation>/.system_generated/logs/transcript.jsonl`.
//!
//! Each line is a step `{step_index, type, source, status, created_at, content…}`.
//! `PLANNER_RESPONSE` steps carry the model's text/thinking/tool calls (without call ids);
//! the following tool steps (`GENERIC`, `RUN_COMMAND`, `VIEW_FILE`, …) are their results,
//! matched in order. A step may be re-written as its status moves `RUNNING` → `DONE`.
//! Sibling files under `~/.gemini/antigravity{,-cli}/` supply title (`annotations/<id>.pbtxt`,
//! `conversation_summaries.db`), workspace cwd (`conversations/<id>.db`,
//! `conversation_summaries.db`, `cache/last_conversations.json`), and per-step protobuf
//! `ModelUsageStats` / model metadata (`conversations/<id>.db` `steps.metadata`).

use crate::common::{FileSig, Sidecar, WithSidecars, file_sig, output_with_reasoning, under_any};
use crate::sqlite::{col, columns, open_ro, wal_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use uniflo_core::procs::{Proc, ProcCache};
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, text_of, ts, ts_str};
use uniflo_core::{Cx, HarnessInfo, LineDecoder, LiveSession, MetaPatch, Record, SourceId};
use uniflo_schema::{Body, Event, Usage, session_key};

pub struct Antigravity {
    pub roots: Vec<PathBuf>,
    procs: Option<ProcCache>,
    summaries: RwLock<HashMap<PathBuf, (FileSig, Arc<SummaryIndex>)>>,
    /// Conversation database → cached workspace and per-step usage/model metadata, keyed by
    /// `file_sig(&[cdb, wal_of(&cdb)])` so live polling and line decoding open each DB at most
    /// once per signature change.
    conv_dbs: RwLock<HashMap<PathBuf, (FileSig, Arc<ConvDb>)>>,
}

#[derive(Default)]
struct SummaryIndex {
    /// Conversation id → `(rank, title)`: rank 2 for `title`, rank 1 for `agent_name`.
    titles: HashMap<String, (u8, String)>,
    cwds: HashMap<String, String>,
    parents: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct ConvDb {
    cwd: Option<String>,
    steps: HashMap<i64, DbStep>,
}

#[derive(Debug, Clone, Default)]
struct DbStep {
    model: Option<String>,
    usage: Option<DbUsage>,
}

#[derive(Debug, Clone, Default)]
struct DbUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
}

const PROC_TTL: Duration = Duration::from_secs(3);

impl Antigravity {
    pub fn new(roots: Vec<PathBuf>, procs: Option<ProcCache>) -> Self {
        Antigravity { roots, procs, summaries: RwLock::new(HashMap::new()), conv_dbs: RwLock::new(HashMap::new()) }
    }

    fn other_bases<'a>(&'a self, base: &'a Path) -> impl Iterator<Item = &'a Path> {
        self.roots.iter().filter_map(|r| r.parent()).filter(move |b| *b != base)
    }

    fn summary_index(&self, base: &Path) -> Arc<SummaryIndex> {
        let sdb = base.join("conversation_summaries.db");
        let lc = base.join("cache/last_conversations.json");
        let sig = file_sig(&[sdb.clone(), wal_of(&sdb), lc.clone()]);
        if let Some((prev, idx)) = self.summaries.read().unwrap().get(base)
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
            && let Ok(cols) = columns(&conn, "conversation_summaries")
            && cols.contains("conversation_id")
        {
            let sql = format!(
                "SELECT conversation_id, {}, {}, {}, {} FROM conversation_summaries",
                col(&cols, "title"),
                col(&cols, "workspace_uris"),
                col(&cols, "parent_conversation_id"),
                col(&cols, "agent_name"),
            );
            if let Ok(mut st) = conn.prepare(&sql)
                && let Ok(rows) = st.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0).unwrap_or_default(),
                        r.get::<_, String>(1).unwrap_or_default(),
                        r.get::<_, String>(2).unwrap_or_default(),
                        r.get::<_, String>(3).unwrap_or_default(),
                        r.get::<_, String>(4).unwrap_or_default(),
                    ))
                })
            {
                for (id, title, uris, parent, agent_name) in rows.flatten() {
                    if id.is_empty() {
                        continue;
                    }
                    let title = title.trim();
                    let agent_name = agent_name.trim();
                    if !title.is_empty() {
                        idx.titles.insert(id.clone(), (2, title.to_owned()));
                    } else if !agent_name.is_empty() && idx.titles.get(&id).is_none_or(|(r, _)| *r <= 1) {
                        idx.titles.insert(id.clone(), (1, agent_name.to_owned()));
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
        }
        let arc = Arc::new(idx);
        self.summaries.write().unwrap().insert(base.to_path_buf(), (sig, arc.clone()));
        arc
    }

    fn summary_title(&self, base: &Path, id: &str) -> Option<(u8, String)> {
        let mut best = self.summary_index(base).titles.get(id).cloned();
        if best.as_ref().is_some_and(|(r, _)| *r >= 2) {
            return best;
        }
        for other in self.other_bases(base) {
            if let Some((r, t)) = self.summary_index(other).titles.get(id)
                && best.as_ref().is_none_or(|(br, _)| *r > *br)
            {
                best = Some((*r, t.clone()));
                if *r >= 2 {
                    break;
                }
            }
        }
        best
    }

    fn cwd_of(&self, base: &Path, id: &str) -> Option<String> {
        let cdb = base.join("conversations").join(format!("{id}.db"));
        if let Some(cwd) = self.conv_db(&cdb).cwd.clone() {
            return Some(cwd);
        }
        if let Some(cwd) = self.summary_index(base).cwds.get(id).cloned() {
            return Some(cwd);
        }
        self.other_bases(base).find_map(|b| self.summary_index(b).cwds.get(id).cloned())
    }

    fn parent_of(&self, base: &Path, id: &str) -> Option<String> {
        self.summary_index(base)
            .parents
            .get(id)
            .cloned()
            .or_else(|| self.other_bases(base).find_map(|b| self.summary_index(b).parents.get(id).cloned()))
    }

    fn conv_db(&self, cdb: &Path) -> Arc<ConvDb> {
        let sig = file_sig(&[cdb.to_path_buf(), wal_of(cdb)]);
        if let Some((prev, db)) = self.conv_dbs.read().unwrap().get(cdb)
            && *prev == sig
        {
            return db.clone();
        }
        let db = Arc::new(load_conv_db(cdb));
        self.conv_dbs.write().unwrap().insert(cdb.to_path_buf(), (sig, db.clone()));
        db
    }

    fn conv_db_for_src(&self, src: &Path) -> Option<Arc<ConvDb>> {
        let (base, id) = base_and_id(src)?;
        let cdb = base.join("conversations").join(format!("{id}.db"));
        Some(self.conv_db(&cdb))
    }

    fn resolve_step(&self, src: &Path, step: i64, v: &Value, need_db: bool) -> (Option<String>, Option<DbStep>) {
        let inline_model = str_of(v, "model").map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let db_step = if step >= 0 && (need_db || inline_model.is_none()) {
            self.conv_db_for_src(src).and_then(|db| db.steps.get(&step).cloned())
        } else {
            None
        };
        let model = inline_model.or_else(|| db_step.as_ref().and_then(|s| s.model.clone()));
        (model, db_step)
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
    "acp",
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

const VALUE_FLAGS: &[&str] = &[
    "--add-dir",
    "--agent",
    "--conversation",
    "--effort",
    "-i",
    "--input-format",
    "--json-schema",
    "--log-file",
    "--mode",
    "--model",
    "--output-format",
    "-p",
    "--print",
    "--print-timeout",
    "--project",
    "--prompt",
    "--prompt-interactive",
    "--settings",
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
    while let Some(tok) = it.next() {
        if tok == "--" {
            break;
        }
        if VALUE_FLAGS.contains(&tok) {
            it.next();
            continue;
        }
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
    let Some(dir_name) = p.parent().and_then(|d| d.file_name()).and_then(|d| d.to_str()) else {
        return false;
    };
    match dir_name {
        "conversations" => {
            name.ends_with(".db") || name.ends_with(".db-wal") || name.ends_with(".db-shm") || name.ends_with(".lock")
        }
        "presence" => name.ends_with(".lock"),
        _ => false,
    }
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
    let dir_name = dir.file_name()?.to_str()?;
    let id = match dir_name {
        "conversations" => name
            .strip_suffix(".db-wal")
            .or_else(|| name.strip_suffix(".db-shm"))
            .or_else(|| name.strip_suffix(".db"))
            .or_else(|| name.strip_suffix(".lock"))?,
        "presence" => name.strip_suffix(".lock")?,
        _ => return None,
    };
    let base = dir.parent()?;
    let brain = base.join("brain");
    if !roots.iter().any(|r| r == &brain) {
        return None;
    }
    brain.join(id).join(".system_generated/logs/transcript.jsonl").is_file().then(|| id.to_owned())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct PendingUsage {
    step: i64,
    ts: i64,
    model: Option<String>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
}

impl PendingUsage {
    fn from_json(step: i64, ts: i64, model: Option<String>, v: &Value) -> Self {
        PendingUsage {
            step,
            ts,
            model,
            input: u64_of(v, "input_tokens"),
            output: u64_of(v, "output_tokens"),
            cache_read: u64_of(v, "cache_read_tokens"),
            cache_write: u64_of(v, "cache_write_tokens"),
            reasoning: u64_of(v, "thinking_output_tokens").or_else(|| u64_of(v, "thinking_tokens")),
        }
    }

    fn has_any_tokens(&self) -> bool {
        self.input.is_some()
            || self.output.is_some()
            || self.cache_read.is_some()
            || self.cache_write.is_some()
            || self.reasoning.is_some()
    }
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
    /// Completed planner/checkpoint steps whose `conversations/<id>.db` lacked `model_usage`
    /// when the transcript line was decoded (live WAL flush race).
    pending_usage: VecDeque<PendingUsage>,
}

const SYSTEM_STEPS: &[&str] = &["SYSTEM_MESSAGE", "CONVERSATION_HISTORY", "DIRECTORY_RULES", "EPHEMERAL_MESSAGE"];

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
        let mut out = self.roots.clone();
        for r in &self.roots {
            if let Some(base) = r.parent() {
                out.push(base.join("presence"));
            }
        }
        out
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
                let (model, db_step) = self.resolve_step(cx.src, step, v, !running);
                if let Some(m) = &model {
                    cx.meta().model = Some(m.clone());
                }
                if let Some(th) = v.get("thinking").map(text_of).filter(|s| !s.is_empty()) {
                    cx.emit(format!("{sid}:r"), t, Body::Reasoning { text: th }).partial = running;
                }
                if !content.is_empty() {
                    cx.emit(format!("{sid}:a"), t, Body::AssistantMessage { text: content, model: model.clone() })
                        .partial = running;
                }
                let calls = v.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                let canceled = matches!(status, "CANCELED" | "CANCELLED");
                let fresh = step > cx.state.planner_max;
                for (i, c) in calls.iter().enumerate() {
                    let call_id = format!("{sid}:{i}");
                    let name = str_of(c, "name").unwrap_or("").to_owned();
                    if fresh && !canceled {
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
                if canceled {
                    let prefix = format!("{sid}:");
                    cx.state.pending.retain(|(id, _)| !id.starts_with(&prefix));
                }
                if !running {
                    emit_step_usage(v, step, &sid, t, model, db_step.as_ref(), cx);
                }
                if canceled {
                    cx.emit(format!("{sid}:end"), t, Body::TurnEnd { reason: Some("canceled".into()) });
                } else if calls.is_empty() && status == "DONE" {
                    cx.emit(format!("{sid}:end"), t, Body::TurnEnd { reason: Some("done".into()) });
                }
            }
            "CHECKPOINT" => {
                let (model, db_step) = self.resolve_step(cx.src, step, v, !running);
                if let Some(m) = &model {
                    cx.meta().model = Some(m.clone());
                }
                cx.emit(sid.clone(), t, Body::System { subtype: "checkpoint".into(), text: content });
                if !running {
                    emit_step_usage(v, step, &sid, t, model, db_step.as_ref(), cx);
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
                let is_swarm_wait = name.as_deref() == Some("swarm_wait");
                let body = Body::ToolResult { call_id, name, output, is_error: failed };
                cx.emit(sid.clone(), res_ts, body).partial = running;
                if is_swarm_wait && status == "DONE" {
                    cx.emit(format!("{sid}:end"), res_ts, Body::TurnEnd { reason: Some("swarm_wait".into()) });
                }
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
        let mut out = vec![base.join("annotations").join(format!("{id}.pbtxt")), cdb.clone(), wal_of(&cdb)];
        for other in self.other_bases(base) {
            out.push(other.join("annotations").join(format!("{id}.pbtxt")));
        }
        out
    }

    fn sidecar_sig(&self, src: &Path) -> FileSig {
        let Some((base, id)) = base_and_id(src) else { return Vec::new() };
        let mut sig = file_sig(&self.sidecars(src));
        // Hash only this session's slice of the shared `conversation_summaries.db` and
        // `last_conversations.json` so updating one conversation does not re-read all ~700 others.
        let summary_cwd = self
            .summary_index(base)
            .cwds
            .get(id)
            .cloned()
            .or_else(|| self.other_bases(base).find_map(|b| self.summary_index(b).cwds.get(id).cloned()));
        let h = fnv_summary(
            self.summary_title(base, id).as_ref(),
            summary_cwd.as_deref(),
            self.parent_of(base, id).as_deref(),
        );
        sig.push((0, h));
        sig
    }

    fn sidecar(&self, src: &Path, state: &mut Links) -> Vec<Record> {
        let Some((base, id)) = base_and_id(src) else { return Vec::new() };
        let pbtxt = base.join("annotations").join(format!("{id}.pbtxt"));
        let title = pbtxt_title(&pbtxt)
            .or_else(|| {
                self.other_bases(base).find_map(|b| pbtxt_title(&b.join("annotations").join(format!("{id}.pbtxt"))))
            })
            .map(|t| (3, t))
            .or_else(|| self.summary_title(base, id));
        let cwd = self.cwd_of(base, id);
        let parent = self.parent_of(base, id);

        let mut model_patch = None;
        let mut events = Vec::new();
        if !state.pending_usage.is_empty() {
            let cdb = base.join("conversations").join(format!("{id}.db"));
            let db = self.conv_db(&cdb);
            let session = session_key("antigravity", id);
            state.pending_usage.retain(|pu| {
                let Some(db_step) = db.steps.get(&pu.step) else {
                    return true;
                };
                let Some(db_u) = db_step.usage.as_ref() else {
                    return true;
                };
                let model = pu.model.clone().or_else(|| db_step.model.clone());
                if model.is_some() {
                    model_patch.clone_from(&model);
                }
                let usage = build_usage(pu, Some(db_u), model);
                events.push(Record::Event(Event {
                    id: format!("s{}:u", pu.step),
                    session: session.clone(),
                    ts: pu.ts,
                    pos: None,
                    partial: false,
                    truncated: false,
                    body: Body::Usage(usage),
                }));
                false
            });
        }

        let patch = MetaPatch { parent, title, cwd, model: model_patch, ..Default::default() };
        let mut out = Vec::new();
        if !patch.is_empty() {
            out.push(Record::Meta(patch));
        }
        out.extend(events);
        out
    }
}

/// Deterministic 64-bit FNV-1a hash of one session's summary-DB metadata so `sidecar_sig`
/// stays stable across restarts and ignores changes to unrelated sessions in the shared DB.
fn fnv_summary(title: Option<&(u8, String)>, cwd: Option<&str>, parent: Option<&str>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    if let Some((rank, t)) = title {
        mix(&[*rank]);
        mix(t.as_bytes());
    } else {
        mix(&[]);
    }
    mix(cwd.unwrap_or("").as_bytes());
    mix(parent.unwrap_or("").as_bytes());
    h
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn build_usage(inl: &PendingUsage, db: Option<&DbUsage>, model: Option<String>) -> Usage {
    let input = inl.input.or_else(|| db.map(|u| u.input)).unwrap_or(0);
    let cache_read = inl.cache_read.or_else(|| db.map(|u| u.cache_read)).unwrap_or(0);
    let cache_write = inl.cache_write.or_else(|| db.map(|u| u.cache_write)).unwrap_or(0);
    let reasoning = inl.reasoning.or_else(|| db.map(|u| u.reasoning)).unwrap_or(0);
    let raw_out = inl.output.or_else(|| db.map(|u| u.output)).unwrap_or(0);
    let output = output_with_reasoning(raw_out, reasoning);
    Usage { input, output, cache_read, cache_write, reasoning, model, cost_usd: None }
}

fn emit_step_usage(
    v: &Value,
    step: i64,
    sid: &str,
    t: i64,
    model: Option<String>,
    db_step: Option<&DbStep>,
    cx: &mut Cx<'_, Links>,
) {
    let inl = PendingUsage::from_json(step, t, model.clone(), v);
    let db_u = db_step.and_then(|s| s.usage.as_ref());
    if inl.has_any_tokens() || db_u.is_some() {
        let usage = build_usage(&inl, db_u, model);
        cx.emit(format!("{sid}:u"), t, Body::Usage(usage));
    }
    if step >= 0 {
        if db_u.is_some() {
            cx.state.pending_usage.retain(|p| p.step != step);
        } else if let Some(slot) = cx.state.pending_usage.iter_mut().find(|p| p.step == step) {
            *slot = inl;
        } else {
            cx.state.pending_usage.push_back(inl);
            if cx.state.pending_usage.len() > 64 {
                cx.state.pending_usage.pop_front();
            }
        }
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

fn load_conv_db(path: &Path) -> ConvDb {
    let mut out = ConvDb::default();
    if !path.is_file() {
        return out;
    }
    let Ok(conn) = open_ro(path) else {
        return out;
    };
    if let Ok(data) =
        conn.query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get::<_, Vec<u8>>(0))
    {
        out.cwd = extract_file_uri(&data);
    }
    if let Ok(mut st) = conn.prepare("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL")
        && let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
    {
        for (idx, blob) in rows.flatten() {
            if let Some(step) = parse_step_metadata(&blob) {
                out.steps.insert(idx, step);
            }
        }
    }
    out
}

enum ProtoVal<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut val = 0u64;
    for i in 0..10 {
        let b = *buf.get(*pos)?;
        *pos += 1;
        if i == 9 && b > 1 {
            return None;
        }
        val |= ((b & 0x7f) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Some(val);
        }
    }
    None
}

fn read_field<'a>(buf: &'a [u8], pos: &mut usize) -> Option<(u64, ProtoVal<'a>)> {
    let tag = read_varint(buf, pos)?;
    let field = tag >> 3;
    if field == 0 {
        return None;
    }
    match tag & 7 {
        0 => Some((field, ProtoVal::Varint(read_varint(buf, pos)?))),
        1 => {
            *pos = pos.checked_add(8)?;
            (*pos <= buf.len()).then_some((field, ProtoVal::Fixed))
        }
        2 => {
            let len = usize::try_from(read_varint(buf, pos)?).ok()?;
            let end = pos.checked_add(len)?;
            let slice = buf.get(*pos..end)?;
            *pos = end;
            Some((field, ProtoVal::Bytes(slice)))
        }
        5 => {
            *pos = pos.checked_add(4)?;
            (*pos <= buf.len()).then_some((field, ProtoVal::Fixed))
        }
        _ => None,
    }
}

fn parse_step_metadata(buf: &[u8]) -> Option<DbStep> {
    let mut pos = 0;
    let mut usage_model = 0u64;
    let mut gen_model = 0u64;
    let mut info_slug: Option<String> = None;
    let mut has_usage_tokens = false;
    let (mut inp, mut out, mut cw, mut cr, mut think, mut resp_out) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);

    while pos < buf.len() {
        let Some((field, val)) = read_field(buf, &mut pos) else { break };
        match (field, val) {
            (9, ProtoVal::Bytes(sub)) => {
                let mut spos = 0;
                while spos < sub.len() {
                    let Some((sf, sv)) = read_field(sub, &mut spos) else { break };
                    match (sf, sv) {
                        (1, ProtoVal::Varint(v)) => usage_model = v,
                        (2, ProtoVal::Varint(v)) => {
                            inp = v;
                            has_usage_tokens = true;
                        }
                        (3, ProtoVal::Varint(v)) => {
                            out = v;
                            has_usage_tokens = true;
                        }
                        (4, ProtoVal::Varint(v)) => {
                            cw = v;
                            has_usage_tokens = true;
                        }
                        (5, ProtoVal::Varint(v)) => {
                            cr = v;
                            has_usage_tokens = true;
                        }
                        (9, ProtoVal::Varint(v)) => {
                            think = v;
                            has_usage_tokens = true;
                        }
                        (10, ProtoVal::Varint(v)) => {
                            resp_out = v;
                            has_usage_tokens = true;
                        }
                        _ => {}
                    }
                }
            }
            (11, ProtoVal::Varint(v)) => gen_model = v,
            (24, ProtoVal::Bytes(sub)) => {
                let mut spos = 0;
                while spos < sub.len() {
                    let Some((sf, sv)) = read_field(sub, &mut spos) else { break };
                    if sf == 8
                        && let ProtoVal::Bytes(s) = sv
                        && let Ok(uri) = std::str::from_utf8(s)
                        && let Some(slug) = model_uri_slug(uri)
                    {
                        info_slug = Some(slug);
                    }
                }
            }
            _ => {}
        }
    }

    let model = resolve_db_model(usage_model, gen_model, info_slug.as_deref());
    let usage = has_usage_tokens.then(|| {
        let raw_out = if out > 0 { out } else { resp_out.saturating_add(think) };
        DbUsage {
            input: inp,
            output: output_with_reasoning(raw_out, think),
            cache_read: cr,
            cache_write: cw,
            reasoning: think,
        }
    });
    (model.is_some() || usage.is_some()).then_some(DbStep { model, usage })
}

fn model_uri_slug(uri: &str) -> Option<String> {
    let before_q = uri.split_once('?').map_or(uri, |(b, _)| b).trim_end_matches('/');
    let path = match before_q.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => before_q,
    };
    let slug = path.rsplit('/').next()?.trim();
    (!slug.is_empty()).then(|| slug.to_owned())
}

fn model_enum_slug(code: u64) -> Option<&'static str> {
    Some(match code {
        1021 => "gemini-3.1-flash-image",
        1026 => "claude-opus-4-6-thinking",
        1036 => "gemini-3.1-pro-low",
        1037 => "gemini-3.1-pro-high",
        1050 => "gemini-3.1-flash-lite-preview",
        1071 => "gemini-3.6-flash-high",
        1072 => "gemini-3.6-flash-medium",
        1073 => "gemini-3.6-flash-low",
        1084 | 1132 | 1149 => "gemini-3.5-flash",
        1298 => "gemini-3.7-flash-high",
        1299 => "gemini-3.7-flash-medium",
        1300 => "gemini-3.7-flash-low",
        1318 => "gemini-3.8-flash-high",
        1319 => "gemini-3.8-flash-medium",
        1320 => "gemini-3.8-flash-low",
        1417 => "gemini-4-argon-high",
        1418 => "gemini-4-argon-medium",
        1419 => "gemini-4-argon-low",
        1427 => "gemini-4-carbon-high",
        1437 => "gemini-4-carbon-medium",
        1438 => "gemini-4-carbon-low",
        1461 | 1525 => "gemini-4-hydrogen-high",
        1462 => "gemini-4-hydrogen-medium",
        1463 => "gemini-4-hydrogen-low",
        1464 | 1528 => "gemini-4-hydrogen-xhigh",
        _ => return None,
    })
}

fn resolve_db_model(usage_model: u64, gen_model: u64, info_slug: Option<&str>) -> Option<String> {
    for code in [usage_model, gen_model] {
        if code == 326 {
            if let Some(s) = info_slug {
                return Some(s.to_owned());
            }
        } else if let Some(s) = model_enum_slug(code) {
            return Some(s.to_owned());
        }
    }
    info_slug.map(str::to_owned)
}

fn find_file_uri_proto(buf: &[u8], depth: u8) -> Option<&str> {
    if depth > 6 {
        return None;
    }
    let mut pos = 0;
    let mut found = None;
    while pos < buf.len() {
        let (_, val) = read_field(buf, &mut pos)?;
        if found.is_none()
            && let ProtoVal::Bytes(s) = val
        {
            if s.starts_with(b"file://") {
                found = std::str::from_utf8(s).ok();
            } else {
                found = find_file_uri_proto(s, depth + 1);
            }
        }
    }
    found
}

fn extract_file_uri(data: &[u8]) -> Option<String> {
    if let Some(uri) = find_file_uri_proto(data, 0)
        && let Some(p) = uri_to_path(uri)
    {
        return Some(p);
    }
    let needle = b"file://";
    let idx = data.windows(needle.len()).position(|w| w == needle)?;
    if idx > 0 && data[idx - 1] & 0x80 == 0 {
        let mut start = idx - 1;
        while start > 0 && idx - start < 4 && data[start - 1] & 0x80 != 0 {
            start -= 1;
        }
        let mut vpos = start;
        if let Some(len) = read_varint(&data[..idx], &mut vpos)
            && vpos == idx
            && let Ok(len) = usize::try_from(len)
            && len > needle.len()
            && let Some(slice) = data.get(idx..idx.checked_add(len)?)
            && let Ok(uri) = std::str::from_utf8(slice)
            && !uri.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            return uri_to_path(uri);
        }
    }
    let mut end = idx;
    while end < data.len() && data[end] >= 0x20 && data[end] != 0x7f {
        end += 1;
    }
    let uri = std::str::from_utf8(&data[idx..end]).ok()?;
    uri_to_path(uri)
}

fn parse_workspace_uris(raw: &str) -> Option<String> {
    let uris: Vec<String> = serde_json::from_str(raw).ok()?;
    uris.into_iter().find_map(|u| uri_to_path(&u))
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(hi), Some(lo)) =
                (b.get(i + 1).copied().and_then(hex_nibble), b.get(i + 2).copied().and_then(hex_nibble))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?.trim();
    if rest.is_empty() {
        return None;
    }
    let decoded = percent_decode(rest);
    if decoded.is_empty() {
        return None;
    }
    // Windows `file:///C:/foo` → `C:/foo`.
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(decoded[1..].to_owned());
    }
    Some(decoded)
}

/// Strip Antigravity's `<USER_REQUEST>` wrapper (including after leading blocks such as
/// `<GRILL_ME>…</GRILL_ME>`) and trailing `<ADDITIONAL_METADATA>` / `<USER_SETTINGS_CHANGE>`
/// blocks from `USER_INPUT` content.
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
    if let Some((before, rest)) = main.split_once("<USER_REQUEST>")
        && (before.is_empty() || (before.trim_start().starts_with('<') && before.trim_end().ends_with('>')))
    {
        let inner = rest
            .strip_suffix("</USER_REQUEST>")
            .or_else(|| rest.rsplit_once("</USER_REQUEST>").map(|(b, _)| b))
            .unwrap_or(rest);
        return inner.trim().to_owned();
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
    use crate::common::testkit::{Fixture, group, kinds};
    use serde_json::json;
    use uniflo_core::{Adapter, HistoryQuery};
    use uniflo_schema::Status;

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    fn adapter(fx: &Fixture) -> WithSidecars<Antigravity> {
        WithSidecars::new(Antigravity::new(vec![fx.root().join("brain")], None))
    }

    fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    fn pb_varint(field: u64, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        push_varint(&mut out, field << 3);
        push_varint(&mut out, v);
        out
    }

    fn pb_bytes(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        push_varint(&mut out, (field << 3) | 2);
        push_varint(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        out
    }

    fn encode_model_usage(
        model: u64,
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
        thinking: u64,
        response_out: u64,
    ) -> Vec<u8> {
        let mut m = Vec::new();
        for (f, v) in
            [(1, model), (2, input), (3, output), (4, cache_write), (5, cache_read), (9, thinking), (10, response_out)]
        {
            if v != 0 {
                m.extend_from_slice(&pb_varint(f, v));
            }
        }
        m
    }

    fn encode_step_metadata(usage: Option<&[u8]>, gen_model: u64, model_uri: Option<&str>) -> Vec<u8> {
        let mut s = Vec::new();
        if let Some(u) = usage {
            s.extend_from_slice(&pb_bytes(9, u));
        }
        if gen_model != 0 {
            s.extend_from_slice(&pb_varint(11, gen_model));
        }
        if let Some(uri) = model_uri {
            let info = pb_bytes(8, uri.as_bytes());
            s.extend_from_slice(&pb_bytes(24, &info));
        }
        s
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
    fn conversation_workspace_is_read_once_per_database_change() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let cdb = fx.root().join("conversations/c1.db");
        std::fs::create_dir_all(cdb.parent().unwrap()).unwrap();
        let put = |blob: Vec<u8>| {
            let c = rusqlite::Connection::open(&cdb).unwrap();
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS trajectory_metadata_blob (data BLOB); DELETE FROM trajectory_metadata_blob;",
            )
            .unwrap();
            c.execute("INSERT INTO trajectory_metadata_blob (data) VALUES (?1)", [blob]).unwrap();
        };
        let raw_len = |uri: &str| {
            let mut b = vec![uri.len() as u8];
            b.extend_from_slice(uri.as_bytes());
            b
        };
        put(raw_len("file:///work/a"));
        let dec = &a.inner.decoder;
        assert_eq!(dec.cwd_of(fx.root(), "c1").as_deref(), Some("/work/a"));
        assert!(dec.conv_dbs.read().unwrap().contains_key(&cdb));
        // Same signature: the memo answers, even if the stored value were different.
        dec.conv_dbs.write().unwrap().get_mut(&cdb).unwrap().1 =
            Arc::new(ConvDb { cwd: Some("/memo".into()), steps: HashMap::new() });
        assert_eq!(dec.cwd_of(fx.root(), "c1").as_deref(), Some("/memo"));

        // Nested protobuf >128B URI with percent-encoded spaces and CJK characters, followed by another field.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let deep_dir = format!("/Users/dev/{}/项目%20目录/sub%20space", "segment_".repeat(20));
        let uri = format!("file://{deep_dir}");
        assert!(uri.len() > 160);
        let mut inner = pb_bytes(1, uri.as_bytes());
        inner.extend_from_slice(&pb_bytes(2, b"trailing-ascii-field"));
        let outer = pb_bytes(3, &inner);
        put(outer);
        let expected = format!("/Users/dev/{}/项目 目录/sub space", "segment_".repeat(20));
        assert_eq!(dec.cwd_of(fx.root(), "c1").as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn inline_usage_combines_with_db_enrichment_and_live_wal_race() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let cdb = fx.root().join("conversations/u1.db");
        std::fs::create_dir_all(cdb.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&cdb).unwrap();
        conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);").unwrap();
        // Step 1 already in DB at initial read: provides cache_write_tokens + thinking_output_tokens.
        let s1_meta = encode_step_metadata(Some(&encode_model_usage(1026, 1200, 300, 450, 8000, 180, 120)), 1026, None);
        conn.execute("INSERT INTO steps (idx, metadata) VALUES (1, ?1)", [s1_meta]).unwrap();

        let st = |i: i64, ty: &str, status: &str, extra: Value| {
            let mut o =
                json!({"step_index":i,"source":"MODEL","type":ty,"status":status,"created_at":"2026-08-24T14:18:26Z"});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            l(o)
        };
        let mut s = st(0, "USER_INPUT", "DONE", json!({"source":"USER_EXPLICIT","content":"run"}));
        // RUNNING step must not emit usage yet.
        s += &st(1, "PLANNER_RESPONSE", "RUNNING", json!({"thinking":"plan","model":"claude-opus-4-6-thinking"}));
        s += &st(
            1,
            "PLANNER_RESPONSE",
            "DONE",
            json!({
                "thinking":"plan",
                "content":"Checked",
                "model":"claude-opus-4-6-thinking",
                "input_tokens":1200,
                "cache_read_tokens":8000,
                "output_tokens":300
            }),
        );
        // CHECKPOINT step with inline tokens + DB enrichment.
        let s2_meta = encode_step_metadata(Some(&encode_model_usage(1084, 400, 90, 50, 1000, 25, 65)), 1084, None);
        conn.execute("INSERT INTO steps (idx, metadata) VALUES (2, ?1)", [s2_meta]).unwrap();
        s += &st(
            2,
            "CHECKPOINT",
            "DONE",
            json!({
                "content":"summary",
                "model":"gemini-3.5-flash",
                "input_tokens":400,
                "cache_read_tokens":1000,
                "output_tokens":90
            }),
        );

        let p = fx.write("brain/u1/.system_generated/logs/transcript.jsonl", &s);
        let out1 = a.read(&p, None).unwrap();
        let g1 = group(out1.batch);
        let r1 = &g1["u1"];
        assert_eq!(r1.status(), Status::Idle);
        assert_eq!(r1.meta.model.as_deref(), Some("gemini-3.5-flash"));

        let h = a.history(&p, "u1", &HistoryQuery { before: None, limit: 100 }).unwrap();
        // Order: user_message, reasoning, assistant_message, usage, turn_end, system(checkpoint), usage
        assert_eq!(
            kinds(&h),
            vec!["user_message", "reasoning", "assistant_message", "usage", "turn_end", "system", "usage"]
        );
        assert!(matches!(&h[2].body, Body::AssistantMessage { text, model: Some(m) }
            if text == "Checked" && m == "claude-opus-4-6-thinking"));
        assert!(matches!(&h[3].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read, u.cache_write, u.reasoning) == (1200, 300, 8000, 450, 180)
                && u.model.as_deref() == Some("claude-opus-4-6-thinking")
                && u.cost_usd.is_none()));
        assert!(matches!(&h[6].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read, u.cache_write, u.reasoning) == (400, 90, 1000, 50, 25)
                && u.model.as_deref() == Some("gemini-3.5-flash")));

        // Verify SessionTrajectory groups reasoning + assistant_message into step 1 and checkpoint system into step 2.
        let pricing = uniflo_core::pricing::Pricing::new(None).current();
        let mut builder = uniflo_core::trajectory::Builder::new("antigravity:u1", &pricing);
        let mut all_recs = Vec::new();
        a.read_all(&p, &["u1".into()], &mut |_, rec| all_recs.push(rec)).unwrap();
        for rec in &all_recs {
            builder.push(rec);
        }
        let traj = builder.finish();
        assert_eq!(traj.turns.len(), 1);
        assert_eq!(traj.turns[0].steps.len(), 2);
        assert_eq!(traj.turns[0].steps[0].items.len(), 2, "reasoning + assistant text grouped into step 1");
        assert_eq!(traj.turns[0].steps[1].items.len(), 1, "checkpoint system grouped into step 2");

        // Now simulate live race: transcript appends step 3 DONE before DB writes step 3 metadata.
        let step3 = st(
            3,
            "PLANNER_RESPONSE",
            "DONE",
            json!({
                "content":"Live reply",
                "model":"claude-opus-4-6-thinking",
                "input_tokens":500,
                "cache_read_tokens":2000,
                "output_tokens":120
            }),
        );
        fx.append(&p, &step3);
        let out2 = a.read(&p, Some(&out1.cursor)).unwrap();
        let evs2 = &group(out2.batch)["u1"].events;
        assert_eq!(kinds(evs2), vec!["assistant_message", "usage", "turn_end"]);
        assert!(matches!(&evs2[1].body, Body::Usage(u)
            if evs2[1].id == "s3:u" && (u.input, u.output, u.cache_read, u.cache_write, u.reasoning) == (500, 120, 2000, 0, 0)));

        // DB flushes step 3 metadata afterwards; sidecar read emits updated s3:u once and clears pending.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let s3_meta = encode_step_metadata(Some(&encode_model_usage(1026, 500, 120, 310, 2000, 70, 50)), 1026, None);
        conn.execute("INSERT INTO steps (idx, metadata) VALUES (3, ?1)", [s3_meta]).unwrap();
        assert!(a.changed(&p, &out2.cursor));
        let out3 = a.read(&p, Some(&out2.cursor)).unwrap();
        let evs3 = &group(out3.batch)["u1"].events;
        assert_eq!(evs3.len(), 1);
        assert_eq!(evs3[0].id, "s3:u");
        assert_eq!(evs3[0].pos, None);
        assert!(matches!(&evs3[0].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read, u.cache_write, u.reasoning) == (500, 120, 2000, 310, 70)
                && u.model.as_deref() == Some("claude-opus-4-6-thinking")));

        // Subsequent unchanged follow emits nothing.
        let out4 = a.read(&p, Some(&out3.cursor)).unwrap();
        assert!(out4.batch.items.is_empty());
    }

    #[test]
    fn older_transcript_falls_back_to_db_usage_and_model_enums() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let cdb = fx.root().join("conversations/old-1.db");
        std::fs::create_dir_all(cdb.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&cdb).unwrap();
        conn.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);").unwrap();

        // Step 1: enum 1461 -> gemini-4-hydrogen-high
        let m1 = encode_step_metadata(Some(&encode_model_usage(1461, 800, 200, 0, 1500, 90, 110)), 1461, None);
        // Step 2: enum 326 -> falls back to model_info field 8 URI slug before '?'
        let m2 = encode_step_metadata(
            Some(&encode_model_usage(326, 300, 60, 20, 900, 15, 45)),
            326,
            Some("gemini-api://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro-exp?alt=sse"),
        );
        conn.execute("INSERT INTO steps (idx, metadata) VALUES (1, ?1)", [m1]).unwrap();
        conn.execute("INSERT INTO steps (idx, metadata) VALUES (2, ?1)", [m2]).unwrap();

        let st = |i: i64, ty: &str, extra: Value| {
            let mut o =
                json!({"step_index":i,"source":"MODEL","type":ty,"status":"DONE","created_at":"2026-08-24T14:18:26Z"});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            l(o)
        };
        let mut s = st(0, "USER_INPUT", json!({"source":"USER_EXPLICIT","content":"hello"}));
        s += &st(1, "PLANNER_RESPONSE", json!({"content":"first"}));
        s += &st(2, "PLANNER_RESPONSE", json!({"content":"second"}));

        let p = fx.write("brain/old-1/.system_generated/logs/transcript.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.meta.model.as_deref(), Some("gemini-2.5-pro-exp"));
        let h = a.history(&p, "old-1", &HistoryQuery { before: None, limit: 100 }).unwrap();
        assert!(matches!(&h[1].body, Body::AssistantMessage { model: Some(m), .. } if m == "gemini-4-hydrogen-high"));
        assert!(matches!(&h[2].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read, u.cache_write, u.reasoning) == (800, 200, 1500, 0, 90)
                && u.model.as_deref() == Some("gemini-4-hydrogen-high")));
        assert!(matches!(&h[4].body, Body::AssistantMessage { model: Some(m), .. } if m == "gemini-2.5-pro-exp"));
        assert!(matches!(&h[5].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read, u.cache_write, u.reasoning) == (300, 60, 900, 20, 15)
                && u.model.as_deref() == Some("gemini-2.5-pro-exp")));
    }

    #[test]
    fn swarm_wait_and_canceled_planner_end_turn_to_idle() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let st = |i: i64, ty: &str, status: &str, extra: Value| {
            let mut o =
                json!({"step_index":i,"source":"MODEL","type":ty,"status":status,"created_at":"2026-08-24T14:18:26Z"});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            l(o)
        };
        let mut s = st(0, "USER_INPUT", "DONE", json!({"source":"USER_EXPLICIT","content":"work"}));
        s += &st(
            1,
            "PLANNER_RESPONSE",
            "DONE",
            json!({"tool_calls":[{"name":"post_message","args":{"text":"done"}},{"name":"swarm_wait","args":{"since":1}}]}),
        );
        s += &st(2, "GENERIC", "DONE", json!({"content":"posted"}));
        s += &st(3, "GENERIC", "RUNNING", json!({"content":""}));
        let p = fx.write("brain/sw-1/.system_generated/logs/transcript.jsonl", &s);
        let out1 = a.read(&p, None).unwrap();
        assert_eq!(group(out1.batch)["sw-1"].status(), Status::Work, "RUNNING swarm_wait is still working");

        // swarm_wait finishes DONE → emits turn_end and transitions worker to Idle.
        fx.append(
            &p,
            &st(
                3,
                "GENERIC",
                "DONE",
                json!({"content":"Created At: 2026-08-24T14:18:27Z\nCompleted At: 2026-08-24T14:19:00Z\n\nwoke"}),
            ),
        );
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Idle);
        let last = r.events.last().unwrap();
        assert_eq!(last.ts, 1787581140000);
        assert!(matches!(&last.body, Body::TurnEnd { reason: Some(r) } if r == "swarm_wait"));

        // CANCELED planner step also ends the turn cleanly without poisoning next turn's tool links.
        let mut c = st(0, "USER_INPUT", "DONE", json!({"source":"USER_EXPLICIT","content":"stop me"}));
        c += &st(1, "PLANNER_RESPONSE", "RUNNING", json!({"tool_calls":[{"name":"stale_tool","args":{}}]}));
        c += &st(
            1,
            "PLANNER_RESPONSE",
            "CANCELED",
            json!({"content":"aborted","tool_calls":[{"name":"stale_tool","args":{}}]}),
        );
        let pc = fx.write("brain/sw-cancel/.system_generated/logs/transcript.jsonl", &c);
        let rc = fx.index(&a, &pc);
        assert_eq!(rc.status(), Status::Idle);
        assert!(matches!(&rc.events.last().unwrap().body, Body::TurnEnd { reason: Some(r) } if r == "canceled"));
    }

    #[test]
    fn clean_user_input_handles_leading_blocks_and_metadata() {
        assert_eq!(
            clean_user_input(
                "<GRILL_ME>\nask one question at a time\n</GRILL_ME>\n<USER_REQUEST>\nalign on schema\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nt\n</ADDITIONAL_METADATA>"
            ),
            "align on schema"
        );
        assert_eq!(
            clean_user_input("<PLAN>\nsteps\n</PLAN><USER_REQUEST>ship it</USER_REQUEST><USER_SETTINGS_CHANGE>x"),
            "ship it"
        );
        assert_eq!(
            clean_user_input("plain text mentioning <USER_REQUEST> inline"),
            "plain text mentioning <USER_REQUEST> inline"
        );
    }

    #[test]
    fn agent_name_fallback_title_ranks_and_cross_root_summaries() {
        let fx = Fixture::new();
        let cli_root = fx.root().join("antigravity-cli/brain");
        let ide_root = fx.root().join("antigravity/brain");
        let a = WithSidecars::new(Antigravity::new(vec![ide_root.clone(), cli_root.clone()], None));

        let cli_sdb = fx.root().join("antigravity-cli/conversation_summaries.db");
        let ide_sdb = fx.root().join("antigravity/conversation_summaries.db");
        std::fs::create_dir_all(cli_sdb.parent().unwrap()).unwrap();
        std::fs::create_dir_all(ide_sdb.parent().unwrap()).unwrap();

        let init_db = |p: &Path| {
            let c = rusqlite::Connection::open(p).unwrap();
            c.execute_batch(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT,
                    title TEXT,
                    workspace_uris TEXT,
                    parent_conversation_id TEXT,
                    agent_name TEXT
                );",
            )
            .unwrap();
            c
        };
        let c_cli = init_db(&cli_sdb);
        c_cli
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5)",
                ["sub-1", "", r#"["file:///Users/me/my%20repo"]"#, "parent-1", "verifier"],
            )
            .unwrap();
        c_cli
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5)",
                ["sub-2", "Summary Title", "", "", "worker"],
            )
            .unwrap();

        let c_ide = init_db(&ide_sdb);
        c_ide
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5)",
                [
                    "mirrored-cli",
                    "IDE Mirrored Title",
                    r#"["file:///Users/me/%E9%A1%B9%E7%9B%AE"]"#,
                    "root-conv",
                    "helper",
                ],
            )
            .unwrap();

        let line = l(json!({
            "step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE",
            "created_at":"2026-08-24T14:18:26Z","content":"go"
        }));
        let p1 = fx.write("antigravity-cli/brain/sub-1/.system_generated/logs/transcript.jsonl", &line);
        let r1 = fx.index(&a, &p1);
        assert_eq!((r1.meta.title_rank, r1.meta.title.as_deref()), (1, Some("verifier")));
        assert_eq!(r1.meta.cwd.as_deref(), Some("/Users/me/my repo"));
        assert_eq!(r1.meta.parent.as_deref(), Some("parent-1"));

        let p2 = fx.write("antigravity-cli/brain/sub-2/.system_generated/logs/transcript.jsonl", &line);
        let r2 = fx.index(&a, &p2);
        assert_eq!((r2.meta.title_rank, r2.meta.title.as_deref()), (2, Some("Summary Title")));

        // pbtxt rank 3 overrides summary title rank 2.
        fx.write("antigravity-cli/annotations/sub-2.pbtxt", "title: \"User Renamed\"\n");
        let r2_pb = fx.index(&a, &p2);
        assert_eq!((r2_pb.meta.title_rank, r2_pb.meta.title.as_deref()), (3, Some("User Renamed")));

        // CLI session whose summary was mirrored only into IDE's conversation_summaries.db.
        let pm = fx.write("antigravity-cli/brain/mirrored-cli/.system_generated/logs/transcript.jsonl", &line);
        let rm = fx.index(&a, &pm);
        assert_eq!((rm.meta.title_rank, rm.meta.title.as_deref()), (2, Some("IDE Mirrored Title")));
        assert_eq!(rm.meta.cwd.as_deref(), Some("/Users/me/项目"));
        assert_eq!(rm.meta.parent.as_deref(), Some("root-conv"));
    }

    #[test]
    fn process_matcher_and_session_assignment() {
        assert!(is_antigravity("/opt/homebrew/bin/agy"));
        assert!(is_antigravity("/Users/x/.local/lib/agy-update-guard/known-good/agy --conversation=c1"));
        assert!(is_antigravity("agy -p update"));
        assert!(is_antigravity("agy --prompt mcp"));
        assert!(is_antigravity("agy --conversation c1 --effort high"));
        assert!(!is_antigravity("/opt/homebrew/bin/agy --bg-updater --app_data_dir=antigravity-cli"));
        assert!(!is_antigravity("agy mcp list"));
        assert!(!is_antigravity("agy --model x mcp list"));
        assert!(!is_antigravity("agy --log-file /tmp/a.log acp"));
        assert!(!is_antigravity("agy acp"));
        assert!(!is_antigravity("agy update"));

        let fx = Fixture::new();
        let a = adapter(&fx);
        let s = l(
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-08-24T14:18:26Z","content":"hi"}),
        );
        fx.write("brain/c-arg/.system_generated/logs/transcript.jsonl", &s);
        fx.write("brain/c-db/.system_generated/logs/transcript.jsonl", &s);
        fx.write("brain/c-shm/.system_generated/logs/transcript.jsonl", &s);
        fx.write("brain/c-pres/.system_generated/logs/transcript.jsonl", &s);
        fx.write("brain/c-cwd/.system_generated/logs/transcript.jsonl", &s);
        let db = fx.write("conversations/c-db.db", "");
        let shm = fx.write("conversations/c-shm.db-shm", "");
        let pres = fx.write("presence/c-pres.lock", "");
        fx.write("cache/last_conversations.json", r#"{"/work/p":"c-cwd"}"#);

        assert!(is_antigravity_file(&shm));
        assert!(is_antigravity_file(&pres));
        assert!(
            !a.inner
                .decoder
                .sidecars(&fx.root().join("brain/c-shm/.system_generated/logs/transcript.jsonl"))
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-shm"))),
            "-shm must stay out of Sidecar::sidecars"
        );

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
            Proc {
                pid: 13,
                start_ms: now - 5_000,
                args: "agy".into(),
                argv: vec!["agy".into()],
                cwd: None,
                files: vec![shm],
            },
            Proc {
                pid: 14,
                start_ms: now - 5_000,
                args: "agy".into(),
                argv: vec!["agy".into()],
                cwd: None,
                files: vec![pres],
            },
        ];
        let mut got: Vec<(u32, String)> = a.inner.decoder.assign(procs).into_iter().map(|l| (l.pid, l.id)).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (10, "c-arg".into()),
                (11, "c-db".into()),
                (12, "c-cwd".into()),
                (13, "c-shm".into()),
                (14, "c-pres".into()),
            ]
        );
    }
}
