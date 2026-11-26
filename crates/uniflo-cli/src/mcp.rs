//! `uniflo mcp`: stdio MCP server (JSON-RPC 2.0, one message per line, protocol written by
//! hand). Every tool is a `GET` on the gateway's REST routes — the daemon's, or the same routes
//! answered in-process over a one-shot index when no daemon is reachable. `content` is a compact
//! text summary built for an agent to act on directly (keys, `#event` ids, titles, harness,
//! model, `~`-shortened cwd, relative time, cost — never the absolute `source` path); the
//! `structured` tool argument (default `false`) additionally returns `structuredContent`, the
//! REST response verbatim (arrays wrapped in an object), for callers that need exact JSON
//! fields. stdout carries protocol messages only; diagnostics go to stderr. The server never
//! prompts.

use crate::agent::build_context;
use crate::client::Client;
use crate::{Cli, enc};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};
use uniflo_core::util::{now_ms, preview};
use uniflo_core::{Engine, EngineOptions};
use uniflo_gateway::GuardOptions;
use uniflo_schema::AgentMessage;
use uniflo_schema::search::{HIGHLIGHT_END, HIGHLIGHT_START};
use uniflo_search::fts::{Fts, FtsOptions};

/// Newest first; the first one is offered when the client asks for something else.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub const LOCAL_NOTE: &str = "守护进程未运行，结果来自一次性索引（uniflo daemon 未在运行，本次在进程内建索引）。";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Detect the owning session key from harness-provided environment variables when available.
fn env_self_key() -> Option<String> {
    if let Ok(v) = std::env::var("UNIFLO_SESSION_KEY") {
        let s = v.trim();
        if !s.is_empty() {
            return Some(s.to_owned());
        }
    }
    for (var, prefix) in
        [("ANTIGRAVITY_CONVERSATION_ID", "antigravity"), ("CLAUDE_SESSION_ID", "claude"), ("CODEX_THREAD_ID", "codex")]
    {
        if let Ok(v) = std::env::var(var) {
            let s = v.trim();
            if !s.is_empty() {
                return Some(if s.contains(':') { s.to_owned() } else { format!("{prefix}:{s}") });
            }
        }
    }
    None
}

fn env_wake_hop() -> Option<u8> {
    std::env::var("UNIFLO_WAKE_HOP").ok().and_then(|v| v.trim().parse::<u8>().ok())
}

/// Shared state of the per-process SSE message watchdog bound to `uniflo mcp --message`.
/// Starts automatically with the MCP server process and shuts down when stdin closes.
struct Watchdog {
    cwd: Option<String>,
    self_key: RwLock<Option<String>>,
    sent_ids: Mutex<HashSet<String>>,
    shutdown: Arc<AtomicBool>,
    initialized: Arc<AtomicBool>,
    subscribed: Arc<AtomicBool>,
    queue: Arc<(Mutex<VecDeque<AgentMessage>>, Condvar)>,
}

impl Watchdog {
    fn start(url: String, token: Option<String>, stdout: Arc<Mutex<std::io::Stdout>>) -> Arc<Watchdog> {
        let cwd = std::env::current_dir().ok().map(|p| p.display().to_string());
        let initial_self = env_self_key();
        let wd = Arc::new(Watchdog {
            cwd: cwd.clone(),
            self_key: RwLock::new(initial_self),
            sent_ids: Mutex::new(HashSet::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            initialized: Arc::new(AtomicBool::new(false)),
            subscribed: Arc::new(AtomicBool::new(false)),
            queue: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
        });
        let w = wd.clone();
        std::thread::spawn(move || watchdog_loop(url, token, cwd, w, stdout));
        wd
    }

    fn self_key(&self) -> Option<String> {
        self.self_key.read().unwrap().clone()
    }

    fn set_self_key(&self, key: String) {
        if key.trim().is_empty() {
            return;
        }
        let mut g = self.self_key.write().unwrap();
        if g.as_deref() != Some(key.as_str()) {
            *g = Some(key);
        }
    }

    fn remember_sent(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let mut g = self.sent_ids.lock().unwrap();
        if g.len() > 512 {
            g.clear();
        }
        g.insert(id.to_owned());
        // Purge from queue in case SSE broadcast raced ahead of remember_sent.
        let mut q = self.queue.0.lock().unwrap();
        q.retain(|m| m.id != id);
    }

    /// True iff `msg` is genuinely addressed to this MCP process (never self-sent, never another peer's).
    fn accepts(&self, msg: &AgentMessage) -> bool {
        if self.sent_ids.lock().unwrap().contains(&msg.id) {
            return false;
        }
        if let Some(sk) = self.self_key() {
            if msg.from == sk {
                return false;
            }
            return msg.to == sk;
        }
        true
    }

    fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.queue.1.notify_all();
    }

    fn drain(&self) -> Vec<AgentMessage> {
        let mut q = self.queue.0.lock().unwrap();
        let all: Vec<AgentMessage> = q.drain(..).collect();
        all.into_iter().filter(|m| self.accepts(m)).collect()
    }

    fn wait_for(&self, timeout: Duration, filter_to: Option<&str>) -> Vec<AgentMessage> {
        let deadline = Instant::now() + timeout;
        let mut q = self.queue.0.lock().unwrap();
        loop {
            let mut matched = Vec::new();
            let mut keep = VecDeque::new();
            while let Some(m) = q.pop_front() {
                if !self.accepts(&m) {
                    continue;
                }
                let hit = match filter_to {
                    Some(t) => m.to == t || m.to.starts_with(t),
                    None => true,
                };
                if hit {
                    matched.push(m);
                } else {
                    keep.push_back(m);
                }
            }
            *q = keep;
            if !matched.is_empty() || self.shutdown.load(Ordering::Relaxed) {
                return matched;
            }
            let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                return Vec::new();
            };
            let (guard, res) = self.queue.1.wait_timeout(q, rem).unwrap();
            q = guard;
            if res.timed_out() && q.is_empty() {
                return Vec::new();
            }
        }
    }
}

fn write_json_line(out: &Arc<Mutex<std::io::Stdout>>, v: &Value) -> std::io::Result<()> {
    let mut g = out.lock().unwrap();
    g.write_all(v.to_string().as_bytes())?;
    g.write_all(b"\n")?;
    g.flush()
}

fn watchdog_loop(
    url: String,
    token: Option<String>,
    cwd: Option<String>,
    wd: Arc<Watchdog>,
    stdout: Arc<Mutex<std::io::Stdout>>,
) {
    while !wd.shutdown.load(Ordering::Relaxed) {
        let _ = watchdog_stream_once(&url, token.as_deref(), cwd.as_deref(), &wd, &stdout);
        // Gentle backoff when daemon is restarting or offline.
        for _ in 0..20 {
            if wd.shutdown.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn watchdog_stream_once(
    url: &str,
    token: Option<&str>,
    cwd: Option<&str>,
    wd: &Watchdog,
    stdout: &Arc<Mutex<std::io::Stdout>>,
) -> Result<()> {
    let rest = url.strip_prefix("http://").ok_or_else(|| anyhow!("non-http url"))?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse::<u16>()?),
        None => (hostport.to_owned(), 80),
    };
    let path = match wd.self_key() {
        Some(sk) => format!("/v1/messages/stream?to={}", enc(&sk)),
        None => match cwd {
            Some(c) if !c.is_empty() && c != "/" => format!("/v1/messages/stream?cwd={}", enc(c)),
            _ => "/v1/messages/stream".to_owned(),
        },
    };
    let sock = std::net::ToSocketAddrs::to_socket_addrs(&format!("{host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow!("resolve failed"))?;
    let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(2))?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Accept: text/event-stream\r\n\r\n")?;
    s.flush()?;
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if !line.contains(" 200 ") {
        return Err(anyhow!("stream status: {}", line.trim()));
    }
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    loop {
        if wd.shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                if let Some(data) = line.trim_end().strip_prefix("data:")
                    && let Ok(msg) = serde_json::from_str::<AgentMessage>(data.trim_start())
                {
                    if !wd.accepts(&msg) {
                        continue;
                    }
                    {
                        let mut q = wd.queue.0.lock().unwrap();
                        if q.len() >= 100 {
                            q.pop_front();
                        }
                        if !q.iter().any(|x| x.id == msg.id) {
                            q.push_back(msg.clone());
                        }
                        wd.queue.1.notify_all();
                    }
                    // Notify MCP client immediately once handshake is initialized.
                    if wd.initialized.load(Ordering::Relaxed) {
                        let note = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/message",
                            "params": {
                                "level": "info",
                                "logger": "uniflo",
                                "data": {
                                    "id": msg.id,
                                    "from": msg.from,
                                    "to": msg.to,
                                    "text": msg.text,
                                    "kind": msg.kind,
                                    "reply_to": msg.reply_to,
                                }
                            }
                        });
                        let _ = write_json_line(stdout, &note);
                        if wd.subscribed.load(Ordering::Relaxed) {
                            let res_note = json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/resources/updated",
                                "params": { "uri": "uniflo://inbox" }
                            });
                            let _ = write_json_line(stdout, &res_note);
                        }
                    }
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// `idle`: a fallback index unused this long is dropped. Agents keep their MCP servers for days,
/// and the index holds every session (and the usage ledgers, the full-text reader) the daemon
/// also has; rebuilding it takes seconds, so the window spans an agent's pauses between calls.
pub fn run(cli: &Cli, idle: Duration, message: bool) -> Result<()> {
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let watchdog = (message && !cli.local).then(|| Watchdog::start(cli.url.clone(), cli.token.clone(), stdout.clone()));
    let mut server =
        Server::new_with_watchdog(cli.url.clone(), cli.token.clone(), cli.local, message, watchdog.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        use std::sync::mpsc::RecvTimeoutError;
        // Only a fallback index has anything to release; otherwise just wait for the next request.
        let next = if server.holds_fallback() {
            rx.recv_timeout(idle)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let line = match next {
            Ok(line) => line?,
            Err(RecvTimeoutError::Timeout) => {
                server.release_idle();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            write_json_line(&stdout, &reply)?;
        }
    }
    if let Some(wd) = watchdog {
        wd.stop();
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
    message: bool,
    watchdog: Option<Arc<Watchdog>>,
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
    #[cfg(test)]
    fn new(url: String, token: Option<String>, local_only: bool, message: bool) -> Server {
        Server::new_with_watchdog(url, token, local_only, message, None)
    }

    fn new_with_watchdog(
        url: String,
        token: Option<String>,
        local_only: bool,
        message: bool,
        watchdog: Option<Arc<Watchdog>>,
    ) -> Server {
        Server { url, token, local_only, message, watchdog, local: None, noted: false }
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
            "initialize" => {
                if let Some(wd) = &self.watchdog {
                    wd.initialized.store(true, Ordering::Relaxed);
                }
                Ok(initialize(&params, self.message))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools(self.message) })),
            "tools/call" => self.call(&params),
            "resources/list" if self.message => Ok(json!({
                "resources": [{
                    "uri": "uniflo://inbox",
                    "name": "Uniflo Agent Inbox",
                    "description": "Live unread cross-agent messages for this session's working directory",
                    "mimeType": "text/plain"
                }]
            })),
            "resources/read" if self.message => self.read_resource(&params),
            "resources/subscribe" if self.message => {
                if let Some(wd) = &self.watchdog {
                    wd.subscribed.store(true, Ordering::Relaxed);
                }
                Ok(json!({}))
            }
            "resources/unsubscribe" if self.message => {
                if let Some(wd) = &self.watchdog {
                    wd.subscribed.store(false, Ordering::Relaxed);
                }
                Ok(json!({}))
            }
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
        let structured = a.bool("structured", false)?;
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
            "uniflo_peers" if self.message => self.peers(&a)?,
            "uniflo_send_message" if self.message => self.send_message(&a)?,
            "uniflo_inbox" if self.message => self.inbox(&a)?,
            "uniflo_ack_messages" if self.message => self.ack_messages(&a)?,
            "uniflo_wait_message" if self.message => self.wait_message(&a)?,
            _ => return Err(invalid(format!("unknown tool: {name}"))),
        };
        if self.local.is_some() && !self.noted {
            self.noted = true;
            out.text = format!("{LOCAL_NOTE}\n\n{}", out.text);
        }
        // Piggyback pending SSE watchdog messages onto read-only tool replies ONLY when we know
        // our exact session key (preventing same-cwd peers in `~` from stealing each other's
        // `delivered_at` flag!).
        if self.message
            && !matches!(
                name,
                "uniflo_peers" | "uniflo_send_message" | "uniflo_inbox" | "uniflo_ack_messages" | "uniflo_wait_message"
            )
            && let Some(wd) = &self.watchdog
            && let Some(sk) = wd.self_key()
        {
            let pending: Vec<AgentMessage> = wd.drain().into_iter().filter(|m| m.to == sk).collect();
            if !pending.is_empty() {
                let ids: Vec<String> = pending.iter().map(|m| m.id.clone()).collect();
                let _ = self.daemon_write("/v1/messages/ack", json!({ "ids": ids, "state": "delivered" }));
                out.text.push_str("\n\n<uniflo-messages>");
                for m in &pending {
                    let reply = m.reply_to.as_deref().map(|r| format!(" reply_to={r}")).unwrap_or_default();
                    let _ = write!(out.text, "\n[{} from={}{reply}] {}", m.id, m.from, m.text);
                }
                out.text.push_str("\n</uniflo-messages>");
            }
        }
        let mut result = json!({
            "content": [{ "type": "text", "text": out.text }],
            "isError": out.is_error,
        });
        if structured {
            result["structuredContent"] = out.structured;
        }
        Ok(result)
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
        if !self.local_only {
            let c = Client::new(&self.url, self.token.clone())?;
            if let Ok(mut r) = c.get(path) {
                let status = r.status;
                let body = r.body()?;
                self.drop_fallback();
                return Ok((status, body));
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

    /// No request for the idle window: drop the fallback index and give its memory back. The next
    /// request asks the daemon first and rebuilds the index (from the cache) only without one.
    fn release_idle(&mut self) {
        if self.holds_fallback() {
            self.drop_fallback();
        }
    }

    /// Back to the daemon (or idle): drop the fallback index and give its memory back.
    fn drop_fallback(&mut self) {
        self.noted = false;
        if self.local.take().is_some() {
            uniflo_core::util::trim_heap();
        }
    }

    fn holds_fallback(&self) -> bool {
        !self.local_only && self.local.is_some()
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
                f.release_writer_when_idle();
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
        if !self.local_only {
            let alive = Client::new(&self.url, self.token.clone()).is_ok_and(|c| c.alive());
            if alive {
                self.drop_fallback();
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
                let mut text = session_rows(&list);
                if list.is_empty() {
                    text.push_str("\n(no match: q words fuzzy-match title / first prompt / cwd; try fewer words or a wider since:)");
                }
                if list.len() as i64 == limit {
                    let _ = write!(text, "\n(first {limit}: narrow q or raise limit for more)");
                }
                Out::ok(text, json!({ "sessions": list }))
            }
            Err(e) => Out::error(e),
        })
    }

    fn session(&mut self, a: &Args) -> Result<Out, RpcError> {
        let (key, frag) = parse_ref(a.req_str("key")?);
        let around = a.str("around")?.map(|s| s.trim_start_matches('#').to_owned()).or(frag);
        let limit = a.int("limit", 1, 200, 60)?;
        let before = a.opt_int("before", 0, i64::MAX)?;
        // A caller centring on one event is usually reading that one event, not the whole page:
        // default smaller unless they asked for a specific size.
        let max_text = a.opt_int("max_text", 0, 1_000_000)?.unwrap_or(if around.is_some() { 600 } else { 4000 });
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
        let mut text = session_header(&sess);
        let mut lines = EventLines::default();
        for e in &events {
            if let Some(l) = lines.line(e) {
                text.push('\n');
                text.push_str(&l);
            }
        }
        if let Some(n) = lines.note() {
            text.push('\n');
            text.push_str(&n);
        }
        let hidden: Vec<String> =
            [(omitted_tools, "tool", "include_tools"), (omitted_reasoning, "reasoning", "include_reasoning")]
                .iter()
                .filter(|(k, ..)| *k > 0)
                .map(|(k, what, flag)| format!("{k} {what} events ({flag}: true)"))
                .collect();
        if !hidden.is_empty() {
            let _ = write!(text, "\nhidden: {}", hidden.join(", "));
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
                let hits = self.get(&format!("/v1/sessions?limit=6&q={}", enc(&format!("id:{key}"))))?;
                match hits.as_array().map(Vec::as_slice) {
                    Some([one]) => Ok(one.clone()),
                    Some(many @ [_, _, ..]) => {
                        let keys: Vec<&str> = many.iter().take(5).filter_map(|s| s["key"].as_str()).collect();
                        let more = if many.len() > 5 { ", …" } else { "" };
                        Err(format!("{key} is ambiguous; pass a full key: {}{more}", keys.join(", ")))
                    }
                    _ => Err(e),
                }
            }
        }
    }

    fn search(&mut self, a: &Args) -> Result<Out, RpcError> {
        let q = a.req_str("q")?.to_owned();
        let filter = a.str("filter")?.map(str::to_owned);
        let limit = a.int("limit", 1, 30, 10)?;
        let offset = a.int("offset", 0, i64::MAX, 0)?;
        let context = a.int("context", 0, 3, 0)?;
        self.prepare(false, true);
        let mut path = format!("/v1/search?q={}&limit={limit}&offset={offset}", enc(&q));
        if let Some(f) = &filter {
            path.push_str(&format!("&filter={}", enc(f)));
        }
        Ok(match self.get(&path) {
            Ok(v) => {
                let results = v["results"].as_array().cloned().unwrap_or_default();
                let total = v["total"].as_u64().unwrap_or(0);
                // A short-term scan stops early, so its total is only a lower bound.
                let plus = if v["scanned_until"].is_null() { "" } else { "+" };
                let mut text =
                    format!("{total}{plus} sessions match · key|title|updated, then #event [kind age] snippet");
                if total == 0 {
                    text.push_str(
                        " (none: terms are ANDed case-insensitive substrings; try fewer terms or drop filter)",
                    );
                }
                if v["order"] == "recent" {
                    text.push_str(" (a term under 3 characters: substring scan, newest first, unranked)");
                }
                if v["partial"] == true {
                    text.push_str(" (scan hit its time budget: lengthen short terms for complete results)");
                }
                if v["indexing"] == true {
                    let _ = write!(
                        text,
                        " (index building, {}/{} sessions: results may be incomplete)",
                        v["progress"]["done"], v["progress"]["total"]
                    );
                }
                let mut cwd = None;
                for (i, s) in results.iter().enumerate() {
                    let key = s["session"].as_str().unwrap_or("");
                    if s["cwd"].as_str() != cwd && s["cwd"].is_string() {
                        cwd = s["cwd"].as_str();
                        let _ = write!(text, "\n@ {}", crate::render::short_path(cwd.unwrap_or_default()));
                    }
                    let _ = write!(
                        text,
                        "\n- {key}|{}|{}",
                        cell(&preview(s["title"].as_str().unwrap_or("(untitled)"), 80)),
                        age(s["updated_at"].as_i64().unwrap_or(0))
                    );
                    for (hi, h) in s["hits"].as_array().into_iter().flatten().enumerate() {
                        let snippet =
                            h["snippet"].as_str().unwrap_or("").replace([HIGHLIGHT_START, HIGHLIGHT_END], "**");
                        let event = h["event"].as_str().unwrap_or("");
                        let _ = write!(
                            text,
                            "\n  #{event} [{} {}] {snippet}",
                            h["kind"].as_str().unwrap_or(""),
                            age(h["ts"].as_i64().unwrap_or(0))
                        );
                        // Only the best hit of the first few sessions: enough to answer "find and
                        // read it" in one call without ballooning the reply for every session.
                        if hi == 0 && context > 0 && i < 3 {
                            let evpath = format!(
                                "/v1/sessions/{}/events?around={}&limit={}&max_text=400",
                                enc(key),
                                enc(event),
                                2 * context + 1
                            );
                            if let Ok(page) = self.get(&evpath) {
                                let mut lines = EventLines::default();
                                for e in page["events"].as_array().into_iter().flatten() {
                                    if let Some(l) = lines.line(e) {
                                        let _ = write!(text, "\n    {l}");
                                    }
                                }
                            }
                        }
                    }
                }
                let next = v["offset"].as_u64().unwrap_or(0) + results.len() as u64;
                if next < total {
                    let _ = write!(text, "\nmore: offset={next}");
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
                for (k, name) in [("since", "since"), ("until", "until")] {
                    if let Some(t) = v[k].as_i64() {
                        let _ = write!(text, " · {name} {}", when(t));
                    }
                }
                if let Some(u) = v["under"].as_str() {
                    let _ = write!(text, " · under {}", crate::render::short_path(u));
                }
                if let Some(tz) = v["tz"].as_str() {
                    let _ = write!(text, " · tz {tz}");
                }
                if v["pricing"]["stale"] == true {
                    let fetched = v["pricing"]["fetched_at"].as_i64().map(when);
                    let _ = write!(text, " · prices stale (fetched {})", fetched.as_deref().unwrap_or("never"));
                }
                let _ = write!(text, " · label|{USAGE_COLS}");
                for r in v["rows"].as_array().into_iter().flatten() {
                    let label = r["label"].as_str().unwrap_or("");
                    // The key is what a follow-up filter takes (a full cwd, a catalog id).
                    let label = match r["key"].as_str() {
                        Some(k) if k != label && !k.is_empty() => format!("{label} ({})", crate::render::short_path(k)),
                        _ => label.to_owned(),
                    };
                    let _ = write!(text, "\n{}|{}", cell(&label), usage_cells(r));
                }
                let _ = write!(text, "\ntotal|{}", usage_cells(&v["totals"]));
                if v["indexing"]["ready"] == false {
                    let _ = write!(
                        text,
                        "\n(usage index still building, {}/{} sessions: totals will grow)",
                        v["indexing"]["done"], v["indexing"]["total"]
                    );
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
                let mut text = format!("{key}: {}", usage_labelled(&v["totals"]));
                text.push_str(&step_summary(v["steps"].as_array().map(Vec::as_slice).unwrap_or_default()));
                let turns = v["turns"].as_array().cloned().unwrap_or_default();
                let skip = turns.len().saturating_sub(TURN_LINES);
                if skip > 0 {
                    let _ = write!(text, "\n({skip} earlier turns not listed: structured=true for all)");
                }
                if !turns.is_empty() {
                    let _ = write!(text, "\nturns · turn|started|{TURN_COLS}");
                }
                for t in &turns[skip..] {
                    let start = t["started_at"].as_i64().map(when).unwrap_or_default();
                    let _ = write!(text, "\n{}|{start}|{}", t["turn"], usage_cells(t));
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
                let mut text = format!("{} models · {MODEL_COLS}", list.len());
                for m in &list {
                    text.push('\n');
                    text.push_str(&model_row(m));
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
                // The agent runs on this machine: give the command for its shell.
                let command = if cfg!(windows) { v["command_powershell"].as_str() } else { None };
                let text = if v["supported"] == true {
                    format!(
                        "Run in a terminal (not executed by uniflo):\n{}",
                        command.or(v["command"].as_str()).unwrap_or("")
                    )
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
                        "\n- {} · {} · {} · {} bytes · updated {}",
                        f["path"].as_str().unwrap_or(""),
                        f["scope"].as_str().unwrap_or(""),
                        f["harness"].as_str().unwrap_or(""),
                        f["bytes"],
                        age(f["updated_at"].as_i64().unwrap_or(0))
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
            "uniflo {} ({source}) · schema v{} · up {} · {} sessions, {} working",
            health["version"].as_str().unwrap_or("?"),
            health["schema"],
            age(now_ms() - health["uptime_ms"].as_i64().unwrap_or(0)),
            health["sessions"],
            health["working"]
        );
        if health["read_only"] == true {
            text.push_str(" · read-only");
        }
        if health["update_available"] == true {
            let _ = write!(text, " · update available: {}", health["latest_version"].as_str().unwrap_or("?"));
        }
        let list = harnesses.as_array().map(Vec::as_slice).unwrap_or_default();
        let roots = |h: &Value| {
            let r: Vec<String> = h["roots"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(crate::render::short_path)
                .collect();
            r.join(", ")
        };
        for h in list.iter().filter(|h| h["sessions"].as_u64() > Some(0)) {
            let _ = write!(
                text,
                "\n- {}: {} sessions, {} working · {}",
                h["id"].as_str().unwrap_or(""),
                h["sessions"],
                h["working"],
                roots(h)
            );
        }
        let empty: Vec<String> = list
            .iter()
            .filter(|h| h["sessions"].as_u64() == Some(0))
            .map(|h| match roots(h) {
                r if r.is_empty() => h["id"].as_str().unwrap_or("").to_owned(),
                r => format!("{} ({r})", h["id"].as_str().unwrap_or("")),
            })
            .collect();
        if !empty.is_empty() {
            let _ = write!(text, "\nno sessions: {}", empty.join(", "));
        }
        Ok(Out::ok(text, json!({ "health": health, "harnesses": harnesses })))
    }

    fn daemon_write(&self, path: &str, body: Value) -> Result<Value, String> {
        self.daemon_write_timeout(path, body, Duration::from_secs(10))
    }

    fn daemon_write_timeout(&self, path: &str, body: Value, timeout: Duration) -> Result<Value, String> {
        if self.local_only {
            return Err("cross-agent messaging requires a running uniflo daemon (not --local)".into());
        }
        let c = Client::new(&self.url, self.token.clone()).map_err(|e| format!("{e:#}"))?;
        c.write_json_timeout::<Value>("POST", path, Some(&body), timeout)
            .map_err(|e| format!("{e:#} (is `uniflo daemon` running at {}?)", self.url))
    }

    fn daemon_get(&self, path: &str) -> Result<Value, String> {
        if self.local_only {
            return Err("cross-agent messaging requires a running uniflo daemon (not --local)".into());
        }
        let c = Client::new(&self.url, self.token.clone()).map_err(|e| format!("{e:#}"))?;
        c.get_json::<Value>(path).map_err(|e| format!("{e:#} (is `uniflo daemon` running at {}?)", self.url))
    }

    /// Resolve this MCP server's own owning session key (env first, then daemon PID ancestry lookup).
    fn resolve_self_key(&self) -> Option<String> {
        if let Some(sk) = self.watchdog.as_ref().and_then(|w| w.self_key()).or_else(env_self_key) {
            return Some(sk);
        }
        let pid = std::process::id();
        if let Ok(v) = self.daemon_get(&format!("/v1/messages/peers?pid={pid}&limit=1"))
            && let Some(sk) = v["self_key"].as_str().filter(|s| !s.is_empty())
        {
            if let Some(wd) = &self.watchdog {
                wd.set_self_key(sk.to_owned());
            }
            return Some(sk.to_owned());
        }
        None
    }

    fn peers(&mut self, a: &Args) -> Result<Out, RpcError> {
        let limit = a.int("limit", 1, 100, 20)?;
        let pid = std::process::id();
        let mut qs = vec![format!("pid={pid}"), format!("limit={limit}")];
        if let Some(sk) = self.watchdog.as_ref().and_then(|w| w.self_key()).or_else(env_self_key) {
            qs.push(format!("self={}", enc(&sk)));
        }
        Ok(match self.daemon_get(&format!("/v1/messages/peers?{}", qs.join("&"))) {
            Ok(v) => {
                if let Some(sk) = v["self_key"].as_str().filter(|s| !s.is_empty())
                    && let Some(wd) = &self.watchdog
                {
                    wd.set_self_key(sk.to_owned());
                }
                let list = v["peers"].as_array().cloned().unwrap_or_default();
                let mut text = String::new();
                if let Some(sk) = v["self_key"].as_str().filter(|s| !s.is_empty()) {
                    let _ = writeln!(text, "self: {sk}");
                }
                let _ = write!(text, "{} reachable peers · key|harness|title|status|channel|cwd|unread", list.len());
                for p in &list {
                    let key = p["key"].as_str().unwrap_or("");
                    let harness = p["harness"].as_str().unwrap_or("");
                    let title = p["title"].as_str().unwrap_or("(untitled)");
                    let status = p["status"].as_str().unwrap_or("?");
                    let ch = match (p["channel"].as_str(), p["terminal"].as_str()) {
                        (Some(c), Some(t)) => format!("{c}:{t}"),
                        (Some(c), None) => c.to_owned(),
                        _ => "inbox".to_owned(),
                    };
                    let cwd = p["cwd"].as_str().map(crate::render::short_path).unwrap_or_else(|| "-".into());
                    let unread = p["unread"].as_u64().unwrap_or(0);
                    let _ =
                        write!(text, "\n- {key}|{harness}|{}|{status}|{ch}|{cwd}|{unread}", cell(&preview(title, 56)));
                }
                if !list.is_empty() {
                    text.push_str("\nTip: call uniflo_send_message(to=<key>, text=..., wait_secs=45) to send, wake without hooks, and get the peer's reply in one call.");
                }
                Out::ok(text, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn send_message(&mut self, a: &Args) -> Result<Out, RpcError> {
        let to = a.req_str("to")?;
        let text = a.req_str("text")?;
        let auto_from = a.str("from")?.map(str::to_owned).or_else(|| self.resolve_self_key());
        let kind = a.str("kind")?;
        let reply_to = a.str("reply_to")?;
        let wake = a.bool("wake", true)?;
        let deliver =
            a.str("deliver")?.map(str::to_owned).unwrap_or_else(|| if wake { "auto".into() } else { "inbox".into() });
        let wait_secs = a.opt_int("wait_secs", 1, 90)?.map(|n| n as u64);
        let hop = a.opt_int("hop", 0, 10)?.map(|n| n as u8).or_else(env_wake_hop);

        let mut body = json!({
            "to": to,
            "from": auto_from,
            "text": text,
            "kind": kind,
            "reply_to": reply_to,
            "deliver": deliver,
        });
        if let Some(w) = wait_secs {
            body["wait_secs"] = json!(w);
        }
        if let Some(h) = hop {
            body["hop"] = json!(h);
        }
        let timeout = Duration::from_secs(wait_secs.map_or(15, |w| w.saturating_add(20)));
        Ok(match self.daemon_write_timeout("/v1/messages", body, timeout) {
            Ok(v) => {
                let id = v["message"]["id"].as_str().unwrap_or("");
                if let Some(wd) = &self.watchdog {
                    wd.remember_sent(id);
                }
                let target = v["target"]["key"].as_str().unwrap_or(to);
                let status = v["target"]["status"].as_str().unwrap_or("?");
                let channel =
                    v["delivery_channel"].as_str().or_else(|| v["target"]["delivery"].as_str()).unwrap_or("inbox");
                let mut summary = format!("sent {id} -> {target} ({status}, channel={channel})");
                if let Some(detail) = v["delivery_detail"].as_str().filter(|s| !s.is_empty()) {
                    let _ = write!(summary, "\n{detail}");
                }
                if let Some(reply_text) = v["reply"]["text"].as_str().filter(|s| !s.is_empty()) {
                    let rid = v["reply"]["id"].as_str().unwrap_or("");
                    let _ = write!(summary, "\nreply [{rid}] from {target}:\n{reply_text}");
                } else if channel == "inbox"
                    && let Some(cmd) = v["target"]["resume_command"].as_str()
                {
                    let _ = write!(summary, "\nresume command (not executed): {cmd}");
                }
                Out::ok(summary, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn inbox(&mut self, a: &Args) -> Result<Out, RpcError> {
        let to_arg = a.str("to")?.map(str::to_owned);
        let cwd = a.str("cwd")?;
        let from = a.str("from")?;
        let to = if to_arg.is_none() && cwd.is_none() && from.is_none() { self.resolve_self_key() } else { to_arg };
        let unread = a.bool("unread", true)?;
        let undelivered = a.bool("undelivered", false)?;
        let limit = a.int("limit", 1, 200, 50)?;
        let mut qs = vec![format!("limit={limit}")];
        if let Some(ref t) = to {
            qs.push(format!("to={}", enc(t)));
        }
        if let Some(c) = cwd {
            qs.push(format!("cwd={}", enc(c)));
        }
        if let Some(f) = from {
            qs.push(format!("from={}", enc(f)));
        }
        if unread {
            qs.push("unread=true".into());
        }
        if undelivered {
            qs.push("undelivered=true".into());
        }
        Ok(match self.daemon_get(&format!("/v1/messages?{}", qs.join("&"))) {
            Ok(v) => {
                let list = v["messages"].as_array().cloned().unwrap_or_default();
                let unread_n = v["unread"].as_u64().unwrap_or(0);
                let mut text = format!("{} messages ({unread_n} unread)", list.len());
                for m in &list {
                    let _ = write!(
                        text,
                        "\n- {} {} -> {} ({}): {}",
                        m["id"].as_str().unwrap_or(""),
                        m["from"].as_str().unwrap_or(""),
                        m["to"].as_str().unwrap_or(""),
                        age(m["created_at"].as_i64().unwrap_or(0)),
                        m["text"].as_str().unwrap_or("")
                    );
                }
                Out::ok(text, v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn ack_messages(&mut self, a: &Args) -> Result<Out, RpcError> {
        let to = a.str("to")?;
        let state = a.str("state")?;
        let ids: Vec<String> = match a.0.get("ids") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| invalid("ids items must be strings")))
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(invalid("ids must be an array of strings")),
        };
        if ids.is_empty() && to.is_none() {
            return Err(invalid("provide ids or to"));
        }
        let body = json!({ "ids": ids, "to": to, "state": state });
        Ok(match self.daemon_write("/v1/messages/ack", body) {
            Ok(v) => {
                let n = v["updated"].as_u64().unwrap_or(0);
                Out::ok(format!("updated {n} messages"), v)
            }
            Err(e) => Out::error(e),
        })
    }

    fn wait_message(&mut self, a: &Args) -> Result<Out, RpcError> {
        let to = a.str("to")?.map(str::to_owned).or_else(|| self.resolve_self_key());
        let cwd = a
            .str("cwd")?
            .map(str::to_owned)
            .or_else(|| self.watchdog.as_ref().and_then(|w| w.cwd.clone()))
            .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()));
        let timeout_secs = a.int("timeout_secs", 1, 60, 30)? as u64;
        let ack = a.bool("ack", false)?;

        // Check already-undelivered messages immediately before waiting on the SSE watchdog.
        let mut qs = vec!["undelivered=true".to_owned(), "limit=50".to_owned()];
        if let Some(ref t) = to {
            qs.push(format!("to={}", enc(t)));
        } else if let Some(ref c) = cwd {
            qs.push(format!("cwd={}", enc(c)));
        }
        if let Ok(v) = self.daemon_get(&format!("/v1/messages?{}", qs.join("&"))) {
            let list = v["messages"].as_array().cloned().unwrap_or_default();
            if !list.is_empty() {
                let ids: Vec<String> = list.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect();
                let state = if ack { "acked" } else { "delivered" };
                let _ = self.daemon_write("/v1/messages/ack", json!({ "ids": ids, "state": state }));
                let mut text = format!("{} messages arrived", list.len());
                for m in &list {
                    let _ = write!(
                        text,
                        "\n- {} {} -> {}: {}",
                        m["id"].as_str().unwrap_or(""),
                        m["from"].as_str().unwrap_or(""),
                        m["to"].as_str().unwrap_or(""),
                        m["text"].as_str().unwrap_or("")
                    );
                }
                return Ok(Out::ok(text, v));
            }
        }

        // Wait on the per-process SSE watchdog (wakes in milliseconds when a peer sends a message).
        let Some(wd) = self.watchdog.clone() else {
            return Ok(Out::ok(
                "no new messages (watchdog inactive in --local mode)".into(),
                json!({ "messages": [] }),
            ));
        };
        let arrived = wd.wait_for(Duration::from_secs(timeout_secs), to.as_deref());
        if arrived.is_empty() {
            return Ok(Out::ok(format!("no new messages within {timeout_secs}s"), json!({ "messages": [] })));
        }
        let ids: Vec<String> = arrived.iter().map(|m| m.id.clone()).collect();
        let state = if ack { "acked" } else { "delivered" };
        let _ = self.daemon_write("/v1/messages/ack", json!({ "ids": ids, "state": state }));
        let mut text = format!("{} messages arrived", arrived.len());
        for m in &arrived {
            let _ = write!(text, "\n- {} {} -> {}: {}", m.id, m.from, m.to, m.text);
        }
        Ok(Out::ok(text, json!({ "messages": arrived })))
    }

    fn read_resource(&mut self, params: &Value) -> Result<Value, RpcError> {
        let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| invalid("missing uri"))?;
        if uri != "uniflo://inbox" {
            return Err(invalid(format!("unknown resource: {uri}")));
        }
        let cwd = self
            .watchdog
            .as_ref()
            .and_then(|w| w.cwd.clone())
            .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()));
        let path = match cwd {
            Some(c) => format!("/v1/messages?unread=true&limit=50&cwd={}", enc(&c)),
            None => "/v1/messages?unread=true&limit=50".to_owned(),
        };
        let text = match self.daemon_get(&path) {
            Ok(v) => {
                let list = v["messages"].as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    "inbox empty".to_owned()
                } else {
                    let mut buf = format!("{} unread messages", list.len());
                    for m in &list {
                        let _ = write!(
                            buf,
                            "\n[{} from={}] {}",
                            m["id"].as_str().unwrap_or(""),
                            m["from"].as_str().unwrap_or(""),
                            m["text"].as_str().unwrap_or("")
                        );
                    }
                    buf
                }
            }
            Err(e) => e,
        };
        Ok(json!({ "contents": [{ "uri": uri, "mimeType": "text/plain", "text": text }] }))
    }
}

fn error_reply(id: Value, code: i64, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

fn initialize(params: &Value, message: bool) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
    let mut instructions = "Read-only access to every local agent-harness session (Claude Code, Codex, omp, OpenCode, Gemini CLI, …). \
        Find sessions with uniflo_sessions (search syntax: h:claude s:work in:<cwd> since:2d 'fuzzy), read one with uniflo_session, \
        search transcript text with uniflo_search, costs with uniflo_usage. Session keys look like claude:<id>; \
        open a #<event-id> from search with uniflo_session (key, around). Answers are compact text: a legend names \
        the |-separated columns, an `@ <dir>` line is the cwd of the rows below it, `↳` rows are subagents of the row above, \
        counts past 10k keep 3 significant digits and `-` means unknown; structured:true adds the exact JSON."
        .to_owned();
    if message {
        instructions.push_str(
            " Cross-agent messaging (zero-hook active delivery + SSE watchdog) is enabled: \
             call uniflo_peers first to see your own session key (`self`) and reachable peers with their delivery channels \
             (herdr/tmux live terminal injection or official headless resume), then call uniflo_send_message(to=<key>, text=..., wait_secs=45) \
             to deliver, wake an idle peer automatically without hooks, and receive their reply inline in one call. \
             Use uniflo_inbox, uniflo_wait_message, or uniflo_ack_messages to manage messages addressed to you.",
        );
    }
    let capabilities = if message {
        json!({
            "tools": { "listChanged": false },
            "resources": { "subscribe": true, "listChanged": false },
            "logging": {}
        })
    } else {
        json!({ "tools": { "listChanged": false } })
    };
    json!({
        "protocolVersion": version,
        "capabilities": capabilities,
        "serverInfo": { "name": "uniflo", "title": "Uniflo", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions,
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

/// Rows of sessions under a one-line legend. Subagents whose parent is listed are nested under
/// it (`↳`, with `@ <dir>` appended when their cwd differs); the cwd of top-level rows is printed
/// as an `@ <dir>` line only when it changes.
fn session_rows(list: &[Value]) -> String {
    let mut text = format!("{} sessions, newest first · key|title|model|status|updated|cost|steps", list.len());
    // Only one level is nested: a subagent of a nested row stays top-level with `sub of`, so
    // every row is printed exactly once.
    let listed = |k: &str| list.iter().find(|s| s["key"].as_str() == Some(k));
    let top_level = |s: &Value| s["parent"].as_str().and_then(listed).is_none();
    let nested = |s: &Value| s["parent"].as_str().and_then(listed).is_some_and(top_level);
    let mut cwd = None::<&str>;
    for top in list.iter().filter(|s| !nested(s)) {
        let here = top["cwd"].as_str();
        if here.is_some() && here != cwd {
            let _ = write!(text, "\n@ {}", crate::render::short_path(here.unwrap_or_default()));
            cwd = here;
        }
        text.push_str("\n- ");
        text.push_str(&session_row(top, true));
        for sub in list.iter().filter(|s| nested(s) && s["parent"] == top["key"]) {
            text.push_str("\n  ↳ ");
            text.push_str(&session_row(sub, false));
            if let Some(c) = sub["cwd"].as_str().filter(|c| Some(*c) != here) {
                let _ = write!(text, " @ {}", crate::render::short_path(c));
            }
        }
    }
    text
}

/// One `key|title|model|status|updated|cost|steps` row; `parent` adds `sub of <key>` to the
/// status of a subagent shown on its own.
fn session_row(s: &Value, parent: bool) -> String {
    let mut status = status_cell(s);
    if parent && let Some(p) = s["parent"].as_str() {
        let _ = write!(status, " sub of {p}");
    }
    let label = s["title"].as_str().or_else(|| s["preview"].as_str()).unwrap_or("(untitled)");
    let u = &s["usage"];
    format!(
        "{}|{}|{}|{status}|{}|{}|{}",
        s["key"].as_str().unwrap_or(""),
        cell(&preview(label, 80)),
        s["model"].as_str().unwrap_or("-"),
        age(s["updated_at"].as_i64().unwrap_or(0)),
        usd(&u["cost_usd"]),
        n(&u["steps"]),
    )
}

/// `idle`, `idle archived`, `work 3m tool_call ctx 169k/1M 16%`.
fn status_cell(s: &Value) -> String {
    let status = s["status"].as_str().unwrap_or("?");
    let mut c = status.to_owned();
    if status == "work" {
        if let Some(t) = s["status_since"].as_i64() {
            let _ = write!(c, " {}", age(t));
        }
        if let Some(r) = s["status_reason"].as_str() {
            let _ = write!(c, " {r}");
        }
        if let Some(x) = context(&s["usage"], "last_context_tokens") {
            let _ = write!(c, " ctx {x}");
        }
    }
    if s["archived"] == true {
        c.push_str(" archived");
    }
    c
}

/// Everything about the one session `uniflo_session` opens, in three or four labelled lines.
fn session_header(s: &Value) -> String {
    let date = |k: &str| s[k].as_i64().map(when).unwrap_or_else(|| "?".into());
    let label = s["title"].as_str().or_else(|| s["preview"].as_str()).unwrap_or("(untitled)");
    let mut text = format!("{} · {}", s["key"].as_str().unwrap_or(""), preview(label, 120));
    let _ = write!(text, "\n{}", s["model"].as_str().unwrap_or("model unknown"));
    let _ = write!(text, " · {} since {}", s["status"].as_str().unwrap_or("?"), date("status_since"));
    if let Some(r) = s["status_reason"].as_str() {
        let _ = write!(text, " ({r})");
    }
    if s["archived"] == true {
        text.push_str(" · archived");
    }
    if let Some(p) = s["parent"].as_str() {
        let _ = write!(text, " · sub of {p}");
    }
    if let Some(c) = s["cwd"].as_str() {
        let _ = write!(text, " · {}", crate::render::short_path(c));
    }
    let _ = write!(text, " · started {} · updated {}", date("started_at"), date("updated_at"));
    if let Some(p) = s["pid"].as_i64() {
        let _ = write!(text, " · pid {p}");
    }
    if let Some(src) = s["source"].as_str() {
        let _ = write!(text, " · source {}", crate::render::short_path(src));
    }
    if s["usage"].is_object() {
        let _ = write!(text, "\nusage: {}", usage_labelled(&s["usage"]));
    } else {
        text.push_str("\nusage: not loaded here (uniflo_session_usage)");
    }
    if s["title"].is_string()
        && let Some(p) = s["preview"].as_str()
    {
        let _ = write!(text, "\nfirst prompt: {}", preview(p, 300));
    }
    text
}

/// Renders a page of events one line each. The date is printed only when it changes, so a page
/// of one day costs `HH:MM` per event; `#id` is printed where an agent needs it to come back
/// (messages, and any event whose text was cut).
#[derive(Default)]
struct EventLines {
    day: String,
    cut: usize,
}

impl EventLines {
    fn line(&mut self, e: &Value) -> Option<String> {
        let s = |k: &str| e[k].as_str().unwrap_or("").to_owned();
        let call = || e["call_id"].as_str().map(|c| format!(" @{}", tail(c, 4))).unwrap_or_default();
        let (tag, body, anchor) = match e["kind"].as_str()? {
            "user_message" => {
                (if e["synthetic"] == true { "user (injected)".into() } else { "user".into() }, s("text"), true)
            }
            "assistant_message" => ("assistant".to_owned(), s("text"), true),
            "reasoning" => ("reasoning".to_owned(), s("text"), false),
            "tool_call" => (format!("tool_call {}{}", s("name"), call()), preview(&e["input"].to_string(), 600), false),
            "tool_result" => {
                let err = if e["is_error"] == true { " error" } else { "" };
                (format!("tool_result {}{}{err}", s("name"), call()), preview(&s("output"), 600), false)
            }
            "system" => (format!("system {}", s("subtype")), s("text"), false),
            // Only the turn ends a reader would want flagged; normal ends are implied by the
            // next user message.
            "turn_end" => match e["reason"].as_str() {
                Some(r) if ABNORMAL_END.iter().any(|w| r.to_ascii_lowercase().contains(w)) => {
                    (format!("turn_end {r}"), String::new(), false)
                }
                _ => return None,
            },
            _ => return None,
        };
        let full = when(e["ts"].as_i64().unwrap_or(0));
        let time = match full.rsplit_once(' ') {
            Some((day, hm)) if day == self.day => hm.to_owned(),
            Some((day, _)) => {
                self.day = day.to_owned();
                full.clone()
            }
            None => full.clone(),
        };
        let cut = e["truncated"] == true;
        self.cut += usize::from(cut);
        let tag = tag.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut head = format!("[{time} {tag}");
        if anchor || cut {
            let _ = write!(head, " #{}", s("id"));
        }
        if cut {
            head.push_str(" cut");
        }
        head.push(']');
        Some(if body.is_empty() { head } else { format!("{head} {body}") })
    }

    /// Footer for a page that had cut events.
    fn note(&self) -> Option<String> {
        (self.cut > 0).then(|| {
            format!("({} events marked cut were shortened; around=<#id> max_text=0 shows all that is stored)", self.cut)
        })
    }
}

/// Lower-cased fragments of harness turn-end reasons worth showing (`turn_end.reason` is the
/// harness's own string).
const ABNORMAL_END: &[&str] = &["interrupt", "abort", "cancel", "error", "fail", "max_tokens", "length", "refus"];

fn tail(s: &str, n: usize) -> &str {
    let start = s.char_indices().rev().nth(n - 1).map_or(0, |(i, _)| i);
    &s[start..]
}

/// `3m`, `5h`, `2d`: how long ago.
fn age(ms: i64) -> String {
    crate::render::age(ms)
}

/// Local `MM-DD HH:MM`, with the year only when it is not this one.
fn when(ms: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) if t.year() == Local::now().year() => t.format("%m-%d %H:%M").to_string(),
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "?".into(),
    }
}

/// A table cell: one line, no column separator inside.
fn cell(s: &str) -> String {
    s.replace(['|', '\n', '\r'], " ")
}

/// Counts to three significant digits past 10k (`12.3k`, `456k`, `1.23M`, `4.5G`); exact
/// values are one `structured: true` away.
fn num(n: u64) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let (v, unit) = match n {
        n if n >= 1_000_000_000 => (n as f64 / 1e9, "G"),
        n if n >= 1_000_000 => (n as f64 / 1e6, "M"),
        n => (n as f64 / 1e3, "k"),
    };
    let s = match v {
        v if v < 10.0 => format!("{v:.2}"),
        v if v < 100.0 => format!("{v:.1}"),
        v => format!("{v:.0}"),
    };
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { &s };
    format!("{s}{unit}")
}

fn n(v: &Value) -> String {
    v.as_u64().map_or_else(|| "-".to_owned(), num)
}

/// `used/limit pct%` of a usage object's context fields, or the used count alone.
fn context(u: &Value, used: &str) -> Option<String> {
    let c = u[used].as_u64()?;
    Some(match u["context_limit"].as_u64() {
        Some(l) if l > 0 => format!("{}/{} {}%", num(c), num(l), c * 100 / l),
        _ => num(c),
    })
}

/// Columns of [`usage_cells`]; a turn has no `sessions` cell.
const USAGE_COLS: &str = "cost|sessions|prompts|steps|in|out|cache_r|cache_w|reasoning|unpriced";
const TURN_COLS: &str = "cost|prompts|steps|in|out|cache_r|cache_w|reasoning|unpriced";

/// One aggregate (usage row, totals, turn) as `|`-separated cells in [`USAGE_COLS`] order,
/// leaving out the `sessions` cell when the aggregate has none.
fn usage_cells(r: &Value) -> String {
    let cells: Vec<String> =
        ["sessions", "prompts", "steps", "input", "output", "cache_read", "cache_write", "reasoning", "unpriced_steps"]
            .iter()
            .filter(|k| **k != "sessions" || !r[*k].is_null())
            .map(|k| n(&r[*k]))
            .collect();
    format!("{}|{}", usd(&r["cost_usd"]), cells.join("|"))
}

/// One aggregate with labels, for a single line (session totals).
fn usage_labelled(r: &Value) -> String {
    let mut l = usd(&r["cost_usd"]);
    let _ = write!(l, " · {} steps · in {} · out {}", n(&r["steps"]), n(&r["input"]), n(&r["output"]));
    let _ = write!(l, " · cache r/w {}/{}", n(&r["cache_read"]), n(&r["cache_write"]));
    if r["reasoning"].as_u64() > Some(0) {
        let _ = write!(l, " · reasoning {}", n(&r["reasoning"]));
    }
    if let Some(c) = context(r, "last_context_tokens") {
        let _ = write!(l, " · ctx {c}");
    }
    if r["unpriced_steps"].as_u64() > Some(0) {
        let _ = write!(l, " · {} unpriced steps", r["unpriced_steps"]);
    }
    l
}

/// `$34.32`, `$0.11`, `$0.0042`; `-` when unknown (unpriced).
fn usd(v: &Value) -> String {
    match v.as_f64() {
        None => "-".into(),
        Some(0.0) => "$0".into(),
        Some(c) if c >= 1.0 => format!("${c:.2}"),
        Some(c) => {
            let digits = ((-c.log10()).floor().max(0.0) as usize + 2).min(8);
            format!("${c:.digits$}")
        }
    }
}

/// Turn rows `uniflo_session_usage` prints (the most recent ones).
const TURN_LINES: usize = 50;

/// What the per-step list says that the totals do not: models used, peak context, the costliest
/// steps and how costs were priced.
fn step_summary(steps: &[Value]) -> String {
    let mut text = String::new();
    let mut models: Vec<(&str, u64, f64)> = Vec::new();
    for st in steps {
        let m = st["model"].as_str().unwrap_or("(no model)");
        let cost = st["cost_usd"].as_f64().unwrap_or(0.0);
        match models.iter_mut().find(|x| x.0 == m) {
            Some(x) => (x.1, x.2) = (x.1 + 1, x.2 + cost),
            None => models.push((m, 1, cost)),
        }
    }
    if !models.is_empty() {
        models.sort_by_key(|a| std::cmp::Reverse(a.1));
        let list: Vec<String> = models.iter().map(|(m, k, c)| format!("{m} {k} steps {}", usd(&json!(c)))).collect();
        let _ = write!(text, "\nmodels: {}", list.join(", "));
    }
    if let Some(p) = steps.iter().max_by_key(|st| st["context_tokens"].as_u64().unwrap_or(0))
        && let Some(c) = context(p, "context_tokens")
    {
        let _ = write!(text, "\npeak context {c} at turn {} #{}", p["turn"], p["event"].as_str().unwrap_or(""));
    }
    let mut costly: Vec<&Value> = steps.iter().filter(|st| st["cost_usd"].as_f64() > Some(0.0)).collect();
    costly.sort_by(|a, b| {
        b["cost_usd"].as_f64().partial_cmp(&a["cost_usd"].as_f64()).unwrap_or(std::cmp::Ordering::Equal)
    });
    for st in costly.iter().take(3) {
        let _ = write!(
            text,
            "\ncostly step #{} turn {} {} {} in {} out {} cache r/w {}/{}",
            st["event"].as_str().unwrap_or(""),
            st["turn"],
            st["model"].as_str().unwrap_or("-"),
            usd(&st["cost_usd"]),
            n(&st["input"]),
            n(&st["output"]),
            n(&st["cache_read"]),
            n(&st["cache_write"])
        );
    }
    let mut sources: Vec<(&str, usize)> = Vec::new();
    for st in steps {
        let src = st["cost_source"].as_str().unwrap_or("unpriced");
        match sources.iter_mut().find(|x| x.0 == src) {
            Some(x) => x.1 += 1,
            None => sources.push((src, 1)),
        }
    }
    if sources.len() > 1 || sources.iter().any(|(s, _)| *s != "harness" && *s != "catalog") {
        let list: Vec<String> = sources.iter().map(|(s, k)| format!("{s} {k}")).collect();
        let _ = write!(text, "\npriced by: {}", list.join(", "));
    }
    text
}

/// Columns of [`model_row`].
const MODEL_COLS: &str = "model|sessions|steps|cost|unpriced|USD per 1M in/out/cache_r/cache_w|ctx|out";

/// One model: catalog match, use, current prices (and tiers), limits.
fn model_row(m: &Value) -> String {
    let name = m["model"].as_str().unwrap_or("?");
    let mut l = format!("- {name}");
    if let Some(c) = m["catalog_id"].as_str().filter(|c| *c != name) {
        let _ = write!(l, " → {c}");
    }
    if let Some(p) = m["provider"].as_str() {
        let _ = write!(l, " ({p})");
    }
    if let Some(x) = m["match"].as_str().filter(|x| *x != "exact") {
        let _ = write!(l, " {x}");
    }
    let _ = write!(l, "|{}|{}|{}|{}|", n(&m["sessions"]), n(&m["steps"]), usd(&m["cost_usd"]), n(&m["unpriced_steps"]));
    let segs = m["prices"].as_array().map(Vec::as_slice).unwrap_or_default();
    match segs.last() {
        Some(p) => {
            l.push_str(&prices(p));
            for t in p["tiers"].as_array().into_iter().flatten() {
                let _ = write!(l, " >{} {}", n(&t["above"]), prices(t));
            }
            if segs.len() > 1 {
                let _ = write!(l, " (+{} earlier)", segs.len() - 1);
            }
        }
        None => l.push('-'),
    }
    let _ = write!(l, "|{}|{}", n(&m["context_limit"]), n(&m["output_limit"]));
    l
}

/// A price per 1M tokens without trailing zeros: `5`, `0.5`, `6.25`.
fn price(v: &Value) -> String {
    v.as_f64().map_or_else(|| "-".to_owned(), |p| format!("{}", (p * 10_000.0).round() / 10_000.0))
}

/// `in/out/cache read/cache write` of one price (segment or tier).
fn prices(p: &Value) -> String {
    format!("{}/{}/{}/{}", price(&p["input"]), price(&p["output"]), price(&p["cache_read"]), price(&p["cache_write"]))
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

/// Every tool gets a `structured` property besides its own: `props` must be a JSON object.
fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str]) -> Value {
    tool_with(name, title, description, props, required, true, true)
}

fn tool_with(
    name: &str,
    title: &str,
    description: &str,
    mut props: Value,
    required: &[&str],
    read_only: bool,
    idempotent: bool,
) -> Value {
    if let Value::Object(m) = &mut props {
        // Described once in the server instructions rather than on every tool.
        m.insert("structured".into(), json!({ "type": "boolean" }));
    }
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required },
        "annotations": { "readOnlyHint": read_only, "destructiveHint": false, "idempotentHint": idempotent, "openWorldHint": false },
    })
}

fn tools(message: bool) -> Vec<Value> {
    let key = json!({ "type": "string", "description": "Session key (claude:<id>), a unique id prefix, or uniflo://session/<key>#<event-id>" });
    let mut list = vec![
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
                "around": { "type": "string", "description": "Event id (#id from uniflo_search or a page) to centre the window on" },
                "max_text": { "type": "integer", "minimum": 0, "default": 4000, "description": "Cut each text field to this many bytes; 0 = no limit. Defaults to 600 (instead of 4000) when around is set" },
                "include_tools": { "type": "boolean", "default": false },
                "include_reasoning": { "type": "boolean", "default": false },
            }),
            &["key"],
        ),
        tool(
            "uniflo_search",
            "Full-text search",
            "Search message, reasoning and tool text of every session (Chinese words and code substrings); hits grouped by session. Open a hit with uniflo_session (key, around=<event id>).",
            json!({
                "q": { "type": "string", "description": "Terms are ANDed; \"a phrase\"; -excluded" },
                "filter": { "type": "string", "description": "Session filter in the uniflo_sessions q syntax" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 30, "default": 10, "description": "Sessions per page" },
                "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Skip this many matching sessions; page with the more: offset=<n> footer" },
                "context": { "type": "integer", "minimum": 0, "maximum": 3, "default": 0, "description": "For up to the first 3 sessions, inline +/-context neighbour events around the best hit (one extra GET per such session)" },
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
    ];
    if message {
        list.push(tool(
            "uniflo_peers",
            "List reachable peers",
            "Discover your own session key (`self`) and other reachable local agent sessions with their zero-hook delivery channel (herdr, tmux, resume_wake, inbox). Call this first before messaging another agent.",
            json!({
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
            }),
            &[],
        ));
        list.push(tool_with(
            "uniflo_send_message",
            "Send message to another session",
            "Send a message to another local agent session (by key, unique prefix, or cwd:<dir>). By default (`wake: true`), Uniflo actively delivers without hooks via Herdr/tmux live injection or official headless resume; pass `wait_secs` (e.g. 45) to receive the peer's reply inline in one call.",
            json!({
                "to": { "type": "string", "description": "Recipient session key (from uniflo_peers), unique prefix, or cwd:<dir>" },
                "text": { "type": "string", "description": "Message body (max 64 KiB)" },
                "from": { "type": "string", "description": "Optional sender session key (auto-detected from process/env when omitted)" },
                "wake": { "type": "boolean", "default": true, "description": "Actively wake idle peers without hooks (Herdr/tmux live injection -> official headless resume). Set false for inbox-only." },
                "deliver": { "type": "string", "enum": ["auto", "wake", "inbox"], "description": "Explicit delivery mode override (default: auto when wake=true, else inbox)" },
                "wait_secs": { "type": "integer", "minimum": 1, "maximum": 90, "description": "Wait inline up to this many seconds for the peer's reply and return it directly" },
                "kind": { "type": "string", "description": "Optional tag (e.g. task, reply, status)" },
                "reply_to": { "type": "string", "description": "Optional message id being replied to" },
            }),
            &["to", "text"],
            false,
            false,
        ));
        list.push(tool(
            "uniflo_inbox",
            "Read message inbox",
            "List messages addressed to a session or working directory (defaults to unread messages). Same as GET /v1/messages.",
            json!({
                "to": { "type": "string", "description": "Recipient session key, prefix, or cwd:<dir>" },
                "cwd": { "type": "string", "description": "Filter by recipient working directory" },
                "from": { "type": "string", "description": "Filter by sender" },
                "unread": { "type": "boolean", "default": true },
                "undelivered": { "type": "boolean", "default": false },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 },
            }),
            &[],
        ));
        list.push(tool_with(
            "uniflo_ack_messages",
            "Acknowledge messages",
            "Mark messages as acknowledged (or delivered) via POST /v1/messages/ack.",
            json!({
                "ids": { "type": "array", "items": { "type": "string" }, "description": "Message ids (m-1, m-2, …)" },
                "to": { "type": "string", "description": "Mark all pending messages for this recipient session" },
                "state": { "type": "string", "enum": ["delivered", "acked"], "default": "acked" },
            }),
            &[],
            false,
            true,
        ));
        list.push(tool_with(
            "uniflo_wait_message",
            "Wait for incoming message",
            "Block on the per-process SSE watchdog until a message arrives for this agent (or timeout_secs elapses); wakes in milliseconds with zero polling.",
            json!({
                "to": { "type": "string", "description": "Optional recipient session key or prefix filter" },
                "cwd": { "type": "string", "description": "Optional working directory filter (defaults to this MCP process cwd)" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 60, "default": 30 },
                "ack": { "type": "boolean", "default": false, "description": "Mark received messages as acked (default: delivered)" },
            }),
            &[],
            false,
            false,
        ));
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new("http://127.0.0.1:9".into(), None, false, false)
    }

    fn call(s: &mut Server, line: &str) -> Value {
        s.handle_line(line).expect("a reply")
    }

    fn empty_local() -> (Local, std::sync::Weak<Engine>) {
        let opts = EngineOptions { cache_path: None, data_dir: None, usage: false, ..Default::default() };
        let engine = Engine::new(Vec::new(), opts);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let router = uniflo_gateway::router(engine.clone(), GuardOptions::default());
        let weak = Arc::downgrade(&engine);
        (Local { rt, engine, router, usage_ready: false, fts: false }, weak)
    }

    #[test]
    fn idle_fallback_index_is_released() {
        let mut s = server();
        let (local, engine) = empty_local();
        s.local = Some(local);
        s.noted = true;
        s.release_idle();
        assert!(s.local.is_none() && !s.noted);
        assert!(engine.upgrade().is_none(), "nothing else keeps the engine alive");

        let mut s = Server::new("http://127.0.0.1:9".into(), None, true, false);
        s.local = Some(empty_local().0);
        s.release_idle();
        assert!(s.local.is_some(), "--local keeps its index");
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

    #[test]
    fn message_tools_are_opt_in() {
        // Default server hides messaging tools and rejects calls to them.
        let mut off = server();
        let list = call(&mut off, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10);
        assert!(tools.iter().all(|t| t["name"] != "uniflo_send_message"));
        let denied = call(
            &mut off,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"uniflo_send_message","arguments":{"to":"claude:1","text":"hi"}}}"#,
        );
        assert_eq!(denied["error"]["code"], -32602);

        // Opt-in `--message` server exposes the 5 messaging tools + inbox resource with accurate readOnlyHint.
        let mut on = Server::new("http://127.0.0.1:9".into(), None, true, true);
        let init = call(&mut on, r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
        assert_eq!(init["result"]["capabilities"]["resources"]["subscribe"], true);
        let res = call(&mut on, r#"{"jsonrpc":"2.0","id":10,"method":"resources/list"}"#);
        assert_eq!(res["result"]["resources"][0]["uri"], "uniflo://inbox");
        let list = call(&mut on, r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 15);
        let peers = tools.iter().find(|t| t["name"] == "uniflo_peers").unwrap();
        assert_eq!(peers["annotations"]["readOnlyHint"], true);
        let send = tools.iter().find(|t| t["name"] == "uniflo_send_message").unwrap();
        assert_eq!(send["annotations"]["readOnlyHint"], false);
        let inbox = tools.iter().find(|t| t["name"] == "uniflo_inbox").unwrap();
        assert_eq!(inbox["annotations"]["readOnlyHint"], true);
        assert!(tools.iter().any(|t| t["name"] == "uniflo_wait_message"));
    }
}
