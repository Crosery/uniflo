//! `uniflo mcp`: stdio MCP server (JSON-RPC 2.0, one message per line, protocol written by
//! hand). Every tool is a `GET` on the gateway's REST routes — the daemon's, or the same routes
//! answered in-process over a one-shot index when no daemon is reachable — so `structuredContent`
//! is the REST response (arrays wrapped in an object). stdout carries protocol messages only;
//! diagnostics go to stderr. The server never prompts.

use crate::agent::build_context;
use crate::client::Client;
use crate::{Cli, enc};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uniflo_core::util::preview;
use uniflo_core::{Engine, EngineOptions};
use uniflo_gateway::GuardOptions;
use uniflo_schema::search::{HIGHLIGHT_END, HIGHLIGHT_START};
use uniflo_search::fts::{Fts, FtsOptions};

/// Newest first; the first one is offered when the client asks for something else.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const LOCAL_NOTE: &str = "守护进程未运行，结果来自一次性索引（uniflo daemon 未在运行，本次在进程内建索引）。";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub fn run(cli: &Cli) -> Result<()> {
    let mut server = Server::new(cli.url.clone(), cli.token.clone(), cli.local);
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            out.write_all(reply.to_string().as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct RpcError(i64, String);

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError(INVALID_PARAMS, msg.into())
}

struct Server {
    url: String,
    token: Option<String>,
    local_only: bool,
    local: Option<Local>,
    noted: bool,
}

/// No daemon: an index built once in this process, served by the gateway's own router.
struct Local {
    rt: tokio::runtime::Runtime,
    engine: Arc<Engine>,
    router: axum::Router,
    usage_ready: bool,
    fts: bool,
}

/// One tool answer before it is wrapped into a `CallToolResult`.
struct Out {
    text: String,
    structured: Value,
    is_error: bool,
}

impl Out {
    fn ok(text: String, structured: Value) -> Out {
        Out { text, structured, is_error: false }
    }

    fn error(msg: String) -> Out {
        Out { structured: json!({ "error": msg }), text: msg, is_error: true }
    }
}

impl Server {
    fn new(url: String, token: Option<String>, local_only: bool) -> Server {
        Server { url, token, local_only, local: None, noted: false }
    }

    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error_reply(Value::Null, PARSE_ERROR, &format!("parse error: {e}"))),
        };
        match msg {
            Value::Array(items) if !items.is_empty() => {
                let replies: Vec<Value> = items.into_iter().filter_map(|m| self.handle(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            m => self.handle(m),
        }
    }

    fn handle(&mut self, msg: Value) -> Option<Value> {
        let Value::Object(m) = msg else {
            return Some(error_reply(Value::Null, INVALID_REQUEST, "not a JSON-RPC object"));
        };
        let id = m.get("id").cloned();
        let Some(method) = m.get("method").and_then(Value::as_str) else {
            // A response to a request we never send, or garbage without an id: nothing to answer.
            return id
                .filter(|_| !m.contains_key("result") && !m.contains_key("error"))
                .map(|id| error_reply(id, INVALID_REQUEST, "missing method"));
        };
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call(&params),
            _ if id.is_none() => return None,
            _ => Err(RpcError(METHOD_NOT_FOUND, format!("method not found: {method}"))),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(RpcError(code, msg)) => error_reply(id, code, &msg),
        })
    }

    fn call(&mut self, params: &Value) -> Result<Value, RpcError> {
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| invalid("missing tool name"))?;
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(o)) => o,
            Some(_) => return Err(invalid("arguments must be an object")),
        };
        let a = Args(args);
        let mut out = match name {
            "uniflo_sessions" => self.sessions(&a)?,
            "uniflo_session" => self.session(&a)?,
            "uniflo_search" => self.search(&a)?,
            "uniflo_usage" => self.usage(&a)?,
            "uniflo_session_usage" => self.session_usage(&a)?,
            "uniflo_models" => self.models()?,
            "uniflo_resume" => self.resume(&a)?,
            "uniflo_memory" => self.memory(&a)?,
            "uniflo_context" => self.context(&a)?,
            "uniflo_status" => self.status()?,
            _ => return Err(invalid(format!("unknown tool: {name}"))),
        };
        if self.local.is_some() && !self.noted {
            self.noted = true;
            out.text = format!("{LOCAL_NOTE}\n\n{}", out.text);
        }
        Ok(json!({
            "content": [{ "type": "text", "text": out.text }],
            "structuredContent": out.structured,
            "isError": out.is_error,
        }))
    }

    // ------------------------------------------------------------------ backend

    /// `GET path` on the daemon, else on the in-process routes. `Err` is a message for the agent.
    fn get(&mut self, path: &str) -> Result<Value, String> {
        let (status, body) = self.raw(path).map_err(|e| format!("{e:#}"))?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("bad JSON from {path}: {e}"))?;
        if status == 200 {
            return Ok(v);
        }
        Err(v.get("error").and_then(Value::as_str).map_or_else(|| format!("HTTP {status}"), str::to_owned))
    }

    fn raw(&mut self, path: &str) -> Result<(u16, Vec<u8>)> {
        if !self.local_only && self.local.is_none() {
            let c = Client::new(&self.url, self.token.clone())?;
            match c.get(path) {
                Ok(mut r) => return Ok((r.status, r.body()?)),
                Err(e) if e.downcast_ref::<std::io::Error>().is_some() => {}
                Err(e) => return Err(e),
            }
        }
        let local = self.local()?;
        Ok(local.rt.block_on(uniflo_gateway::agent::get_in_process(local.router.clone(), path)))
    }

    fn local(&mut self) -> Result<&mut Local> {
        if self.local.is_none() {
            eprintln!("uniflo mcp: no daemon at {}, indexing in-process", self.url);
            let engine = Engine::new(uniflo_adapters::all(), EngineOptions::default());
            engine.index();
            let _ = engine.save_cache();
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            let router = uniflo_gateway::router(engine.clone(), GuardOptions::default());
            self.local = Some(Local { rt, engine, router, usage_ready: false, fts: false });
        }
        self.local.as_mut().ok_or_else(|| anyhow!("no local index"))
    }

    /// In-process mode: build the usage ledgers before the first usage answer.
    fn need_usage(&mut self) {
        if let Some(l) = self.local.as_mut().filter(|l| !l.usage_ready) {
            l.engine.index_usage();
            l.usage_ready = true;
        }
    }

    /// In-process mode: catch the on-disk full-text index up (bounded wait) and serve it.
    fn need_fts(&mut self) {
        let Some(l) = self.local.as_mut().filter(|l| !l.fts) else { return };
        l.fts = true;
        match Fts::start(l.engine.clone(), FtsOptions { pause: 0.0, ..Default::default() }) {
            Ok(f) => {
                let t0 = Instant::now();
                while !f.wait_idle(Duration::from_secs(1)) && t0.elapsed() < Duration::from_secs(20) {}
                l.router =
                    uniflo_gateway::router_with_fts(l.engine.clone(), GuardOptions::default(), Some(Arc::new(f)));
            }
            Err(e) => eprintln!("uniflo mcp: full-text index unavailable: {e:#}"),
        }
    }

    /// Route through the backend once to learn whether it is local, then run `f`.
    fn prepare(&mut self, usage: bool, fts: bool) {
        if self.local.is_none() && !self.local_only {
            let alive = Client::new(&self.url, self.token.clone()).is_ok_and(|c| c.alive());
            if alive {
                return;
            }
        }
        if self.local().is_err() {
            return;
        }
        if usage {
            self.need_usage();
        }
        if fts {
            self.need_fts();
        }
    }

    // ------------------------------------------------------------------ tools

    fn sessions(&mut self, a: &Args) -> Result<Out, RpcError> {
        let q = a.str("q")?.unwrap_or("");
        let limit = a.int("limit", 1, 100, 20)?;
        Ok(match self.get(&format!("/v1/sessions?limit={limit}&q={}", enc(q))) {
            Ok(v) => {
                let list = v.as_array().cloned().unwrap_or_default();
                let mut text = format!("{} sessions", list.len());
                for s in &list {
                    text.push('\n');
                    text.push_str(&session_line(s));
                }
                Out::ok(text, json!({ "sessions": list }))
            }
            Err(e) => Out::error(e),
        })
    }

    fn session(&mut self, a: &Args) -> Result<Out, RpcError> {
        let (key, frag) = parse_ref(a.req_str("key")?);
        let around = a.str("around")?.map(str::to_owned).or(frag);
        let limit = a.int("limit", 1, 200, 60)?;
        let before = a.opt_int("before", 0, i64::MAX)?;
        let max_text = a.int("max_text", 0, 1_000_000, 4000)?;
        let tools = a.bool("include_tools", false)?;
        let reasoning = a.bool("include_reasoning", false)?;
        let sess = match self.find_session(&key) {
            Ok(s) => s,
            Err(e) => return Ok(Out::error(e)),
        };
        let skey = sess["key"].as_str().unwrap_or(&key).to_owned();
        let mut path = format!("/v1/sessions/{}/events?limit={limit}&max_text={max_text}", enc(&skey));
        match (&around, before) {
            (Some(id), _) => path.push_str(&format!("&around={}", enc(id))),
            (None, Some(b)) => path.push_str(&format!("&before={b}")),
            _ => {}
        }
        let page = match self.get(&path) {
            Ok(v) => v,
            Err(e) => return Ok(Out::error(e)),
        };
        let (mut omitted_tools, mut omitted_reasoning) = (0, 0);
        let events: Vec<Value> = page["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| match e["kind"].as_str().unwrap_or("") {
                "tool_call" | "tool_result" if !tools => {
                    omitted_tools += 1;
                    false
                }
                "reasoning" if !reasoning => {
                    omitted_reasoning += 1;
                    false
                }
                _ => true,
            })
            .collect();
        let mut text = session_line(&sess);
        for e in &events {
            if let Some(l) = event_text(e) {
                text.push('\n');
                text.push_str(&l);
            }
        }
        if omitted_tools + omitted_reasoning > 0 {
            let _ = write!(
                text,
                "\n({omitted_tools} tool events and {omitted_reasoning} reasoning events hidden: include_tools / include_reasoning)"
            );
        }
        if let Some(b) = page["next_before"].as_u64() {
            let _ = write!(text, "\nolder events: before={b}");
        }
        let mut structured = json!({
            "session": sess,
            "events": events,
            "next_before": page["next_before"],
            "omitted": { "tool_events": omitted_tools, "reasoning": omitted_reasoning },
        });
        if let Some(id) = around {
            structured["around"] = json!(id);
        }
        Ok(Out::ok(text, structured))
    }

    /// A full key, else a unique key / id prefix (like the CLI).
    fn find_session(&mut self, key: &str) -> Result<Value, String> {
        match self.get(&format!("/v1/sessions/{}", enc(key))) {
            Ok(s) => Ok(s),
            Err(e) => {
                let hits = self.get(&format!("/v1/sessions?limit=2&q={}", enc(&format!("id:{key}"))))?;
                match hits.as_array().map(Vec::as_slice) {
                    Some([one]) => Ok(one.clone()),
                    Some([_, _, ..]) => Err(format!("{key} is ambiguous; pass the full session key")),
                    _ => Err(e),
                }
            }
        }
    }

    fn search(&mut self, a: &Args) -> Result<Out, RpcError> {
        let q = a.req_str("q")?.to_owned();
        let filter = a.str("filter")?.map(str::to_owned);
        let limit = a.int("limit", 1, 30, 10)?;
        self.prepare(false, true);
        let mut path = format!("/v1/search?q={}&limit={limit}", enc(&q));
        if let Some(f) = &filter {
            path.push_str(&format!("&filter={}", enc(f)));
        }
        Ok(match self.get(&path) {
            Ok(v) => {
                let mut text = format!("{} matching sessions", v["total"]);
                if v["indexing"] == true {
                    text.push_str(" (index still building: results may be incomplete)");
                }
                for s in v["results"].as_array().into_iter().flatten() {
                    let key = s["session"].as_str().unwrap_or("");
                    let _ = write!(
                        text,
                        "\n- {} · {} · {} · {key}",
                        s["title"].as_str().unwrap_or("(untitled)"),
                        s["harness"].as_str().unwrap_or(""),
                        s["cwd"].as_str().unwrap_or("")
                    );
                    for h in s["hits"].as_array().into_iter().flatten() {
                        let snippet =
                            h["snippet"].as_str().unwrap_or("").replace([HIGHLIGHT_START, HIGHLIGHT_END], "**");
                        let _ = write!(
                            text,
                            "\n  [{}] {} — uniflo://session/{key}#{}",
                            h["kind"].as_str().unwrap_or(""),
                            snippet,
                            h["event"].as_str().unwrap_or("")
                        );
                    }
                }
                Out::ok(text, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn usage(&mut self, a: &Args) -> Result<Out, RpcError> {
        let mut qs = Vec::new();
        for k in ["group_by", "q", "since", "until", "under"] {
            if let Some(v) = a.str(k)? {
                qs.push(format!("{k}={}", enc(v)));
            }
        }
        if let Some(d) = a.opt_int("depth", 1, 32)? {
            qs.push(format!("depth={d}"));
        }
        if let Some(l) = a.opt_int("limit", 1, 1000)? {
            qs.push(format!("limit={l}"));
        }
        self.prepare(true, false);
        Ok(match self.get(&format!("/v1/usage?{}", qs.join("&"))) {
            Ok(v) => {
                let mut text = format!("usage by {}", v["group_by"].as_str().unwrap_or("?"));
                for r in v["rows"].as_array().into_iter().flatten() {
                    text.push('\n');
                    text.push_str(&usage_line(r["label"].as_str().unwrap_or(""), r));
                }
                text.push('\n');
                text.push_str(&usage_line("total", &v["totals"]));
                if v["indexing"]["ready"] == false {
                    text.push_str("\n(usage index still building: totals will grow)");
                }
                Out::ok(text, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn session_usage(&mut self, a: &Args) -> Result<Out, RpcError> {
        let key = parse_ref(a.req_str("key")?).0;
        self.prepare(true, false);
        Ok(match self.get(&format!("/v1/sessions/{}/usage", enc(&key))) {
            Ok(v) => {
                let mut text = usage_line(&key, &v["totals"]);
                for t in v["turns"].as_array().into_iter().flatten() {
                    text.push('\n');
                    text.push_str(&usage_line(&format!("turn {}", t["turn"]), t));
                }
                Out::ok(text, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn models(&mut self) -> Result<Out, RpcError> {
        self.prepare(true, false);
        Ok(match self.get("/v1/models") {
            Ok(v) => {
                let list = v.as_array().cloned().unwrap_or_default();
                let mut text = format!("{} models", list.len());
                for m in &list {
                    let _ = write!(
                        text,
                        "\n- {} · {} steps · {} · context {}",
                        m["model"].as_str().unwrap_or("?"),
                        m["steps"],
                        money(&m["cost_usd"]),
                        m["context_limit"]
                    );
                }
                Out::ok(text, json!({ "models": list }))
            }
            Err(e) => Out::error(e),
        })
    }

    fn resume(&mut self, a: &Args) -> Result<Out, RpcError> {
        let key = parse_ref(a.req_str("key")?).0;
        let key = match self.find_session(&key) {
            Ok(s) => s["key"].as_str().unwrap_or(&key).to_owned(),
            Err(e) => return Ok(Out::error(e)),
        };
        Ok(match self.get(&format!("/v1/sessions/{}/resume", enc(&key))) {
            Ok(v) => {
                let text = if v["supported"] == true {
                    format!("Run in a terminal (not executed by uniflo):\n{}", v["command"].as_str().unwrap_or(""))
                } else {
                    format!("{key} cannot be resumed: {}", v["reason"].as_str().unwrap_or(""))
                };
                Out::ok(text, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn memory(&mut self, a: &Args) -> Result<Out, RpcError> {
        if let Some(path) = a.str("path")? {
            return Ok(match self.get(&format!("/v1/memory/file?path={}", enc(path))) {
                Ok(v) => {
                    let mut text = format!("{} ({} bytes)", v["path"].as_str().unwrap_or(""), v["bytes"]);
                    if v["truncated"] == true {
                        text.push_str(", truncated");
                    }
                    let _ = write!(text, "\n\n{}", v["content"].as_str().unwrap_or(""));
                    Out::ok(text, v)
                }
                Err(e) => Out::error(e),
            });
        }
        let mut path = "/v1/memory".to_owned();
        if let Some(c) = a.str("cwd")? {
            path.push_str(&format!("?cwd={}", enc(c)));
        }
        Ok(match self.get(&path) {
            Ok(v) => {
                let list = v.as_array().cloned().unwrap_or_default();
                let mut text = format!("{} files (read one with path=…)", list.len());
                for f in &list {
                    let _ = write!(
                        text,
                        "\n- {} · {} · {} · {} bytes",
                        f["path"].as_str().unwrap_or(""),
                        f["scope"].as_str().unwrap_or(""),
                        f["harness"].as_str().unwrap_or(""),
                        f["bytes"]
                    );
                }
                Out::ok(text, json!({ "files": list }))
            }
            Err(e) => Out::error(e),
        })
    }

    fn context(&mut self, a: &Args) -> Result<Out, RpcError> {
        let cwd = a.req_str("cwd")?;
        let limit = a.int("limit", 1, 50, 5)? as usize;
        let since = a.str("since")?.unwrap_or("14d").to_owned();
        let home = uniflo_core::util::home();
        let cwd = match cwd.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{}{rest}", home.display()),
            _ => cwd.to_owned(),
        };
        if !std::path::Path::new(&cwd).is_absolute() {
            return Ok(Out::error(format!("cwd must be an absolute path: {cwd}")));
        }
        self.prepare(true, false);
        let mut get = |p: &str| self.get(p).map_err(|e| anyhow!(e));
        Ok(match build_context(&mut get, std::path::Path::new(&cwd), limit, &since) {
            Ok(r) => {
                let md = uniflo_core::context::markdown(&r);
                let text = if md.is_empty() { format!("No sessions in {} since {since}.", r.project) } else { md };
                Out::ok(text, serde_json::to_value(&r).unwrap_or_default())
            }
            Err(e) => Out::error(format!("{e:#}")),
        })
    }

    fn status(&mut self) -> Result<Out, RpcError> {
        let health = match self.get("/v1/health") {
            Ok(v) => v,
            Err(e) => return Ok(Out::error(e)),
        };
        let harnesses = self.get("/v1/harnesses").unwrap_or_default();
        let source = if self.local.is_some() { "in-process index" } else { "daemon" };
        let mut text = format!(
            "uniflo {} ({source}) · {} sessions, {} working",
            health["version"].as_str().unwrap_or("?"),
            health["sessions"],
            health["working"]
        );
        for h in harnesses.as_array().into_iter().flatten().filter(|h| h["sessions"].as_u64() > Some(0)) {
            let _ = write!(
                text,
                "\n- {}: {} sessions, {} working",
                h["id"].as_str().unwrap_or(""),
                h["sessions"],
                h["working"]
            );
        }
        Ok(Out::ok(text, json!({ "health": health, "harnesses": harnesses })))
    }
}

fn error_reply(id: Value, code: i64, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "uniflo", "title": "Uniflo", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Read-only access to every local agent-harness session (Claude Code, Codex, omp, OpenCode, Gemini CLI, …). \
            Find sessions with uniflo_sessions (search syntax: h:claude s:work in:<cwd> since:2d 'fuzzy), read one with uniflo_session, \
            search transcript text with uniflo_search, costs with uniflo_usage. Session keys look like claude:<id>; \
            uniflo://session/<key>#<event-id> references open with uniflo_session.",
    })
}

/// `uniflo://session/<key>#<event-id>` → (key, event id); anything else is a key.
fn parse_ref(s: &str) -> (String, Option<String>) {
    let Some(rest) = s.strip_prefix("uniflo://session/") else { return (s.to_owned(), None) };
    let (key, frag) = match rest.split_once('#') {
        Some((k, f)) => (k, Some(f).filter(|f| !f.is_empty()).map(decode)),
        None => (rest, None),
    };
    (decode(key), frag)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn session_line(s: &Value) -> String {
    let label = s["title"].as_str().or_else(|| s["preview"].as_str()).unwrap_or("(untitled)");
    let mut line = format!(
        "- {} · {} · {} · updated {}",
        preview(label, 100),
        s["harness"].as_str().unwrap_or(""),
        s["status"].as_str().unwrap_or(""),
        crate::render::date_time(s["updated_at"].as_i64().unwrap_or(0))
    );
    if let Some(c) = s["cwd"].as_str() {
        let _ = write!(line, " · {c}");
    }
    if let Some(u) = s.get("usage").filter(|u| u.is_object()) {
        let _ = write!(line, " · {}", money(&u["cost_usd"]));
    }
    let _ = write!(line, " · {}", s["key"].as_str().unwrap_or(""));
    line
}

/// One readable line per event (usage and turn markers are skipped).
fn event_text(e: &Value) -> Option<String> {
    let time = crate::render::date_time(e["ts"].as_i64().unwrap_or(0));
    let s = |k: &str| e[k].as_str().unwrap_or("").to_owned();
    let (tag, body) = match e["kind"].as_str()? {
        "user_message" => (if e["synthetic"] == true { "user (injected)".into() } else { "user".into() }, s("text")),
        "assistant_message" => ("assistant".to_owned(), s("text")),
        "reasoning" => ("reasoning".to_owned(), s("text")),
        "tool_call" => (format!("tool_call {}", s("name")), preview(&e["input"].to_string(), 600)),
        "tool_result" => {
            let err = if e["is_error"] == true { " error" } else { "" };
            (format!("tool_result {}{err}", s("name")), preview(&s("output"), 600))
        }
        "system" => (format!("system {}", s("subtype")), s("text")),
        _ => return None,
    };
    Some(format!("[{time} {tag}] {body}"))
}

fn usage_line(label: &str, r: &Value) -> String {
    format!(
        "- {label}: {} · {} steps · in {} out {} cache r/w {}/{}{}",
        money(&r["cost_usd"]),
        r["steps"],
        r["input"],
        r["output"],
        r["cache_read"],
        r["cache_write"],
        match r["unpriced_steps"].as_u64() {
            Some(n) if n > 0 => format!(" · {n} unpriced steps"),
            _ => String::new(),
        }
    )
}

fn money(v: &Value) -> String {
    v.as_f64().map_or_else(|| "cost unknown".to_owned(), |c| format!("${c:.4}"))
}

/// Typed access to `arguments`: a present value of the wrong type is `-32602`; out-of-range
/// numbers are clamped.
struct Args<'a>(&'a Map<String, Value>);

impl Args<'_> {
    fn str(&self, k: &str) -> Result<Option<&str>, RpcError> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str()).filter(|s| !s.is_empty())),
            Some(_) => Err(invalid(format!("{k} must be a string"))),
        }
    }

    fn req_str(&self, k: &str) -> Result<&str, RpcError> {
        self.str(k)?.ok_or_else(|| invalid(format!("{k} is required")))
    }

    fn opt_int(&self, k: &str, lo: i64, hi: i64) -> Result<Option<i64>, RpcError> {
        let n = match self.0.get(k) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::Number(n)) => n,
            Some(_) => return Err(invalid(format!("{k} must be an integer"))),
        };
        let v = if let Some(i) = n.as_i64() {
            i
        } else if n.as_u64().is_some() {
            i64::MAX
        } else {
            match n.as_f64() {
                Some(f) if f.fract() == 0.0 => f.clamp(i64::MIN as f64, i64::MAX as f64) as i64,
                _ => return Err(invalid(format!("{k} must be an integer"))),
            }
        };
        Ok(Some(v.clamp(lo, hi)))
    }

    fn int(&self, k: &str, lo: i64, hi: i64, default: i64) -> Result<i64, RpcError> {
        Ok(self.opt_int(k, lo, hi)?.unwrap_or(default))
    }

    fn bool(&self, k: &str, default: bool) -> Result<bool, RpcError> {
        match self.0.get(k) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(invalid(format!("{k} must be a boolean"))),
        }
    }
}

fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
        "annotations": { "title": title, "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
    })
}

fn tools() -> Vec<Value> {
    let key = json!({ "type": "string", "description": "Session key (claude:<id>), a unique id prefix, or uniflo://session/<key>#<event-id>" });
    vec![
        tool(
            "uniflo_sessions",
            "List sessions",
            "List or search agent sessions across harnesses, newest first. Same as GET /v1/sessions.",
            json!({
                "q": { "type": "string", "description": "Search syntax: h:claude s:work in:<cwd substring> since:2h|7d before: is:root|sub|live id:<prefix>, other words fuzzy-match title / first prompt / cwd" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
            }),
            &[],
        ),
        tool(
            "uniflo_session",
            "Read a session",
            "Session metadata and a window of its transcript (newest page, an older page via before, or centred on an event via around).",
            json!({
                "key": key,
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 60, "description": "Events fetched (before hiding tools / reasoning)" },
                "before": { "type": "integer", "minimum": 0, "description": "next_before of the previous page" },
                "around": { "type": "string", "description": "Event id to centre the window on" },
                "max_text": { "type": "integer", "minimum": 0, "default": 4000, "description": "Cut each text field to this many bytes; 0 = no limit" },
                "include_tools": { "type": "boolean", "default": false },
                "include_reasoning": { "type": "boolean", "default": false },
            }),
            &["key"],
        ),
        tool(
            "uniflo_search",
            "Full-text search",
            "Search message, reasoning and tool text of every session (Chinese words and code substrings); hits grouped by session with uniflo:// references.",
            json!({
                "q": { "type": "string", "description": "Terms are ANDed; \"a phrase\"; -excluded" },
                "filter": { "type": "string", "description": "Session filter in the uniflo_sessions q syntax" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 30, "default": 10 },
            }),
            &["q"],
        ),
        tool(
            "uniflo_usage",
            "Token usage and cost",
            "Tokens and API-equivalent cost aggregated by one dimension. Same as GET /v1/usage.",
            json!({
                "group_by": { "type": "string", "enum": ["harness", "model", "project", "cwd", "dir", "day", "hour", "weekday", "session", "weekday_hour"], "default": "harness" },
                "q": { "type": "string", "description": "Session filter (search syntax); since:/before: apply to step time" },
                "since": { "type": "string", "description": "30m, 2h, 7d, YYYY-MM-DD or epoch ms" },
                "until": { "type": "string" },
                "under": { "type": "string", "description": "Only cwds under this directory" },
                "depth": { "type": "integer", "minimum": 1, "maximum": 32 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000 },
            }),
            &[],
        ),
        tool(
            "uniflo_session_usage",
            "Usage of one session",
            "Per-step tokens, cost and context use of one session, with per-turn totals.",
            json!({ "key": key }),
            &["key"],
        ),
        tool(
            "uniflo_models",
            "Models",
            "Models seen in sessions with their prices, context limits, steps and cost.",
            json!({}),
            &[],
        ),
        tool(
            "uniflo_resume",
            "Resume command",
            "The shell command that continues a session in its own harness. Only returns the command; nothing is run.",
            json!({ "key": key }),
            &["key"],
        ),
        tool(
            "uniflo_memory",
            "Agent memory files",
            "List the instruction and memory files agents load for a directory (CLAUDE.md, AGENTS.md, Claude project memory, Cursor rules…), or read one listed file.",
            json!({
                "cwd": { "type": "string", "description": "Absolute project directory; omit for global files only" },
                "path": { "type": "string", "description": "Read this listed file (at most 256 KB)" },
            }),
            &[],
        ),
        tool(
            "uniflo_context",
            "Recent project sessions",
            "Recent sessions of the project (git root) containing cwd, one line each, with how to read them.",
            json!({
                "cwd": { "type": "string", "description": "Absolute directory inside the project" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 5 },
                "since": { "type": "string", "default": "14d" },
            }),
            &["cwd"],
        ),
        tool(
            "uniflo_status",
            "Status",
            "Daemon version, session counts and the harnesses found on this machine.",
            json!({}),
            &[],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new("http://127.0.0.1:9".into(), None, false)
    }

    fn call(s: &mut Server, line: &str) -> Value {
        s.handle_line(line).expect("a reply")
    }

    #[test]
    fn handshake_and_errors() {
        let mut s = server();
        let r =
            call(&mut s, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["capabilities"], json!({"tools":{"listChanged":false}}));
        let r =
            call(&mut s, r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#);
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let r = call(&mut s, r#"{"jsonrpc":"2.0","id":"t","method":"tools/list"}"#);
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10);
        assert!(tools.iter().all(|t| t["annotations"]["readOnlyHint"] == true && t["inputSchema"]["type"] == "object"));
        assert_eq!(call(&mut s, r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)["result"], json!({}));
        assert_eq!(call(&mut s, r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#)["error"]["code"], -32601);
        assert_eq!(call(&mut s, "{not json")["error"]["code"], -32700);
        let bad = [
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"uniflo_sessions","arguments":{"limit":"many"}}}"#,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"uniflo_sessions","arguments":{"limit":2.5}}}"#,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"uniflo_session","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"uniflo_session","arguments":{"key":"a","include_tools":"yes"}}}"#,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"uniflo_search","arguments":[]}}"#,
        ];
        for b in bad {
            assert_eq!(call(&mut s, b)["error"]["code"], -32602, "{b}");
        }
        let batch = call(
            &mut s,
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#,
        );
        assert_eq!(batch.as_array().unwrap().len(), 1);
    }

    #[test]
    fn refs_and_clamping() {
        assert_eq!(parse_ref("claude:abc"), ("claude:abc".into(), None));
        assert_eq!(parse_ref("uniflo://session/claude:abc#msg_1:2"), ("claude:abc".into(), Some("msg_1:2".into())));
        assert_eq!(parse_ref("uniflo://session/craft:a%2Fb#e"), ("craft:a/b".into(), Some("e".into())));
        let m: Map<String, Value> = serde_json::from_value(json!({"a": 1000, "b": -5, "c": 3.0, "d": 1e300})).unwrap();
        let a = Args(&m);
        assert_eq!(a.int("a", 1, 100, 20).unwrap(), 100);
        assert_eq!(a.int("b", 1, 100, 20).unwrap(), 1);
        assert_eq!(a.int("c", 1, 100, 20).unwrap(), 3);
        assert_eq!(a.int("d", 1, 100, 20).unwrap(), 100);
        assert_eq!(a.int("missing", 1, 100, 20).unwrap(), 20);
    }
}
