//! The real `uniflo` binary over a synthetic `UNIFLO_HOME`: `grep` against the daemon and
//! in-process, and `daemon --no-fts`.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");

fn user(uuid: &str, ts: i64, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":"/w/demo","message":{"role":"user","content":text}})
    )
}

fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude/projects/-w-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let t = 1_790_000_000_000;
    std::fs::write(
        dir.join("s1.jsonl"),
        [
            user("u1", t, "请帮我修复缓存击穿问题"),
            format!("{}\n", json!({"type":"ai-title","aiTitle":"缓存治理"})),
            user("u2", t + 10, "缓存击穿之后再看雪崩"),
        ]
        .concat(),
    )
    .unwrap();
    std::fs::write(dir.join("s2.jsonl"), user("v1", t + 20, "另一个会话也提到缓存击穿")).unwrap();
    std::fs::write(dir.join("s3.jsonl"), user("w1", t + 30, "unrelated")).unwrap();
    home
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
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

fn daemon(home: &Path, extra: &[&str]) -> Daemon {
    let port = free_port();
    let child = Command::new(BIN)
        .args(["daemon", "--bind", &format!("127.0.0.1:{port}"), "--no-update-check"])
        .args(extra)
        .env("UNIFLO_HOME", home)
        .env_remove("UNIFLO_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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

fn uniflo(home: &Path, args: &[&str]) -> (String, String) {
    let out = Command::new(BIN).args(args).env("UNIFLO_HOME", home).env_remove("UNIFLO_TOKEN").output().unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

/// (session, event ids) pairs of a search response, in order.
fn hits(v: &Value) -> Vec<(String, Vec<String>)> {
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let ids = s["hits"].as_array().unwrap().iter().map(|h| h["event"].as_str().unwrap().to_owned()).collect();
            (s["session"].as_str().unwrap().to_owned(), ids)
        })
        .collect()
}

fn find(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(find(&p, name));
        } else if p.file_name().is_some_and(|n| n == name) {
            out.push(p);
        }
    }
    out
}

const Q: &str = "/v1/search?q=%E7%BC%93%E5%AD%98%E5%87%BB%E7%A9%BF";

/// Scenario: CLI grep.
#[test]
fn grep_matches_rest_and_renders_sessions() {
    let home = home();
    let d = daemon(home.path(), &[]);
    let url = format!("http://127.0.0.1:{}", d.port);
    let t0 = Instant::now();
    let rest = loop {
        let (st, v) = get(d.port, Q).unwrap();
        assert_eq!(st, 200, "{v}");
        if v["indexing"] == false {
            break v;
        }
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(50));
    };
    let want = vec![
        ("claude:s2".to_owned(), vec!["v1".to_owned()]),
        ("claude:s1".to_owned(), vec!["u2".to_owned(), "u1".to_owned()]),
    ];
    let mut sorted = hits(&rest);
    sorted.iter_mut().for_each(|(_, ids)| ids.sort_by(|a, b| b.cmp(a)));
    sorted.sort();
    let mut want_sorted = want.clone();
    want_sorted.sort();
    assert_eq!(sorted, want_sorted, "{rest}");

    let (out, _) = uniflo(home.path(), &["--url", &url, "grep", "缓存击穿", "--json"]);
    let cli: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(hits(&cli), hits(&rest), "CLI --json and REST agree on sessions and event ids");

    let (out, err) = uniflo(home.path(), &["--url", &url, "grep", "缓存击穿"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2 + 3, "a title line per session plus one line per hit:\n{out}");
    let s1 = lines.iter().position(|l| l.contains("claude:s1")).unwrap();
    assert!(lines[s1].contains("缓存治理"), "{out}");
    assert!(lines[s1 + 1].starts_with("  ") && lines[s1 + 1].contains("缓存击穿"), "{out}");
    assert!(!out.contains('\u{2}') && !out.contains('\x1b'), "not a terminal: plain text");
    assert!(err.contains("2 of 2 matching sessions"), "{err}");

    let (out, _) = uniflo(home.path(), &["--url", &url, "grep", "缓存击穿", "--filter", "id:s2", "--json"]);
    assert_eq!(hits(&serde_json::from_str(&out).unwrap()), vec![("claude:s2".to_owned(), vec!["v1".to_owned()])]);
    assert_eq!(find(home.path(), "fts-v1.sqlite").len(), 1, "index lives under UNIFLO_HOME's cache dir");
    drop(d);

    // No daemon: the in-process path opens the same index and answers identically.
    let (out, _) = uniflo(home.path(), &["--url", "http://127.0.0.1:9", "grep", "缓存击穿", "--json"]);
    assert_eq!(hits(&serde_json::from_str(&out).unwrap()), hits(&rest));
}

/// Scenario: 后台构建、格式升级与关闭 — `--no-fts`.
#[test]
fn no_fts_answers_503_and_creates_no_index() {
    let home = home();
    let d = daemon(home.path(), &["--no-fts"]);
    let (st, v) = get(d.port, Q).unwrap();
    assert_eq!(st, 503);
    assert!(v["error"].as_str().unwrap().contains("--no-fts"), "{v}");
    let (st, s) = get(d.port, "/v1/stats").unwrap();
    assert!(st == 200 && s["fts"].is_null());
    drop(d);
    assert!(find(home.path(), "fts-v1.sqlite").is_empty());
}
