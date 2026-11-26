//! End-to-end test of optional cross-agent messaging with the real `uniflo` binary:
//! lazy disk creation, default-off MCP tools vs `mcp --message`, CLI `message send/inbox/ack`,
//! and silent `message hook` injection (and offline safety).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Env {
    dir: tempfile::TempDir,
    url: String,
    daemon: Option<Child>,
}

impl Env {
    fn start() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let claude = root.join(".claude/projects/-proj");
        std::fs::create_dir_all(&claude).unwrap();
        let now = now_ms();
        std::fs::write(
            claude.join("alpha.jsonl"),
            format!(
                "{}\n",
                json!({"type":"user","uuid":"u1","timestamp":now - 500,"cwd":proj,"message":{"role":"user","content":"working on backend"}})
            ),
        )
        .unwrap();

        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let url = format!("http://{bind}");
        let mut cmd = base_cmd(root, &url);
        let child = cmd
            .args(["daemon", "-f", "--bind", &bind, "--no-fts", "--no-cache", "--no-update-check", "--no-price-sync"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(&bind).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Env { dir, url, daemon: Some(child) }
    }

    fn cmd(&self) -> Command {
        base_cmd(self.dir.path(), &self.url)
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut c) = self.daemon.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn base_cmd(root: &Path, url: &str) -> Command {
    let mut c = Command::new(BIN);
    c.env("HOME", root)
        .env("USERPROFILE", root)
        .env("UNIFLO_HOME", root)
        .env("UNIFLO_DATA_DIR", root.join("data"))
        .env("UNIFLO_CACHE_DIR", root.join("cache"))
        .env("UNIFLO_CONFIG_DIR", root.join("config"))
        .env("UNIFLO_URL", url)
        .env("UNIFLO_LANG", "en")
        .env("UNIFLO_NO_SETUP", "1")
        .env("NO_COLOR", "1")
        .env_remove("UNIFLO_TOKEN")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME");
    c
}

fn mcp_roundtrip(env: &Env, extra_args: &[&str], requests: &[Value]) -> Vec<Value> {
    let mut cmd = env.cmd();
    cmd.arg("mcp").args(extra_args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for r in requests {
        writeln!(stdin, "{r}").unwrap();
    }
    drop(stdin);
    let out: Vec<Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&l).unwrap())
        .filter(|v: &Value| v.get("id").is_some())
        .collect();
    assert!(child.wait().unwrap().success());
    out
}

#[test]
fn opt_in_mcp_cli_and_silent_hook_end_to_end() {
    let env = Env::start();

    // 1. Lazy disk creation: no messages directory until first message is sent.
    assert!(!env.data_dir().join("messages").exists());

    // 2. Default `uniflo mcp` exposes only the 10 read-only tools.
    let replies = mcp_roundtrip(&env, &[], &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})]);
    assert_eq!(replies[0]["result"]["tools"].as_array().unwrap().len(), 10);

    // 3. Opt-in `uniflo mcp --message` sends, lists inbox, and acks end-to-end.
    let replies = mcp_roundtrip(
        &env,
        &["--message"],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"uniflo_send_message",
                "arguments":{"to":"claude:alpha","from":"codex:peer","text":"sync check","structured":true}
            }}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"uniflo_inbox",
                "arguments":{"to":"claude:alpha","structured":true}
            }}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"uniflo_ack_messages",
                "arguments":{"to":"claude:alpha","structured":true}
            }}),
        ],
    );
    assert_eq!(replies[0]["result"]["tools"].as_array().unwrap().len(), 14);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[1]["result"]["structuredContent"]["target"]["key"], "claude:alpha");
    assert_eq!(replies[2]["result"]["structuredContent"]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(replies[3]["result"]["structuredContent"]["updated"], 1);
    assert!(env.data_dir().join("messages/log.jsonl").is_file());

    // 3b. Per-process SSE watchdog bound to `uniflo mcp --message`: wakes `uniflo_wait_message`
    // in milliseconds and emits JSON-RPC `notifications/message` over stdio for any MCP harness.
    let proj = env.dir.path().join("proj");
    let mut wd_cmd = env.cmd();
    wd_cmd
        .current_dir(&proj)
        .args(["mcp", "--message"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut wd_child = wd_cmd.spawn().unwrap();
    let mut wd_in = wd_child.stdin.take().unwrap();
    let mut wd_out = BufReader::new(wd_child.stdout.take().unwrap());
    writeln!(wd_in, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).unwrap();
    wd_in.flush().unwrap();
    let mut init_line = String::new();
    wd_out.read_line(&mut init_line).unwrap();
    assert!(init_line.contains("\"resources\""));
    // Give SSE watchdog a brief moment to connect to `/v1/messages/stream?cwd=...`.
    std::thread::sleep(Duration::from_millis(120));
    // Start blocking `uniflo_wait_message`, then send from a peer in another thread.
    writeln!(
        wd_in,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"uniflo_wait_message","arguments":{"timeout_secs":5,"ack":true,"structured":true}}})
    )
    .unwrap();
    wd_in.flush().unwrap();
    let t_send = Instant::now();
    let push = env
        .cmd()
        .args(["message", "send", "claude:alpha", "ping-watchdog", "--from", "codex:peer", "--json"])
        .output()
        .unwrap();
    assert!(push.status.success());
    let mut saw_notif = false;
    let mut wait_reply = None;
    for _ in 0..5 {
        let mut l = String::new();
        wd_out.read_line(&mut l).unwrap();
        let v: Value = serde_json::from_str(&l).unwrap();
        if v["method"] == "notifications/message" {
            saw_notif = v["params"]["data"]["text"] == "ping-watchdog";
        } else if v["id"] == 2 {
            wait_reply = Some(v);
            break;
        }
    }
    assert!(t_send.elapsed() < Duration::from_secs(3), "watchdog took {:?}", t_send.elapsed());
    assert!(saw_notif, "expected live MCP notifications/message from SSE watchdog");
    let reply = wait_reply.expect("expected uniflo_wait_message reply");
    assert_eq!(reply["result"]["isError"], false);
    assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("ping-watchdog"));
    drop(wd_in);
    assert!(wd_child.wait().unwrap().success());

    // 4. CLI `message send` -> `message hook` injects once, then stays silent.
    let send = env
        .cmd()
        .args(["message", "send", "claude:alpha", "build", "green", "--from", "agent-b", "--json"])
        .output()
        .unwrap();
    assert!(send.status.success(), "{}", String::from_utf8_lossy(&send.stderr));

    let hook_input = json!({"session_id":"alpha","cwd":env.dir.path().join("proj")}).to_string();
    let run_hook = |url: &str| {
        let mut c = base_cmd(env.dir.path(), url);
        let mut child = c
            .args(["message", "hook", "--harness", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(hook_input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };

    let first = run_hook(&env.url);
    assert!(first.status.success());
    let stdout1 = String::from_utf8(first.stdout).unwrap();
    assert!(stdout1.contains("<uniflo-messages>") && stdout1.contains("build green"), "{stdout1}");

    // Second hook call: already delivered, completely silent.
    let second = run_hook(&env.url);
    assert!(second.status.success());
    assert!(second.stdout.is_empty(), "expected empty stdout, got: {}", String::from_utf8_lossy(&second.stdout));

    // Offline daemon: hook exits 0 silently without blocking the host agent.
    let closed = format!("http://127.0.0.1:{}", free_port());
    let offline = run_hook(&closed);
    assert!(offline.status.success());
    assert!(offline.stdout.is_empty());
}
