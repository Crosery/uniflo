//! The real `uniflo` binary over a synthetic `UNIFLO_HOME` with an injected trash directory:
//! `clean --dry-run`, refusal without `--yes`, `clean --yes` against the daemon and in-process,
//! `archive ls` / `archive rm`, and a `--read-only` daemon.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");
const SID: &str = "c1ea0000-0000-4000-8000-000000000001";
const KEY: &str = "claude:c1ea0000-0000-4000-8000-000000000001";

fn line(v: Value) -> String {
    format!("{v}\n")
}

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-w-demo");
        std::fs::create_dir_all(proj.join(format!("{SID}/subagents"))).unwrap();
        let t = 1_790_000_000_000i64;
        let big = "build log line\n".repeat(40_000);
        let s = line(
            json!({"type":"user","uuid":"u1","timestamp":t,"cwd":"/w/demo","message":{"role":"user","content":"清理测试"}}),
        ) + &line(json!({"type":"assistant","uuid":"a1","timestamp":t+1,"message":{"id":"m1","model":"m",
                "content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"make"}}],"stop_reason":"tool_use",
                "usage":{"input_tokens":10,"output_tokens":2}}}))
            + &line(json!({"type":"user","uuid":"r1","timestamp":t+2,"message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu1","content":big}]}}))
            + &line(json!({"type":"assistant","uuid":"a2","timestamp":t+3,"message":{"id":"m2","model":"m",
                "content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}));
        std::fs::write(proj.join(format!("{SID}.jsonl")), s).unwrap();
        let a = line(
            json!({"type":"user","uuid":"x1","timestamp":t+1,"isSidechain":true,"message":{"role":"user","content":"sub"}}),
        );
        std::fs::write(proj.join(format!("{SID}/subagents/agent-q1.jsonl")), a).unwrap();
        Home { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn src(&self) -> PathBuf {
        self.path().join(format!(".claude/projects/-w-demo/{SID}.jsonl"))
    }

    fn trash(&self) -> PathBuf {
        self.path().join("trash")
    }

    fn source_bytes(&self) -> u64 {
        let side = self.path().join(format!(".claude/projects/-w-demo/{SID}/subagents/agent-q1.jsonl"));
        std::fs::metadata(self.src()).unwrap().len() + std::fs::metadata(side).unwrap().len()
    }

    fn archives(&self) -> Vec<PathBuf> {
        find(self.path(), ".jsonl.zst")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("UNIFLO_HOME", self.path())
            .env("UNIFLO_TRASH_DIR", self.trash())
            .env_remove("UNIFLO_TOKEN")
            .env_remove("UNIFLO_DATA_DIR")
            .env_remove("UNIFLO_CACHE_DIR")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

fn find(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(find(&p, suffix));
        } else if p.to_string_lossy().ends_with(suffix) {
            out.push(p);
        }
    }
    out
}

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn daemon(home: &Home, extra: &[&str]) -> Daemon {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bind = format!("127.0.0.1:{port}");
    let mut args = vec!["daemon", "--bind", &bind, "--no-update-check", "--no-price-sync", "--no-fts"];
    args.extend_from_slice(extra);
    let child = home.cmd(&args).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let d = Daemon { child, port };
    let t0 = Instant::now();
    while get(port, "/v1/health").map(|(s, _)| s) != Some(200) {
        assert!(t0.elapsed() < Duration::from_secs(20), "daemon did not come up");
        std::thread::sleep(Duration::from_millis(50));
    }
    d
}

fn get(port: u16, path: &str) -> Option<(u16, Value)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null)))
}

/// Scenario: CLI clean 与 archive — against a running daemon (the CLI goes through REST).
#[test]
fn clean_and_archive_through_the_daemon() {
    let home = Home::new();
    let before = std::fs::read(home.src()).unwrap();
    let bytes = home.source_bytes();
    let d = daemon(&home, &[]);
    let url = format!("http://127.0.0.1:{}", d.port);

    let out = home.ok(&["--url", &url, "clean", KEY, "--dry-run"]);
    assert!(out.contains(KEY) && out.contains("1 of 1 session(s) can be cleaned"), "{out}");
    let plan: Value =
        serde_json::from_str(&home.ok(&["--url", &url, "clean", &SID[..8], "--dry-run", "--json"])).unwrap();
    assert_eq!(plan["sessions"][0]["key"], KEY, "an id prefix resolves to the key");
    assert_eq!(plan["freed_bytes"].as_u64(), Some(bytes));
    assert_eq!(std::fs::read(home.src()).unwrap(), before, "dry run changes nothing");
    assert!(home.archives().is_empty() && !home.trash().exists());

    let refused = home.run(&["--url", &url, "clean", KEY]);
    assert!(!refused.status.success(), "no terminal and no --yes");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    assert!(home.src().is_file());

    let report: Value = serde_json::from_str(&home.ok(&["--url", &url, "clean", KEY, "--yes", "--json"])).unwrap();
    let r = &report["results"][0];
    assert_eq!((r["key"].as_str(), r["status"].as_str()), (Some(KEY), Some("archived")), "{report}");
    let archived: u64 = home.archives().iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
    assert_eq!((r["freed_bytes"].as_u64(), r["archive_bytes"].as_u64()), (Some(bytes), Some(archived)));
    assert!(!home.src().exists() && find(&home.trash(), ".jsonl").len() == 2);
    let (_, rest) = get(d.port, "/v1/archive").unwrap();
    assert_eq!(rest["bytes"].as_u64(), Some(archived), "the CLI result is the REST result");
    let (_, s) = get(d.port, &format!("/v1/sessions/{}", KEY.replace(':', "%3A"))).unwrap();
    assert_eq!(s["archived"], true);

    let ls = home.ok(&["--url", &url, "archive", "ls"]);
    assert!(ls.lines().any(|l| l.starts_with(KEY) && l.contains("KB")), "{ls}");
    let lsj: Value = serde_json::from_str(&home.ok(&["--url", &url, "archive", "--json"])).unwrap();
    assert_eq!(lsj, rest);

    let rm = home.ok(&["--url", &url, "archive", "rm", KEY]);
    assert!(rm.contains("deleted 2 archive(s)"), "{rm}");
    assert!(home.archives().is_empty());
    let list: Value = serde_json::from_str(&home.ok(&["--url", &url, "ls", "--json", "-n", "100"])).unwrap();
    assert!(list.as_array().unwrap().iter().all(|s| s["key"] != KEY), "{list}");
    let gone = home.run(&["--url", &url, "archive", "rm", KEY]);
    assert!(!gone.status.success());
}

/// The same flow without a daemon, and a `--read-only` daemon refusing writes.
#[test]
fn clean_in_process_and_read_only_daemon() {
    let home = Home::new();
    let refused = home.run(&["--local", "clean", KEY]);
    assert!(!refused.status.success());
    let report: Value = serde_json::from_str(&home.ok(&["--local", "clean", KEY, "--yes", "--json"])).unwrap();
    assert_eq!(report["results"][0]["status"], "archived", "{report}");
    let ls: Value = serde_json::from_str(&home.ok(&["--local", "archive", "ls", "--json"])).unwrap();
    assert_eq!(ls["archives"].as_array().unwrap().len(), 2);
    assert_eq!(ls["bytes"], report["archive_bytes"]);
    let s: Value = serde_json::from_str(&home.ok(&["--local", "ls", "--json", "is:archived"])).unwrap();
    assert_eq!(s.as_array().unwrap().len(), 2, "archived sessions are listed: {s}");

    let d = daemon(&home, &["--read-only"]);
    let url = format!("http://127.0.0.1:{}", d.port);
    assert_eq!(get(d.port, "/v1/health").unwrap().1["read_only"], true);
    let denied = home.run(&["--url", &url, "archive", "rm", KEY]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("403"), "{}", String::from_utf8_lossy(&denied.stderr));
    assert_eq!(home.archives().len(), 2);

    // Without an injected trash directory a sandboxed UNIFLO_HOME never reaches the real trash.
    let other = Home::new();
    let out = other.cmd(&["--local", "clean", KEY, "--yes", "--json"]).env_remove("UNIFLO_TRASH_DIR").output().unwrap();
    assert!(!out.status.success());
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["results"][0]["reason"], "trash_failed", "{r}");
    assert!(other.src().is_file() && other.archives().is_empty(), "nothing moved, archive discarded");
}
