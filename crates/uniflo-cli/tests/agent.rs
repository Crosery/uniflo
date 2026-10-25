//! Agent access end to end with the real `uniflo` binary over a synthetic `UNIFLO_HOME` (= `HOME`):
//! the MCP server against a daemon and without one, the three entry points agreeing, resume
//! commands, memory files, `uniflo context`, and `uniflo setup` outside a terminal. Nothing
//! outside the temp directory is read or written.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");
const CODEX_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

fn closed_url() -> String {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    format!("http://127.0.0.1:{port}")
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn jl(v: Value) -> String {
    format!("{v}\n")
}

fn user(uuid: &str, ts: i64, cwd: &str, text: &str) -> String {
    jl(json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":cwd,"message":{"role":"user","content":text}}))
}

fn assistant(uuid: &str, ts: i64, content: Value, input: u64) -> String {
    jl(
        json!({"type":"assistant","uuid":format!("{uuid}-l"),"timestamp":ts,"message":{"id":uuid,"model":"claude-sonnet-4-5",
        "content":content,"stop_reason":"end_turn",
        "usage":{"input_tokens":input,"output_tokens":50,"cache_read_input_tokens":10,"cache_creation_input_tokens":0}}}),
    )
}

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn proj(&self) -> PathBuf {
        self.root().join("proj")
    }

    fn quoted_cwd(&self) -> PathBuf {
        self.root().join("it's a dir")
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let p = self.root().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// Only the temp home, no token, no network.
    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("HOME", self.root())
            .env("USERPROFILE", self.root())
            .env("UNIFLO_HOME", self.root())
            .env("UNIFLO_LANG", "en")
            .env_remove("UNIFLO_TOKEN")
            .env_remove("UNIFLO_URL")
            .env_remove("UNIFLO_DATA_DIR")
            .env_remove("UNIFLO_TRASH_DIR")
            .env_remove("UNIFLO_CACHE_DIR")
            .env_remove("UNIFLO_CONFIG_DIR")
            .env("NO_COLOR", "1");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).stdin(Stdio::null()).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    fn daemon(&self) -> Daemon {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bind = format!("127.0.0.1:{port}");
        let child = self
            .cmd()
            .args(["daemon", "--bind", &bind, "--no-cache", "--no-update-check", "--no-price-sync"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon { child, url: format!("http://{bind}"), port };
        wait("health", || d.get("/v1/health").0 == 200);
        wait("usage index", || d.get("/v1/usage").1["indexing"]["ready"] == true);
        wait("full-text index", || {
            let s = d.get("/v1/stats").1;
            s["fts"]["indexing"] == false && s["fts"]["progress"]["done"].as_u64() > Some(0)
        });
        d
    }

    fn mcp(&self, url: &str) -> Mcp {
        let mut child = self
            .cmd()
            .args(["--url", url, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for l in out.lines() {
                let Ok(l) = l else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Mcp { child, stdin, rx, id: 0 }
    }
}

/// Synthetic sessions of five harnesses plus agent memory files.
fn home() -> Home {
    let h = Home { dir: tempfile::tempdir().unwrap() };
    let proj = h.proj();
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::create_dir_all(proj.join("sub")).unwrap();
    std::fs::create_dir_all(h.quoted_cwd()).unwrap();
    let sub = proj.join("sub").display().to_string();
    let t = now_ms() - 3_600_000;
    let thinking = json!([{"type":"thinking","thinking":"plan the fix"},{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]);
    let tool_result = jl(json!({"type":"user","uuid":"r1","timestamp":t + 2,"cwd":sub,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"Cargo.toml"}]}}));
    h.write(
        ".claude/projects/-p/c1.jsonl",
        user("u1", t, &sub, "请帮我修复缓存击穿问题")
            + &assistant("m1", t + 1, thinking, 1200)
            + &tool_result
            + &assistant("m2", t + 3, json!([{"type":"text","text":"缓存击穿已修复"}]), 300),
    );
    let root = proj.display().to_string();
    h.write(
        ".claude/projects/-p/c2.jsonl",
        user("v1", t + 10, &root, "second task")
            + &assistant("n1", t + 11, json!([{"type":"text","text":"done"}]), 100),
    );
    h.write(".claude/projects/-p/c3.jsonl", user("w1", t + 20, &root, "third task"));
    let quoted = h.quoted_cwd().display().to_string();
    h.write(".claude/projects/-q/c4.jsonl", user("x1", t + 30, &quoted, "quoted cwd"));
    h.write(".claude/projects/-q/--dangerously-skip-permissions.jsonl", user("y1", t + 40, &quoted, "evil id"));
    h.write(".claude/projects/-o/c5.jsonl", user("z1", t + 50, "/elsewhere", "缓存击穿 elsewhere"));
    let l = |ty: &str, p: Value| jl(json!({"timestamp":"2026-10-09T10:00:00.000Z","type":ty,"payload":p}));
    h.write(
        &format!(".codex/sessions/2026/10/09/rollout-2026-10-09T10-00-00-{CODEX_ID}.jsonl"),
        l("session_meta", json!({"id":CODEX_ID,"timestamp":"2026-10-09T10:00:00Z","cwd":"/repo","source":"cli"})),
    );
    let db = h.root().join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT, directory TEXT, title TEXT,
           version TEXT, time_created INTEGER, time_updated INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
    )
    .unwrap();
    c.execute("INSERT INTO session VALUES ('ses_1','p',NULL,'s','/work','OpenCode task','1',?1,?1)", [t]).unwrap();
    drop(c);
    let header = json!({"type":"session","version":4,"id":"d1","createdAt":t,"cwd":"/w"}).to_string() + "\n";
    let frame = ruzstd::encoding::compress_to_vec(header.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
    h.write(".dsh/sessions/--w--/d1/session.jsonl.zstd", frame);
    h.write(".claude/CLAUDE.md", "global rules");
    h.write("proj/AGENTS.md", "project rules");
    h.write("proj/.cursor/rules/a.mdc", "cursor rule");
    h.write("proj/README.md", "not an instruction file");
    h.write(&format!(".claude/projects/{}/memory/MEMORY.md", slug(&proj)), "- fact");
    h
}

fn slug(p: &Path) -> String {
    p.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

struct Daemon {
    child: Child,
    url: String,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn get(&self, path: &str) -> (u16, Value) {
        let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", self.port)) else { return (0, Value::Null) };
        write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", self.port).unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { return (0, Value::Null) };
        let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut body = raw[split + 4..].to_vec();
        if head.contains("transfer-encoding: chunked") {
            body = dechunk(&body);
        }
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
        let n = usize::from_str_radix(std::str::from_utf8(&b[..i]).unwrap().trim(), 16).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&b[i + 2..i + 2 + n]);
        b = &b[i + 4 + n..];
    }
    out
}

fn wait(what: &str, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    id: u64,
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mcp {
    fn send_raw(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let line = match self.rx.recv_timeout(Duration::from_secs(120)) {
            Ok(l) => l,
            Err(e) => panic!("an MCP reply timeout (id={}): {e:?}", self.id),
        };
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout must carry JSON-RPC only: {e}: {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let msg = json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
        self.send_raw(&msg.to_string());
        let r = self.recv();
        assert_eq!(r["id"], self.id, "{r}");
        r
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        eprintln!("[test] calling tool: {name}");
        let r = self.request("tools/call", json!({"name":name,"arguments":args}));
        assert!(r.get("error").is_none(), "{name}: {r}");
        r["result"].clone()
    }

    /// Like `call`, with `structured: true` merged into `args` so `structuredContent` comes back
    /// (off by default to save tokens; most assertions in this file inspect it).
    fn call_structured(&mut self, name: &str, mut args: Value) -> Value {
        args.as_object_mut().expect("tool arguments must be an object").insert("structured".into(), json!(true));
        self.call(name, args)
    }

    fn handshake(&mut self) -> Value {
        let r = self.request(
            "initialize",
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}),
        );
        self.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        r
    }
}

/// A search response without `score`: bm25 × a recency weight computed at query time, so two
/// queries a few milliseconds apart differ in the last digits.
fn unscored(mut v: Value) -> Value {
    for s in v["results"].as_array_mut().into_iter().flatten() {
        s.as_object_mut().unwrap().remove("score");
    }
    v
}

fn ids(v: &Value, list: &str, field: &str) -> Vec<String> {
    v[list].as_array().unwrap().iter().map(|x| x[field].as_str().unwrap().to_owned()).collect()
}

/// Scenarios: MCP 握手与工具列表 · MCP 工具结果与 REST 一致 · 三个入口数据一致.
#[test]
fn mcp_cli_and_rest_agree() {
    let _lock = lock();
    let h = home();
    let d = h.daemon();
    let mut m = h.mcp(&d.url);
    let init = m.handshake();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["capabilities"], json!({"tools":{"listChanged":false}}));
    let tools = m.request("tools/list", json!({}))["result"]["tools"].clone();
    let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "uniflo_sessions",
            "uniflo_session",
            "uniflo_search",
            "uniflo_usage",
            "uniflo_session_usage",
            "uniflo_models",
            "uniflo_resume",
            "uniflo_memory",
            "uniflo_context",
            "uniflo_status"
        ]
    );
    for t in tools.as_array().unwrap() {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
    }
    assert_eq!(m.request("prompts/list", json!({}))["error"]["code"], -32601);
    assert_eq!(
        m.request("tools/call", json!({"name":"uniflo_sessions","arguments":{"limit":"ten"}}))["error"]["code"],
        -32602
    );
    assert_eq!(m.request("tools/call", json!({"name":"uniflo_nope","arguments":{}}))["error"]["code"], -32602);
    m.send_raw("{\"jsonrpc\":\"2.0\",\"id\":99,");
    assert_eq!(m.recv()["error"]["code"], -32700);

    // sessions: limit 1000 is clamped to 100, same list as REST. `structured` defaults to false.
    let r = m.call("uniflo_sessions", json!({"limit": 1000}));
    assert_eq!(r["isError"], false);
    assert!(r.get("structuredContent").is_none(), "structured defaults to false: {r}");
    let r = m.call_structured("uniflo_sessions", json!({"limit": 1000}));
    let rest = d.get("/v1/sessions?limit=100&q=").1;
    assert_eq!(r["structuredContent"]["sessions"], rest);
    assert_eq!(rest.as_array().unwrap().len(), 9);
    let cli = h.json(&["--url", &d.url, "ls", "-n", "100", "--json"]);
    assert_eq!(cli, rest, "CLI --json, REST and MCP list the same sessions with the same numbers");
    let c1 = rest.as_array().unwrap().iter().find(|s| s["key"] == "claude:c1").unwrap();
    assert!(c1["usage"]["cost_usd"].as_f64().unwrap() > 0.0);

    // search: limit 1000 → 30; hits reference the event id, not a repeated uniflo:// URL.
    let r = m.call_structured("uniflo_search", json!({"q": "缓存击穿", "limit": 1000}));
    let rest = d.get(&format!("/v1/search?q={}&limit=30", enc("缓存击穿"))).1;
    assert_eq!(unscored(r["structuredContent"].clone()), unscored(rest.clone()));
    let mut hit = ids(&rest, "results", "session");
    hit.sort();
    assert_eq!(hit, ["claude:c1", "claude:c5"]);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\n- claude:c1|") && text.contains("\n  #u1 [user_message "), "{text}");
    assert!(!text.contains("uniflo://session"), "{text}");
    let cli = h.json(&["--url", &d.url, "grep", "缓存击穿", "-n", "30", "--json"]);
    assert_eq!(unscored(cli), unscored(rest));

    // usage, per-session usage.
    let r = m.call_structured("uniflo_usage", json!({"group_by": "model"}));
    let rest = d.get("/v1/usage?group_by=model").1;
    assert_eq!(r["structuredContent"], rest);
    assert_eq!(h.json(&["--url", &d.url, "usage", "--by", "model", "--json"]), rest);
    assert_eq!(rest["totals"]["steps"], 3);
    let r = m.call_structured("uniflo_session_usage", json!({"key": "claude:c1"}));
    assert_eq!(r["structuredContent"], d.get("/v1/sessions/claude%3Ac1/usage").1);

    // One session: events and ids as REST, tools and reasoning only when asked for.
    let r = m.call_structured(
        "uniflo_session",
        json!({"key": "claude:c1", "include_tools": true, "include_reasoning": true, "limit": 1000}),
    );
    let rest = d.get("/v1/sessions/claude%3Ac1/events?limit=200&max_text=4000").1;
    assert_eq!(r["structuredContent"]["events"], rest["events"]);
    assert_eq!(r["structuredContent"]["session"], d.get("/v1/sessions/claude%3Ac1").1);
    let kinds: Vec<&str> = rest["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"tool_call") && kinds.contains(&"reasoning"), "{kinds:?}");
    let r = m.call_structured("uniflo_session", json!({"key": "uniflo://session/claude:c1#u1"}));
    let lean = &r["structuredContent"];
    assert_eq!(lean["around"], "u1");
    assert!(
        lean["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| !["tool_call", "tool_result", "reasoning"].contains(&e["kind"].as_str().unwrap()))
    );
    assert!(lean["omitted"]["tool_events"].as_u64() >= Some(2));
    assert!(r["content"][0]["text"].as_str().unwrap().contains("请帮我修复缓存击穿问题"));

    // Unknown keys are results with isError, not protocol errors.
    for (tool, args) in [
        ("uniflo_session", json!({"key": "claude:missing"})),
        ("uniflo_session_usage", json!({"key": "claude:missing"})),
        ("uniflo_resume", json!({"key": "claude:missing"})),
    ] {
        assert_eq!(m.call(tool, args)["isError"], true, "{tool}");
    }
    let r = m.call_structured("uniflo_resume", json!({"key": "claude:c4"}));
    assert_eq!(r["structuredContent"], d.get("/v1/sessions/claude%3Ac4/resume").1);
    let r = m.call_structured("uniflo_models", json!({}));
    assert_eq!(r["structuredContent"]["models"], d.get("/v1/models").1);
    let r = m.call_structured("uniflo_status", json!({}));
    assert_eq!(r["structuredContent"]["harnesses"], d.get("/v1/harnesses").1);
    assert_eq!(r["structuredContent"]["health"]["sessions"], 9);
}

/// Scenario: `structured` 默认关闭，只有显式传 true 才带 structuredContent.
#[test]
fn structured_flag_defaults_to_off() {
    let _lock = lock();
    let h = home();
    let mut m = h.mcp(&closed_url());
    m.handshake();
    let r = m.call("uniflo_status", json!({}));
    assert!(r.get("structuredContent").is_none(), "{r}");
    assert!(r["content"][0]["text"].as_str().unwrap().contains("sessions"), "{r}");
    let r = m.call_structured("uniflo_status", json!({}));
    assert!(r["structuredContent"]["health"].is_object(), "{r}");
}

/// Scenario: `uniflo_sessions` 默认文本的每条会话行足够紧凑（token 预算）.
#[test]
fn uniflo_sessions_default_text_is_compact() {
    let _lock = lock();
    let h = home();
    let mut m = h.mcp(&closed_url());
    m.handshake();
    let r = m.call("uniflo_sessions", json!({"limit": 100}));
    assert!(r.get("structuredContent").is_none(), "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("- ")).collect();
    assert!(lines.len() >= 6, "{text}");
    for l in &lines {
        assert!(l.len() <= 240, "{} bytes over budget: {l}", l.len());
    }
}

/// Scenario: `uniflo_search` 的 offset 翻页到互不重叠的会话并带 more: footer；context 把邻近事件内联到命中下.
#[test]
fn search_paging_and_context() {
    let _lock = lock();
    let h = home();
    let d = h.daemon();
    let mut m = h.mcp(&d.url);
    m.handshake();

    // offset=0: first page, one disjoint session, a footer because one more matches.
    let r = m.call("uniflo_search", json!({"q": "缓存击穿", "limit": 1}));
    let page0 = r["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(page0.contains("claude:c1") && !page0.contains("claude:c5"), "{page0}");
    assert!(page0.contains("more: offset=1"), "{page0}");
    assert!(!page0.contains("uniflo://session"), "hits reference an event id, not a repeated URL: {page0}");

    // offset=1: the other session, no more pages left.
    let r = m.call("uniflo_search", json!({"q": "缓存击穿", "limit": 1, "offset": 1}));
    let page1 = r["content"][0]["text"].as_str().unwrap();
    assert!(page1.contains("claude:c5") && !page1.contains("claude:c1"), "{page1}");
    assert!(!page1.contains("more:"), "last page has no footer: {page1}");

    // context: 1 inlines +/-1 neighbour events under the first hit of each of the first sessions.
    let r = m.call("uniflo_search", json!({"q": "缓存击穿", "context": 1}));
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\n    ["), "indented context lines: {text}");
    assert!(text.contains("Cargo.toml"), "neighbour tool_result inlined: {text}");
    assert!(text.contains("assistant #m2-l#0] 缓存击穿已修复"), "neighbour assistant text inlined: {text}");

    // context: 0 (default) does not fetch neighbours.
    let r = m.call("uniflo_search", json!({"q": "缓存击穿"}));
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("\n    ["), "context defaults to 0: {text}");
}

/// Scenario: `around` 不显式传 max_text 时裁到 600 字节；显式传参覆盖这个默认值.
#[test]
fn session_around_defaults_max_text_to_600() {
    let _lock = lock();
    let h = Home { dir: tempfile::tempdir().unwrap() };
    let t = now_ms();
    let long = "缓存击穿排查记录".repeat(200);
    assert!(long.len() > 4000, "fixture must be long enough to tell 600 / 4000 / unlimited apart");
    h.write(
        ".claude/projects/-p/c1.jsonl",
        user("u1", t, "/repo", "short question") + &assistant("m1", t + 1, json!([{"type":"text","text": long}]), 10),
    );
    let mut m = h.mcp(&closed_url());
    m.handshake();
    let event = |r: &Value| -> Value {
        r["structuredContent"]["events"].as_array().unwrap().iter().find(|e| e["id"] == "m1-l#0").unwrap().clone()
    };

    // No max_text: around defaults to 600, not the usual 4000.
    let r = m.call_structured("uniflo_session", json!({"key": "claude:c1", "around": "m1-l#0"}));
    let e = event(&r);
    assert_eq!(e["truncated"], true, "{e}");
    assert!(e["text"].as_str().unwrap().len() <= 600, "{e}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("assistant #m1-l#0 cut]") && text.contains("max_text=0"),
        "cut events say how to get the rest: {text}"
    );

    // Explicit max_text overrides the around-specific default.
    let r = m.call_structured("uniflo_session", json!({"key": "claude:c1", "around": "m1-l#0", "max_text": 4800}));
    let e = event(&r);
    assert_ne!(e["truncated"], json!(true), "`truncated` is omitted (defaults to false) when not cut: {e}");
    assert_eq!(e["text"].as_str().unwrap().len(), 4800);
}

/// Scenario: 默认文本是紧凑表格但不丢有用字段：图例、cwd 分组、子代理嵌套、消息 #id、用量列、价格、短词扫描与零结果提示.
#[test]
fn compact_text_keeps_useful_fields() {
    let _lock = lock();
    let h = Home { dir: tempfile::tempdir().unwrap() };
    let t = now_ms() - 60_000;
    h.write(
        ".claude/projects/-p/root1.jsonl",
        user("u1", t, "/repo", "修复缓存击穿") + &assistant("m1", t + 1, json!([{"type":"text","text":"ok"}]), 1200),
    );
    h.write(".claude/projects/-p/root1/subagents/agent-s1.jsonl", user("s1", t + 2, "/repo", "子任务"));
    h.write(".claude/projects/-p/agent-s1/subagents/agent-s2.jsonl", user("s2", t + 4, "/repo", "孙任务"));
    h.write(".claude/projects/-q/other.jsonl", user("o1", t + 3, "/elsewhere", "another"));
    let mut m = h.mcp(&closed_url());
    m.handshake();
    let text = |r: Value| r["content"][0]["text"].as_str().unwrap().to_owned();

    let s = text(m.call("uniflo_sessions", json!({})));
    assert!(s.contains("4 sessions, newest first · key|title|model|status|updated|cost|steps"), "{s}");
    assert!(
        s.contains("\n- claude:root1|") && s.contains("\n  ↳ claude:agent-s1|"),
        "subagent nested under its parent: {s}"
    );
    assert!(
        s.contains("\n- claude:agent-s2|") && s.contains("sub of claude:agent-s1"),
        "a nested row's subagent stays listed: {s}"
    );
    assert_eq!(s.matches("claude:agent-s1|子任务").count(), 1, "printed once: {s}");
    assert!(s.contains("\n@ /elsewhere\n- claude:other|"), "{s}");

    let s = text(m.call("uniflo_session", json!({"key": "claude:root1"})));
    assert!(s.starts_with("claude:root1 · "), "{s}");
    assert!(s.contains("started ") && s.contains("\nusage: "), "{s}");
    assert!(s.contains(" user #u1] 修复缓存击穿") && s.contains(" assistant #m1-l#0] ok"), "{s}");

    let s = text(m.call("uniflo_usage", json!({"group_by": "model"})));
    assert!(s.contains("label|cost|sessions|prompts|steps|in|out|cache_r|cache_w|reasoning|unpriced"), "{s}");
    assert!(s.contains("\nclaude-sonnet-4-5|$") && s.contains("\ntotal|$"), "{s}");

    let s = text(m.call("uniflo_models", json!({})));
    assert!(s.contains("model|sessions|steps|cost|unpriced|USD per 1M in/out/cache_r/cache_w|ctx|out"), "{s}");
    assert!(s.contains("\n- claude-sonnet-4-5"), "{s}");

    let s = text(m.call("uniflo_session_usage", json!({"key": "claude:root1"})));
    assert!(
        s.contains("\nmodels: claude-sonnet-4-5 1 steps") && s.contains("\nturns · turn|started|cost|prompts"),
        "{s}"
    );

    let s = text(m.call("uniflo_search", json!({"q": "缓存"})));
    assert!(s.contains("substring scan, newest first"), "a 2-character term says how it was searched: {s}");
    let s = text(m.call("uniflo_search", json!({"q": "zzqqxx"})));
    assert!(s.starts_with("0 sessions match") && s.contains("try fewer terms"), "{s}");

    let s = text(m.call("uniflo_status", json!({})));
    assert!(s.contains("schema v") && s.contains(" · up ") && s.contains("\n- claude: 4 sessions"), "{s}");
    assert!(s.contains("\nno sessions: ") && !s.contains("()"), "harnesses without roots print no empty parens: {s}");
}

/// Scenario: MCP 在守护进程不可用时退回本地.
#[test]
fn mcp_answers_in_process_without_a_daemon() {
    let _lock = lock();
    let h = home();
    let mut m = h.mcp(&closed_url());
    m.handshake();
    let r = m.call_structured("uniflo_sessions", json!({"q": "h:claude", "limit": 50}));
    assert!(r["content"][0]["text"].as_str().unwrap().starts_with("守护进程未运行"), "{r}");
    let keys = ids(&r["structuredContent"], "sessions", "key");
    assert_eq!(keys.len(), 6);
    assert!(keys.contains(&"claude:c1".to_owned()));
    let r = m.call_structured("uniflo_usage", json!({"group_by": "harness"}));
    assert!(!r["content"][0]["text"].as_str().unwrap().contains("守护进程未运行"), "noted once");
    assert_eq!(r["structuredContent"]["totals"]["steps"], 3);
    let r = m.call_structured("uniflo_search", json!({"q": "缓存击穿"}));
    assert!(ids(&r["structuredContent"], "results", "session").contains(&"claude:c1".to_owned()), "{r}");
    assert_eq!(m.call_structured("uniflo_status", json!({}))["structuredContent"]["health"]["sessions"], 9);
}

#[test]
fn mcp_switches_back_to_a_daemon_started_after_local_fallback() {
    let _lock = lock();
    let h = home();
    let url = closed_url();
    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    let mut m = h.mcp(&url);
    m.handshake();
    let first = m.call_structured("uniflo_search", json!({"q": "缓存击穿"}));
    assert!(ids(&first["structuredContent"], "results", "session").contains(&"claude:c1".to_owned()));
    h.write(".claude/projects/-p/recovered.jsonl", user("recovered-u", now_ms(), "/synthetic", "daemonrecoverymarker"));
    let child = h
        .cmd()
        .args(["daemon", "--bind", &format!("127.0.0.1:{port}"), "--no-cache", "--no-update-check", "--no-price-sync"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let d = Daemon { child, url, port };
    wait("recovered daemon health", || d.get("/v1/health").0 == 200);
    wait("MCP returns new daemon content", || {
        let r = m.call_structured("uniflo_search", json!({"q": "daemonrecoverymarker"}));
        r["structuredContent"]["results"]
            .as_array()
            .is_some_and(|a| a.iter().any(|s| s["session"] == "claude:recovered"))
    });
    assert_eq!(d.get("/v1/stats").1["fts"]["errors"], 0);
}

/// Scenario: 恢复命令与注入防护.
#[test]
fn resume_commands_and_injection_guard() {
    let _lock = lock();
    let h = home();
    let d = h.daemon();
    let resume = |key: &str| d.get(&format!("/v1/sessions/{}/resume", enc(key))).1;
    assert_eq!(resume(&format!("codex:{CODEX_ID}"))["argv"], json!(["codex", "resume", CODEX_ID]));
    assert_eq!(resume("opencode:ses_1")["argv"], json!(["opencode", "--session", "ses_1"]));
    let dsh = resume("dsh:d1");
    assert_eq!(dsh["supported"], false);
    assert!(dsh["reason"].as_str().unwrap().starts_with("dsh"), "{dsh}");
    let evil = resume("claude:--dangerously-skip-permissions");
    assert_eq!((evil["supported"].as_bool(), evil["reason"].as_str()), (Some(false), Some("会话 id 不合法")));

    // The shell line reproduces cwd and argv byte for byte.
    let c4 = resume("claude:c4");
    assert_eq!(c4["argv"], json!(["claude", "--resume", "c4"]));
    // The command line runs in a POSIX shell with a stub `claude` on PATH.
    #[cfg(unix)]
    {
        let stub = h.root().join("stub");
        std::fs::create_dir_all(&stub).unwrap();
        let rec = h.root().join("argv.txt");
        std::fs::write(
            stub.join("claude"),
            format!("#!/bin/sh\n{{ pwd; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; }} > '{}'\n", rec.display()),
        )
        .unwrap();
        std::fs::set_permissions(stub.join("claude"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let st = Command::new("/bin/sh")
            .args(["-c", c4["command"].as_str().unwrap()])
            .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
            .status()
            .unwrap();
        assert!(st.success());
        let got = std::fs::read_to_string(&rec).unwrap();
        let mut lines = got.lines();
        let pwd = PathBuf::from(lines.next().unwrap());
        assert_eq!(pwd.canonicalize().unwrap(), h.quoted_cwd().canonicalize().unwrap());
        assert_eq!(lines.collect::<Vec<_>>(), ["--resume", "c4"]);
    }

    // `uniflo resume --print` prints the same line; unsupported sessions exit 2.
    let out = h.run(&["--url", &d.url, "resume", "claude:c4", "--print"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), c4["command"].as_str().unwrap());
    for key in ["dsh:d1", "claude:--dangerously-skip-permissions"] {
        let out = h.run(&["--url", &d.url, "resume", key, "--print"]);
        assert_eq!(out.status.code(), Some(2), "{key}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot resume"), "{key}");
    }
    drop(d);
    let out = h.run(&["--url", &closed_url(), "resume", "dsh:d1", "--print"]);
    assert_eq!(out.status.code(), Some(2), "in-process too");
}

/// Scenario: 记忆文件与接力上下文.
#[test]
fn memory_files_and_context() {
    let _lock = lock();
    let h = home();
    let d = h.daemon();
    let sub = h.proj().join("sub");
    let (st, list) = d.get(&format!("/v1/memory?cwd={}", enc(&sub.display().to_string())));
    assert_eq!(st, 200);
    let scope = |suffix: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"].as_str().unwrap().replace('\\', "/").ends_with(suffix))
            .map(|f| f["scope"].clone())
    };
    assert_eq!(scope(".claude/CLAUDE.md"), Some(json!("global")));
    assert_eq!(scope("proj/AGENTS.md"), Some(json!("project")));
    assert_eq!(scope(".cursor/rules/a.mdc"), Some(json!("project")));
    assert_eq!(scope("memory/MEMORY.md"), Some(json!("project")));
    assert_eq!(scope("README.md"), None);
    let agents = h.proj().join("AGENTS.md").display().to_string();
    let (st, f) = d.get(&format!("/v1/memory/file?path={}", enc(&agents)));
    assert_eq!((st, f["content"].as_str()), (200, Some("project rules")));
    for bad in [h.proj().join("README.md"), h.root().join(".ssh/config"), h.proj().join("sub/../AGENTS.md")] {
        assert_eq!(d.get(&format!("/v1/memory/file?path={}", enc(&bad.display().to_string()))).0, 403, "{bad:?}");
    }

    let out = h.run(&["--url", &d.url, "context", "--cwd", &sub.display().to_string()]);
    assert!(out.status.success());
    let md = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<&str> = md.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(rows.len(), 3, "{md}");
    assert!(rows[0].contains("`claude:c3`") && rows[2].contains("`claude:c1`"), "newest first: {md}");
    assert!(rows[2].contains("请帮我修复缓存击穿问题") && rows[2].contains('$'), "{md}");
    assert!(md.contains("uniflo_session") && md.contains("uniflo show"), "{md}");
    let report = h.json(&["--url", &d.url, "context", "--cwd", &sub.display().to_string(), "--json", "-n", "2"]);
    assert_eq!(report["sessions"].as_array().unwrap().len(), 2);
    let mut m = h.mcp(&d.url);
    m.handshake();
    let r = m.call_structured("uniflo_context", json!({"cwd": sub.display().to_string(), "limit": 2}));
    assert_eq!(r["structuredContent"]["sessions"], report["sessions"]);
    let r = m.call_structured("uniflo_memory", json!({"cwd": sub.display().to_string()}));
    assert_eq!(r["structuredContent"]["files"], list);
    let r = m.call_structured("uniflo_memory", json!({"path": agents}));
    assert_eq!(r["structuredContent"]["content"], "project rules");
    drop(m);
    let url = d.url.clone();
    drop(d);

    let out = h.run(&["--url", &url, "context", "--cwd", &sub.display().to_string()]);
    assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0), "no daemon: empty, exit 0");
    let out = h.run(&["--url", &url, "context", "--cwd", "/nowhere"]);
    assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 0));
}

/// Scenario: 非交互、dry-run 与触发时机 — through the binary, outside a terminal.
#[test]
fn setup_outside_a_terminal_writes_nothing_and_nothing_prompts() {
    let _lock = lock();
    let h = Home { dir: tempfile::tempdir().unwrap() };
    h.write(".cursor/mcp.json", "{\"mcpServers\":{}}");
    std::fs::create_dir_all(h.root().join(".claude/skills")).unwrap();
    let snapshot = || {
        let mut v = Vec::new();
        let mut stack = vec![h.root().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let md = std::fs::symlink_metadata(e.path()).unwrap();
                v.push((e.path(), md.len(), md.modified().unwrap()));
                if md.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        v.sort();
        v
    };
    let before = snapshot();
    for args in [&["setup"][..], &["setup", "--dry-run"], &["setup", "--both", "--hook"], &["setup", "--uninstall"]] {
        let out = h.cmd().env("PATH", "/usr/bin:/bin").args(args).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{args:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        if args.get(1) != Some(&"--uninstall") {
            assert!(text.contains("cursor") && text.contains("~/.cursor/mcp.json"), "{args:?}: {text}");
            assert!(text.contains("nothing written"), "{args:?}: {text}");
        }
    }
    // Ordinary commands never ask outside a terminal; neither does the MCP server.
    let closed = closed_url();
    let out = h.run(&["--url", &closed, "ls", "--json"]);
    assert!(out.status.success() && !String::from_utf8_lossy(&out.stdout).contains("设置"));
    let mut m = h.mcp(&closed);
    assert_eq!(m.handshake()["result"]["serverInfo"]["name"], "uniflo");
    drop(m);
    let own = |p: &Path| {
        ["Library", "AppData", ".cache", ".local", ".config"].iter().any(|d| p.starts_with(h.root().join(d)))
    };
    let after: Vec<_> = snapshot().into_iter().filter(|(p, ..)| !own(p)).collect();
    let before: Vec<_> = before.into_iter().filter(|(p, ..)| !own(p)).collect();
    assert_eq!(after, before, "only Uniflo's own cache may appear");
    assert!(!snapshot().iter().any(|(p, ..)| p.ends_with("setup.json")), "no state written");
    let out = h.run(&["skill", "print"]);
    assert!(String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n").starts_with("---\nname: uniflo\n"));
}

/// Scenario: 真实 harness 接入（隔离 HOME）. Needs the real `claude` and `codex` on PATH; run with
/// `cargo test -p uniflo --test agent -- --ignored real_`.
#[test]
#[ignore = "uses the real claude and codex CLIs"]
fn real_claude_and_codex_setup_in_an_isolated_home() {
    let _lock = lock();
    let h = Home { dir: tempfile::tempdir().unwrap() };
    let claude_json = json!({"hasCompletedOnboarding": true, "mcpServers": {"other": {"type": "stdio", "command": "/usr/bin/true", "args": []}}});
    h.write(".claude.json", serde_json::to_string_pretty(&claude_json).unwrap());
    let toml = "model = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"/usr/bin/true\"\nargs = []\n";
    h.write(".codex/config.toml", toml);
    let real = |args: &[&str]| -> Output {
        let mut c = Command::new(args[0]);
        c.args(&args[1..])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", h.root())
            .env("TERM", "dumb")
            .stdin(Stdio::null());
        c.output().unwrap()
    };
    let setup = |extra: &[&str]| {
        let mut a = vec!["setup", "--mcp", "--agents", "claude,codex", "--yes"];
        a.extend_from_slice(extra);
        let out = h.run(&a);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    let out = setup(&[]);
    println!("{out}");
    let list = real(&["claude", "mcp", "list"]);
    let list = String::from_utf8_lossy(&list.stdout).to_string();
    println!("claude mcp list:\n{list}");
    let line = list.lines().find(|l| l.starts_with("uniflo:")).expect("uniflo listed by claude");
    assert!(line.contains("Connected"), "{line}");
    let clist = String::from_utf8_lossy(&real(&["codex", "mcp", "list"]).stdout).to_string();
    println!("codex mcp list:\n{clist}");
    assert!(clist.lines().any(|l| l.starts_with("uniflo ")), "{clist}");

    let backups = |h: &Home| {
        std::fs::read_dir(h.root())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-uniflo-"))
            .count()
            + std::fs::read_dir(h.root().join(".codex"))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".bak-uniflo-"))
                .count()
    };
    let n = backups(&h);
    let out = setup(&[]);
    assert!(out.contains("未变化"), "{out}");
    assert_eq!(backups(&h), n, "no new backups");
    let cj: Value = serde_json::from_str(&std::fs::read_to_string(h.root().join(".claude.json")).unwrap()).unwrap();
    assert_eq!(cj["mcpServers"].as_object().unwrap().keys().collect::<Vec<_>>(), ["other", "uniflo"]);
    let ct = std::fs::read_to_string(h.root().join(".codex/config.toml")).unwrap();
    assert_eq!(ct.matches("[mcp_servers.uniflo]").count(), 1);

    let out = h.run(&["setup", "--uninstall", "--yes"]);
    println!("{}", String::from_utf8_lossy(&out.stdout));
    assert!(out.status.success());
    let cj: Value = serde_json::from_str(&std::fs::read_to_string(h.root().join(".claude.json")).unwrap()).unwrap();
    assert_eq!(cj["mcpServers"], claude_json["mcpServers"], "claude: other entries as before");
    let ct = std::fs::read_to_string(h.root().join(".codex/config.toml")).unwrap();
    println!("codex config.toml after uninstall byte-identical to the original: {}", ct == toml);
    let doc: toml_edit::DocumentMut = ct.parse().unwrap();
    assert_eq!(doc["model"].as_str(), Some("gpt-5"));
    assert_eq!(doc["mcp_servers"]["other"]["command"].as_str(), Some("/usr/bin/true"));
    assert!(doc["mcp_servers"].get("uniflo").is_none());
    assert!(!String::from_utf8_lossy(&real(&["claude", "mcp", "list"]).stdout).contains("uniflo:"));
    assert!(!String::from_utf8_lossy(&real(&["codex", "mcp", "list"]).stdout).contains("uniflo "));
}
