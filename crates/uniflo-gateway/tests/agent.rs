//! Resume and open-terminal routes over real TCP, with an injected launcher: no test ever runs
//! `open` or opens a window.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_core::resume::{sh_quote, terminal_script};
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::GuardOptions;
use uniflo_gateway::agent::{Launcher, with_launcher};

struct Srv {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    addr: std::net::SocketAddr,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

fn line(id: &str, cwd: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"user","uuid":format!("{id}-u"),"timestamp":"2026-10-02T12:00:00Z","cwd":cwd,"message":{"role":"user","content":"hi"}})
    )
}

async fn start(guard: GuardOptions) -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("it's a dir");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-w");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("s1.jsonl"), line("s1", cwd.to_str().unwrap())).unwrap();
    std::fs::write(proj.join("--dangerously-skip-permissions.jsonl"), line("d", cwd.to_str().unwrap())).unwrap();
    let adapter: Arc<dyn Adapter> = Arc::new(JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![root],
        live_dir: None,
    }));
    let engine = Engine::new(vec![adapter], EngineOptions { cache_path: None, ..Default::default() });
    engine.index();
    let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let rec = calls.clone();
    let launcher: Launcher = Arc::new(move |argv: &[String]| {
        rec.lock().unwrap().push(argv.to_vec());
        Ok(())
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = with_launcher(uniflo_gateway::router(engine, guard), launcher);
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));
    Srv { _dir: dir, cwd, addr, calls }
}

async fn req(addr: std::net::SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut r = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: 0\r\n",
        addr.port()
    );
    for (k, v) in headers {
        r += &format!("{k}: {v}\r\n");
    }
    r += "\r\n";
    s.write_all(r.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw)).await.unwrap().unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = String::from_utf8_lossy(&raw[..split]).split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null))
}

const OPEN: &str = "/v1/sessions/claude%3As1/open-terminal";

/// Scenario: 恢复命令与注入防护 (gateway half).
#[tokio::test]
async fn resume_commands_validate_ids_and_quote_paths() {
    let s = start(GuardOptions::default()).await;
    let (st, v) = req(s.addr, "GET", "/v1/sessions/claude%3As1/resume", &[]).await;
    assert_eq!(st, 200);
    let cwd = s.cwd.to_str().unwrap();
    assert_eq!(v["argv"], json!(["claude", "--resume", "s1"]));
    assert_eq!(v["cwd"], cwd);
    assert_eq!(v["command"].as_str().unwrap(), format!("cd {} && claude --resume s1", sh_quote(cwd)));
    let (st, v) = req(s.addr, "GET", "/v1/sessions/claude%3A--dangerously-skip-permissions/resume", &[]).await;
    assert_eq!(st, 200);
    assert_eq!((v["supported"].as_bool(), v["reason"].as_str()), (Some(false), Some("会话 id 不合法")));
    assert_eq!(v["argv"], json!([]));
    assert!(v.get("command").is_none());
    assert_eq!(req(s.addr, "GET", "/v1/sessions/claude%3Anope/resume", &[]).await.0, 404);
}

/// Scenario: macOS 打开终端 — the write header, read-only mode, and the exact `open` argv and script.
#[tokio::test]
async fn open_terminal_needs_the_write_header_and_launches_through_open_without_apple_events() {
    let s = start(GuardOptions::default()).await;
    let (st, v) = req(s.addr, "POST", OPEN, &[]).await;
    assert_eq!((st, v["error"].as_str()), (403, Some("missing X-Uniflo-Write: 1")));
    assert_eq!(req(s.addr, "POST", OPEN, &[("X-Uniflo-Write", "yes")]).await.0, 403);
    let evil = [("X-Uniflo-Write", "1"), ("Origin", "https://evil.example")];
    assert_eq!(req(s.addr, "POST", OPEN, &evil).await.0, 403);
    assert_eq!(req(s.addr, "GET", OPEN, &[("X-Uniflo-Write", "1")]).await.0, 405, "POST only");
    assert!(s.calls.lock().unwrap().is_empty());

    let (st, v) = req(s.addr, "POST", &format!("{OPEN}?terminal=terminal"), &[("X-Uniflo-Write", "1")]).await;
    if cfg!(target_os = "macos") {
        assert_eq!(st, 200, "{v}");
        let line = format!("cd {} && claude --resume s1", sh_quote(s.cwd.to_str().unwrap()));
        assert_eq!(v["command"].as_str(), Some(line.as_str()));
        let calls = s.calls.lock().unwrap();
        let [argv] = &calls[..] else { panic!("{calls:?}") };
        assert_eq!(argv[..3], ["open", "-a", "Terminal"]);
        assert!(!argv.iter().any(|a| a.contains("osascript")), "{argv:?}");
        assert_eq!(std::fs::read_to_string(&argv[3]).unwrap(), terminal_script(&line));
        let _ = std::fs::remove_dir_all(std::path::Path::new(&argv[3]).parent().unwrap());
    } else {
        assert_eq!(st, 501);
        assert!(v["command"].as_str().unwrap().ends_with("claude --resume s1"));
    }
    let (st, _) = req(
        s.addr,
        "POST",
        "/v1/sessions/claude%3A--dangerously-skip-permissions/open-terminal",
        &[("X-Uniflo-Write", "1")],
    )
    .await;
    assert_eq!(st, 422, "an invalid id never reaches a terminal");

    let ro = start(GuardOptions { read_only: true, ..Default::default() }).await;
    let (st, v) = req(ro.addr, "POST", OPEN, &[("X-Uniflo-Write", "1")]).await;
    assert_eq!((st, v["error"].as_str()), (403, Some("read-only")));
    assert_eq!(req(ro.addr, "GET", "/v1/sessions/claude%3As1/resume", &[]).await.0, 200, "reads still work");
    assert!(ro.calls.lock().unwrap().is_empty());

    let tok = start(GuardOptions { token: Some("t0k".into()), ..Default::default() }).await;
    assert_eq!(req(tok.addr, "POST", OPEN, &[("X-Uniflo-Write", "1")]).await.0, 403);
    assert!(tok.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn preflight_allows_the_write_header() {
    let s = start(GuardOptions::default()).await;
    let mut st = TcpStream::connect(s.addr).await.unwrap();
    let r = format!(
        "OPTIONS {OPEN} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://localhost:5173\r\nConnection: close\r\n\r\n",
        s.addr.port()
    );
    st.write_all(r.as_bytes()).await.unwrap();
    let mut raw = String::new();
    st.read_to_string(&mut raw).await.unwrap();
    let lower = raw.to_ascii_lowercase();
    assert!(lower.contains("access-control-allow-methods: get, post, delete, options"), "{raw}");
    assert!(lower.contains("x-uniflo-write"), "{raw}");
}
