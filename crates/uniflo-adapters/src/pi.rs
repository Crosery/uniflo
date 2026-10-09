//! Pi coding-agent transcript family: Pi, oh-my-pi (omp), Crosery agents, Command Code.
//!
//! Layout: `<root>/<cwd-slug>/<ts>_<id>.jsonl`; sub-agents and forks live under a
//! directory named after the parent file (`<ts>_<id>/<Name>.jsonl`, `<ts>_<id>/forks/…`).
//! Messages use `toolCall` blocks + `toolResult` role (Pi) or Anthropic
//! `tool_use`/`tool_result` blocks (Command Code); both are accepted.

use crate::common::{after_ts_prefix, ends_turn_reason, output_with_reasoning, reported_cost, under_any};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;
use uniflo_core::procs::{Proc, ProcCache};
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, text_of, ts};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, LiveSession, SourceId};
use uniflo_schema::{Body, Usage};

pub struct PiFamily {
    pub info: HarnessInfo,
    pub roots: Vec<PathBuf>,
    /// Running CLI processes of this harness (none of them keeps an on-disk registry).
    procs: Option<ProcCache>,
    /// Session directory → cwd recorded in its newest transcript header.
    dir_cwd: RwLock<HashMap<PathBuf, Option<String>>>,
}

impl PiFamily {
    pub fn new(info: HarnessInfo, roots: Vec<PathBuf>, procs: Option<ProcCache>) -> Self {
        PiFamily { info, roots, procs, dir_cwd: RwLock::new(HashMap::new()) }
    }
}

/// Pi, oh-my-pi, Crosery agents and Command Code.
pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![
        std::sync::Arc::new(pi()),
        std::sync::Arc::new(omp()),
        std::sync::Arc::new(crosery()),
        std::sync::Arc::new(commandcode()),
    ]
}

fn family(id: &'static str, name: &'static str, rel: &[&str], procs: Option<ProcCache>) -> JsonlAdapter<PiFamily> {
    let h = home();
    JsonlAdapter::new(PiFamily::new(HarnessInfo { id, name }, rel.iter().map(|r| h.join(r)).collect(), procs))
}

const PROC_TTL: Duration = Duration::from_secs(3);

fn is_jsonl(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "jsonl")
}

/// The entrypoint is argv[0], or argv[1] when argv[0] is a JS runtime (`bun …/bin/omp`).
fn entry(args: &str, pred: impl Fn(&str) -> bool) -> bool {
    let mut it = args.split_whitespace();
    let Some(first) = it.next() else { return false };
    let base = first.rsplit('/').next().unwrap_or(first);
    if matches!(base, "bun" | "node" | "deno" | "tsx") { it.next().is_some_and(pred) } else { pred(first) }
}

fn is_omp(args: &str) -> bool {
    entry(args, |a| a == "omp" || a.ends_with("/omp")) && !args.contains("__omp_worker")
}

fn is_pi(args: &str) -> bool {
    entry(args, |a| a == "pi" || a.ends_with("/pi") || a.ends_with("pi-coding-agent/dist/cli.js"))
        && !args.contains("@oh-my-pi/")
        && !args.contains("__omp_")
}

pub fn pi() -> JsonlAdapter<PiFamily> {
    family("pi", "Pi", &[".pi/agent/sessions"], Some(ProcCache::new(PROC_TTL, is_pi).with_files(is_jsonl)))
}

pub fn omp() -> JsonlAdapter<PiFamily> {
    family("omp", "oh-my-pi", &[".omp/agent/sessions"], Some(ProcCache::new(PROC_TTL, is_omp).with_files(is_jsonl)))
}

pub fn crosery() -> JsonlAdapter<PiFamily> {
    family("crosery", "Crosery Agent", &[".crosery/agent-sessions"], None)
}

pub fn commandcode() -> JsonlAdapter<PiFamily> {
    family("commandcode", "Command Code", &[".commandcode/projects"], None)
}

/// `cwd` of the `session` header. omp writes a padded `title` record first, so look a few lines in.
fn header_cwd(p: &Path) -> Option<String> {
    let f = std::fs::File::open(p).ok()?;
    let r = std::io::BufReader::new(f.take(256 * 1024));
    r.lines().take(8).map_while(Result::ok).find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        (str_of(&v, "type") == Some("session")).then(|| string_of(&v, "cwd")).flatten()
    })
}

/// Top-level transcripts of one session directory with their mtimes.
fn transcripts(dir: &Path) -> Vec<(PathBuf, i64)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            let md = e.metadata().ok()?;
            (md.is_file() && p.extension().is_some_and(|x| x == "jsonl")).then(|| (p, file_mtime_ms(&md)))
        })
        .collect()
}

impl PiFamily {
    /// Session directories whose transcripts were recorded in `cwd`.
    fn dirs_for_cwd(&self, cwd: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for root in &self.roots {
            let Ok(rd) = std::fs::read_dir(root) else { continue };
            for d in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
                let known = self.dir_cwd.read().unwrap().get(&d).cloned();
                let dir_cwd = match known {
                    Some(c) => c,
                    None => {
                        let newest = transcripts(&d).into_iter().max_by_key(|(_, m)| *m);
                        let c = newest.and_then(|(p, _)| header_cwd(&p));
                        // Directories without a readable header are retried on the next probe.
                        if c.is_some() {
                            self.dir_cwd.write().unwrap().insert(d.clone(), c.clone());
                        }
                        c
                    }
                };
                if dir_cwd.as_deref() == Some(cwd) {
                    out.push(d);
                }
            }
        }
        out
    }

    /// Map running processes to sessions, most certain evidence first:
    /// 1. `--resume <file>`, 2. transcripts the process holds open,
    /// 3. for the rest (newest process first), the newest unclaimed transcript recorded in the
    ///    process cwd and written since it started. Step 3 can swap sessions between several
    ///    idle processes sharing one cwd; it never claims a file another process holds.
    fn assign(&self, mut list: Vec<Proc>) -> Vec<LiveSession> {
        let mut out = Vec::new();
        let mut claimed = HashSet::new();
        let mut claim = |out: &mut Vec<LiveSession>, sid: SourceId, pid: u32| {
            if claimed.insert(sid.id.clone()) {
                out.push(LiveSession { id: sid.id, pid, status: None });
            }
        };
        let mut unresolved = Vec::new();
        for p in list.drain(..) {
            let resume = p.arg_after("--resume").filter(|a| a.ends_with(".jsonl")).map(PathBuf::from);
            let exact: Vec<SourceId> =
                resume.iter().chain(&p.files).filter(|f| self.is_source(f)).filter_map(|f| self.identify(f)).collect();
            if exact.is_empty() {
                unresolved.push(p);
                continue;
            }
            for sid in exact {
                claim(&mut out, sid, p.pid);
            }
        }
        unresolved.sort_by_key(|p| std::cmp::Reverse(p.start_ms));
        for p in unresolved {
            let Some(cwd) = p.cwd.as_ref().and_then(|c| c.to_str()) else { continue };
            let mut files: Vec<(PathBuf, i64)> = self
                .dirs_for_cwd(cwd)
                .iter()
                .flat_map(|d| transcripts(d))
                .filter(|(_, m)| *m >= p.start_ms - 5_000)
                .collect();
            for root in &self.roots {
                for (f, m) in transcripts(root) {
                    if m >= p.start_ms - 5_000 && header_cwd(&f).as_deref() == Some(cwd) {
                        files.push((f, m));
                    }
                }
            }
            files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
            let ids: Vec<SourceId> = files.iter().filter_map(|(f, _)| self.identify(f)).collect();
            if let Some(sid) = ids.into_iter().find(|sid| !out.iter().any(|l: &LiveSession| l.id == sid.id)) {
                claim(&mut out, sid, p.pid);
            }
        }
        out
    }
}

const IGNORED: &[&str] = &[
    "thinking_level_change",
    "custom",
    "context_edit",
    "ttsr_injection",
    "label",
    "branch_summary",
    "mode_change",
    "model_usage",
    "mcp_tool_selection",
    "service_tier_change",
    "session_state",
    "git_state",
];

impl LineDecoder for PiFamily {
    type State = ();

    fn info(&self) -> HarnessInfo {
        self.info
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        // `<id>.checkpoints.jsonl` and other dotted siblings are not transcripts.
        p.extension().is_some_and(|e| e == "jsonl")
            && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| !s.contains('.'))
            && under_any(p, &self.roots)
            && !p.iter().any(|c| c == "subagent-artifacts")
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let stem = p.file_stem()?.to_str()?;
        let parent = p
            .ancestors()
            .skip(1)
            .take_while(|a| !self.roots.iter().any(|r| r == a))
            .filter_map(|a| a.file_name()?.to_str())
            .find_map(after_ts_prefix)
            .map(str::to_owned);
        let id = match (after_ts_prefix(stem), &parent) {
            (Some(id), _) => id.to_owned(),
            (None, Some(par)) => format!("{par}.{stem}"),
            (None, None) => stem.to_owned(),
        };
        Some(SourceId { id, parent })
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        self.procs.as_ref().map(|p| self.assign(p.get()))
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let t = v.get("timestamp").and_then(ts).unwrap_or(0);
        let lid = str_of(v, "id").unwrap_or("").to_owned();
        match str_of(v, "type").unwrap_or("") {
            "session" => {
                let m = cx.meta();
                m.cwd = string_of(v, "cwd");
                m.started_at = Some(t).filter(|t| *t > 0);
                if let Some(title) = string_of(v, "title") {
                    m.title = Some((2, title));
                }
            }
            "model_change" => {
                if let Some(model) = string_of(v, "modelId").or_else(|| string_of(v, "model")) {
                    cx.meta().model = Some(model);
                }
            }
            "session_init" => {
                if let Some(model) = string_of(v, "resolvedModel") {
                    cx.meta().model = Some(model);
                }
            }
            "session_info" => title(cx, 3, str_of(v, "name")),
            "title" | "title_change" => {
                let rank = if str_of(v, "source") == Some("auto") { 2 } else { 3 };
                title(cx, rank, str_of(v, "title"));
            }
            "message" => message(v.get("message").unwrap_or(&Value::Null), &lid, t, cx),
            "compaction" => {
                let text = string_of(v, "shortSummary").or_else(|| string_of(v, "summary")).unwrap_or_default();
                cx.emit(lid, t, Body::System { subtype: "compact".into(), text });
            }
            "custom_message" => {
                let sub = string_of(v, "customType").unwrap_or_else(|| "custom".into());
                cx.emit(lid, t, Body::System { subtype: sub, text: text_of(v.get("content").unwrap_or(&Value::Null)) });
            }
            // Sums the subagent transcript's own usage, which is indexed as its own session.
            "child_usage_attributed" => {}
            "agent_status" => {
                if let Some(s) = v.get("status").and_then(|s| string_of(s, "summary")) {
                    cx.emit(lid, t, Body::System { subtype: "agent_status".into(), text: s });
                }
            }
            ty if IGNORED.contains(&ty) => {}
            ty => cx.unknown(format!("type={ty}")),
        }
    }
}

fn title(cx: &mut Cx<'_, ()>, rank: u8, t: Option<&str>) {
    if let Some(t) = t.filter(|t| !t.trim().is_empty()) {
        cx.meta().title = Some((rank, t.to_owned()));
    }
}

fn message(m: &Value, lid: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let t = if t > 0 { t } else { m.get("timestamp").and_then(ts).unwrap_or(0) };
    let content = m.get("content").unwrap_or(&Value::Null);
    match str_of(m, "role").unwrap_or("") {
        "user" => user(content, lid, t, cx),
        "assistant" => assistant(m, content, lid, t, cx),
        "toolResult" => {
            cx.emit(
                lid,
                t,
                Body::ToolResult {
                    call_id: str_of(m, "toolCallId").unwrap_or("").to_owned(),
                    name: string_of(m, "toolName"),
                    output: text_of(content),
                    is_error: m.get("isError").and_then(Value::as_bool).unwrap_or(false),
                },
            );
        }
        "bashExecution" => {
            let text = format!("$ {}\n{}", str_of(m, "command").unwrap_or(""), str_of(m, "output").unwrap_or(""));
            cx.emit(lid, t, Body::System { subtype: "bash".into(), text });
        }
        role @ ("system" | "developer") => {
            cx.emit(lid, t, Body::System { subtype: role.to_owned(), text: text_of(content) });
        }
        role => cx.unknown(format!("message.role={role}")),
    }
}

fn user(content: &Value, lid: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let mut text = String::new();
    if let Value::Array(blocks) = content {
        for (i, b) in blocks.iter().enumerate() {
            if str_of(b, "type") == Some("tool_result") {
                cx.emit(
                    format!("{lid}#{i}"),
                    t,
                    Body::ToolResult {
                        call_id: str_of(b, "tool_use_id").unwrap_or("").to_owned(),
                        name: None,
                        output: text_of(b.get("content").unwrap_or(&Value::Null)),
                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    },
                );
            } else {
                let piece = text_of(&Value::Array(vec![b.clone()]));
                if !piece.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&piece);
                }
            }
        }
    } else {
        text = text_of(content);
    }
    if !text.is_empty() {
        let synthetic = text.starts_with("[Wait interrupted") || text.starts_with("<system-reminder>");
        cx.emit(lid.to_owned(), t, Body::UserMessage { text, synthetic });
    }
}

fn assistant(m: &Value, content: &Value, lid: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let model = string_of(m, "model");
    if let Some(model) = &model {
        cx.meta().model = Some(model.clone());
    }
    let mut calls = 0;
    let mut texts = 0;
    for (i, b) in content.as_array().map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let body = match str_of(b, "type").unwrap_or("") {
            "text" => {
                texts += 1;
                Body::AssistantMessage { text: str_of(b, "text").unwrap_or("").to_owned(), model: model.clone() }
            }
            "thinking" => Body::Reasoning { text: str_of(b, "thinking").unwrap_or("").to_owned() },
            "redactedThinking" | "redacted_thinking" => Body::Reasoning { text: "[redacted]".into() },
            "toolCall" | "tool_use" => {
                calls += 1;
                Body::ToolCall {
                    call_id: str_of(b, "id").unwrap_or("").to_owned(),
                    name: str_of(b, "name").unwrap_or("").to_owned(),
                    input: json_arg(b.get("arguments").or_else(|| b.get("input")).unwrap_or(&Value::Null)),
                }
            }
            other => {
                cx.unknown(format!("assistant.block={other}"));
                continue;
            }
        };
        cx.emit(format!("{lid}#{i}"), t, body);
    }
    if let Some(u) = m.get("usage").filter(|u| u.is_object()) {
        let n = |a: &str, b: &str| u.get(a).or_else(|| u.get(b)).and_then(Value::as_u64).unwrap_or(0);
        let reasoning = n("reasoning", "reasoningTokens");
        let usage = Usage {
            input: n("input", "inputTokens"),
            output: output_with_reasoning(n("output", "outputTokens"), reasoning),
            cache_read: n("cacheRead", "cacheReadTokens"),
            cache_write: n("cacheWrite", "cacheWriteTokens"),
            reasoning,
            model: model.clone(),
            cost_usd: reported_cost(u.pointer("/cost/total").or_else(|| u.get("cost"))),
        };
        cx.emit(format!("{lid}:usage"), t, Body::Usage(usage));
    }
    if let Some(err) = string_of(m, "errorMessage") {
        cx.emit(format!("{lid}:error"), t, Body::System { subtype: "error".into(), text: err });
    }
    let end = match str_of(m, "stopReason") {
        Some(r) if ends_turn_reason(r) || r == "error" || r == "aborted" => Some(r.to_owned()),
        Some(_) => None,
        // Command Code has no stop reason: a text-only reply ends the turn.
        None => (calls == 0 && texts > 0).then(|| "stop".to_owned()),
    };
    if let Some(reason) = end {
        cx.emit(format!("{lid}:end"), t, Body::TurnEnd { reason: Some(reason) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn fam(fx: &Fixture) -> JsonlAdapter<PiFamily> {
        JsonlAdapter::new(PiFamily::new(HarnessInfo { id: "omp", name: "omp" }, vec![fx.root().to_path_buf()], None))
    }

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn pi_turn_with_tool_roundtrip() {
        let fx = Fixture::new();
        let a = fam(&fx);
        let mut s = l(
            json!({"type":"session","version":3,"id":"01a0","timestamp":"2026-10-02T11:39:57.523Z","cwd":"/proj","title":"auto title","titleSource":"auto"}),
        );
        s += &l(
            json!({"type":"model_change","id":"m","timestamp":"2026-10-02T11:39:57.600Z","provider":"p","modelId":"glm-5"}),
        );
        s += &l(json!({"type":"thinking_level_change","id":"x","thinkingLevel":"high"}));
        s += &l(
            json!({"type":"message","id":"u1","timestamp":"2026-10-02T11:40:00Z","message":{"role":"user","content":[{"type":"text","text":"build it"}]}}),
        );
        s += &l(
            json!({"type":"message","id":"a1","timestamp":"2026-10-02T11:40:01Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"ok"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"make"}}],"model":"glm-5","usage":{"input":10,"output":2,"reasoning":5,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.0125}},"stopReason":"toolUse"}}),
        );
        s += &l(
            json!({"type":"message","id":"r1","timestamp":"2026-10-02T11:40:02Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","content":[{"type":"text","text":"ok"}],"isError":false}}),
        );
        s += &l(
            json!({"type":"message","id":"a2","timestamp":"2026-10-02T11:40:03Z","message":{"role":"assistant","content":[{"type":"text","text":"built"}],"stopReason":"stop"}}),
        );
        s += &l(json!({"type":"title_change","id":"tc","title":"Build project","source":"user"}));
        let p = fx.write("-proj/2026-10-02T11-39-57-523Z_01a0.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.id, "01a0");
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "reasoning", "tool_call", "usage", "tool_result", "assistant_message", "turn_end"]
        );
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { call_id, input, .. } if call_id == "call_1" && input["command"] == "make")
        );
        assert!(
            matches!(&r.events[4].body, Body::ToolResult { name: Some(n), output, .. } if n == "bash" && output == "ok")
        );
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Build project"));
        assert_eq!(r.meta.model.as_deref(), Some("glm-5"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/proj"));
        // reasoning > output means `output` left it out: added back per the usage contract.
        assert!(matches!(&r.events[3].body, Body::Usage(u)
            if (u.output, u.reasoning) == (7, 5) && u.cost_usd == Some(0.0125) && u.model.as_deref() == Some("glm-5")));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
    }

    #[test]
    fn tool_use_stop_reason_keeps_working() {
        let fx = Fixture::new();
        let a = fam(&fx);
        let s = l(
            json!({"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"toolCall","id":"c","name":"read","arguments":{}}],"stopReason":"toolUse"}}),
        );
        let p = fx.write("-p/2026-10-02T11-39-57-523Z_x.jsonl", &s);
        assert_eq!(fx.index(&a, &p).status(), Status::Work);
    }

    #[test]
    fn subagents_and_forks_identity() {
        let fx = Fixture::new();
        let a = fam(&fx);
        let d = &a.decoder;
        let root = fx.root();
        let named = root.join("-p/2026-10-01T16-35-54-766Z_01a0f852/SpecAxis.jsonl");
        assert_eq!(
            d.identify(&named),
            Some(SourceId { id: "01a0f852.SpecAxis".into(), parent: Some("01a0f852".into()) })
        );
        let fork = root.join("-p/2026-08-29T15-54-05-510Z_aaa/forks/2026-08-29T17-02-25-694Z_bbb.jsonl");
        assert_eq!(d.identify(&fork), Some(SourceId { id: "bbb".into(), parent: Some("aaa".into()) }));
        let cc = root.join("-users-x/e56e1e3e.jsonl");
        assert_eq!(d.identify(&cc), Some(SourceId { id: "e56e1e3e".into(), parent: None }));
        assert!(!d.is_source(&root.join("-users-x/e56e1e3e.checkpoints.jsonl")));
        assert!(!d.is_source(&root.join("-p/subagent-artifacts/x.jsonl")));
    }

    #[test]
    fn process_matchers() {
        assert!(is_omp("bun /u/.bun/bin/omp --extension a.ts"));
        assert!(is_omp("bun /u/.bun/bin/omp"));
        assert!(!is_omp(
            "/u/.bun/bin/bun /u/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js __omp_worker_lsp_mux"
        ));
        assert!(!is_omp("vim /tmp/omp"));
        assert!(is_pi("node /usr/local/bin/pi --continue"));
        assert!(is_pi("node /x/@mariozechner/pi-coding-agent/dist/cli.js"));
        assert!(!is_pi("bun /x/@oh-my-pi/pi-coding-agent/dist/cli.js __omp_worker_daemon_broker"));
        assert!(!is_pi("bun /u/.bun/bin/omp"));
    }

    #[test]
    fn cwd_mapping_reads_session_headers() {
        let fx = Fixture::new();
        let a = fam(&fx);
        let hdr = |cwd: &str| l(json!({"type":"session","id":"x","cwd":cwd}));
        let titled = l(json!({"type":"title","v":1,"title":"t","pad":"    "})) + &hdr("/proj");
        let old = fx.write("-p/2026-10-01T00-00-00-000Z_old.jsonl", &titled);
        fx.write("-p/2026-10-02T00-00-00-000Z_new.jsonl", &hdr("/proj"));
        fx.write("-q/2026-10-02T00-00-00-000Z_other.jsonl", &hdr("/elsewhere"));
        fx.write("-r/2026-10-02T00-00-00-000Z_nohdr.jsonl", &l(json!({"type":"message","id":"m"})));
        let d = &a.decoder;
        assert_eq!(d.dirs_for_cwd("/proj"), vec![fx.root().join("-p")]);
        assert_eq!(d.dirs_for_cwd("/elsewhere"), vec![fx.root().join("-q")]);
        assert!(d.dirs_for_cwd("/none").is_empty());
        assert_eq!(header_cwd(&old).as_deref(), Some("/proj"));
        assert_eq!(transcripts(&fx.root().join("-p")).len(), 2);
        assert!(!d.dir_cwd.read().unwrap().contains_key(&fx.root().join("-r")), "headerless dirs are not cached");
        assert_eq!(d.live(), None, "no process probe configured");
    }

    #[test]
    fn assigns_processes_to_sessions() {
        let fx = Fixture::new();
        let a = fam(&fx);
        let hdr = l(json!({"type":"session","id":"x","cwd":"/proj"}));
        let resumed = fx.write("-p/2026-09-01T00-00-00-000Z_resumed.jsonl", &hdr);
        fx.write("-p/2026-10-02T00-00-00-000Z_a.jsonl", &hdr);
        std::thread::sleep(std::time::Duration::from_millis(30));
        fx.write("-p/2026-10-02T00-00-01-000Z_b.jsonl", &hdr);
        std::thread::sleep(std::time::Duration::from_millis(30));
        // Newest file, but held open by pid 6: the cwd fallback must not hand it to pid 2.
        let held = fx.write("-p/2026-09-02T00-00-00-000Z_held.jsonl", &hdr);
        let now = uniflo_core::util::now_ms();
        let proc_ = |pid, start_ms, args: String, files: Vec<PathBuf>| Proc {
            pid,
            start_ms,
            args,
            cwd: Some(PathBuf::from("/proj")),
            files,
        };
        let procs = vec![
            proc_(1, now - 60_000, "bun /b/omp".into(), vec![]),
            proc_(2, now - 30_000, "bun /b/omp".into(), vec![]),
            proc_(3, now - 10_000, format!("bun /b/omp --resume {}", resumed.display()), vec![]),
            // Holds its transcript open: exact regardless of process order.
            proc_(6, now - 5_000, "bun /b/omp".into(), vec![held.clone(), PathBuf::from("/tmp/other.jsonl")]),
            // Started after every transcript was last written: nothing to claim.
            proc_(4, now + 60_000, "bun /b/omp".into(), vec![]),
            Proc { cwd: Some(PathBuf::from("/other")), ..proc_(5, now - 1_000, "bun /b/omp".into(), vec![]) },
        ];
        let mut got: Vec<(u32, String)> = a.decoder.assign(procs).into_iter().map(|l| (l.pid, l.id)).collect();
        got.sort();
        let want = [(1, "a"), (2, "b"), (3, "resumed"), (6, "held")];
        assert_eq!(got, want.map(|(p, s)| (p, s.to_owned())));
    }

    #[test]
    fn command_code_anthropic_blocks() {
        let fx = Fixture::new();
        let a = fam(&fx);
        let mut s = l(json!({"type":"session","id":"cc1","timestamp":"2026-08-31T01:00:00Z","cwd":"/c"}));
        s += &l(json!({"type":"message","id":"u","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}));
        s += &l(
            json!({"type":"message","id":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{"p":1}}],"usage":{"inputTokens":3,"outputTokens":1}}}),
        );
        s += &l(
            json!({"type":"message","id":"r","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"x"}]}]}}),
        );
        s += &l(
            json!({"type":"message","id":"b","message":{"role":"assistant","content":[{"type":"text","text":"bye"}]}}),
        );
        let p = fx.write("-c/cc1.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "tool_call", "usage", "tool_result", "assistant_message", "turn_end"]
        );
        assert!(matches!(&r.events[2].body, Body::Usage(u) if u.input == 3));
        assert_eq!(r.status(), Status::Idle);
    }
}
