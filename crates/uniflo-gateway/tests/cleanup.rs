//! Session cleanup over real TCP on synthetic sessions: plan eligibility, archive + trash,
//! archived sessions in list / events / full-text search / usage, refusals, restore from the
//! trash, and the write-endpoint guard. The trash is always a temp directory.

use serde_json::{Value, json};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_adapters::codex::Codex;
use uniflo_adapters::opencode::OpenCode;
use uniflo_adapters::pi::PiFamily;
use uniflo_core::archive::sha256_file;
use uniflo_core::cleanup::trash::{DirTrash, Trash};
use uniflo_core::cleanup::{Cleanup, CleanupOptions, PLAN_TTL};
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::{GuardOptions, Services};
use uniflo_search::fts::{Fts, FtsOptions};

const SID: &str = "5e55c0de-0000-4000-8000-000000000001";
const AGENT: &str = "agent-a1";
const CODEX_ID: &str = "0199a000-0000-7000-8000-00000000c0de";
const PI_STEM: &str = "2026-10-02T11-39-57-523Z_01a0";
const NEEDLE: &str = "缓存击穿归档needle";

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn usage(i: u64, o: u64) -> Value {
    json!({"input_tokens":i,"output_tokens":o,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50})
}

/// Trash that also records, at the moment of each move, whether the cleanup log already held
/// the manifest of every file being moved (with the hash the file has right then).
struct CheckedTrash {
    inner: DirTrash,
    log: PathBuf,
    checks: Mutex<Vec<(PathBuf, bool)>>,
}

impl Trash for CheckedTrash {
    fn trash(&self, path: &Path) -> anyhow::Result<()> {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let manifest: Vec<Value> = log
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == "archived")
            .flat_map(|v| v["files"].as_array().cloned().unwrap_or_default())
            .collect();
        let mut files = Vec::new();
        walk(path, &mut files);
        let ok = !files.is_empty()
            && files.iter().all(|f| {
                manifest.iter().any(|m| {
                    m["path"].as_str() == Some(&f.display().to_string())
                        && m["sha256"].as_str() == sha256_file(f).ok().as_deref()
                })
            });
        self.checks.lock().unwrap().push((path.to_path_buf(), ok));
        self.inner.trash(path)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

fn walk(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        for e in std::fs::read_dir(p).unwrap().flatten() {
            walk(&e.path(), out);
        }
    } else {
        out.push(p.to_path_buf());
    }
}

struct World {
    dir: tempfile::TempDir,
    engine: Arc<Engine>,
    trash: Arc<CheckedTrash>,
    cleanup: Arc<Cleanup>,
    fts: Arc<Fts>,
}

impl World {
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn src(&self) -> PathBuf {
        self.root().join(format!("claude/-w-demo/{SID}.jsonl"))
    }
    fn side_dir(&self) -> PathBuf {
        self.root().join(format!("claude/-w-demo/{SID}"))
    }
    fn key(&self) -> String {
        format!("claude:{SID}")
    }
}

/// Synthetic sessions: a ~2 MB Claude session with a sub-agent directory, a big tool output and
/// base64 images; an OpenCode database session; a working Codex session; a Pi session whose
/// sidecar directory is a symlink.
fn world(ttl: Duration) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let now = now_ms();
    let t = now - 2 * 3_600_000;
    for d in ["claude/-w-demo", "codex", "pi/--w--", "pi-elsewhere", "opencode", "data/pricing", "trash"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("data/pricing/overrides.json"),
        json!([{"id":"model-a","provider":"test","prices":[{"from":0,"input":3.0,"output":15.0,"cache_read":0.3}]}])
            .to_string(),
    )
    .unwrap();

    let png =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==".repeat(3000);
    let big: String = (0..20_000).map(|i| format!("{i:06} cargo build output line with some padding text\n")).collect();
    let mut c = line(json!({"type":"user","uuid":"u1","timestamp":t,"cwd":"/w/demo",
        "message":{"role":"user","content":[{"type":"text","text":format!("请看截图并修复 {NEEDLE}")},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}]}}));
    c += &line(json!({"type":"assistant","uuid":"a1","timestamp":t+1000,"message":{"id":"m1","model":"model-a",
        "content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo build"}}],
        "stop_reason":"tool_use","usage":usage(100,20)}}));
    c += &line(json!({"type":"user","uuid":"r1","timestamp":t+2000,"message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":big},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":png}}]}]}}));
    c += &line(json!({"type":"assistant","uuid":"a2","timestamp":t+3000,"message":{"id":"m2","model":"model-a",
        "content":[{"type":"tool_use","id":"toolu_2","name":"Write","input":{"file_path":"/w/demo/logo.png","content":png}}],
        "stop_reason":"tool_use","usage":usage(200,30)}}));
    c += &line(json!({"type":"user","uuid":"r2","timestamp":t+4000,"message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_2","content":"written"}]}}));
    c += &line(json!({"type":"assistant","uuid":"a3","timestamp":t+5000,"message":{"id":"m3","model":"model-a",
        "content":[{"type":"text","text":"修好了：缓存击穿已处理"}],"stop_reason":"end_turn","usage":usage(300,40)}}));
    std::fs::write(root.join(format!("claude/-w-demo/{SID}.jsonl")), c).unwrap();
    let sub = root.join(format!("claude/-w-demo/{SID}/subagents"));
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(root.join(format!("claude/-w-demo/{SID}/tool-results"))).unwrap();
    std::fs::write(root.join(format!("claude/-w-demo/{SID}/tool-results/hook-1.txt")), "hook out").unwrap();
    let mut a = line(json!({"type":"user","uuid":"su1","timestamp":t+1500,"isSidechain":true,"cwd":"/w/demo",
        "message":{"role":"user","content":"子代理任务：检查锁"}}));
    a += &line(json!({"type":"assistant","uuid":"sa1","timestamp":t+1600,"isSidechain":true,"message":{"id":"sm1",
        "model":"model-a","content":[{"type":"text","text":"锁没问题"}],"stop_reason":"end_turn","usage":usage(7,3)}}));
    std::fs::write(sub.join(format!("{AGENT}.jsonl")), a).unwrap();
    std::fs::write(sub.join(format!("{AGENT}.meta.json")), "{}").unwrap();

    let ev = |ty: &str, ts: i64, p: Value| line(json!({"timestamp":ts,"type":ty,"payload":p}));
    let x = ev("session_meta", now - 5000, json!({"id":CODEX_ID,"cwd":"/w/x","timestamp":now - 5000}))
        + &ev("event_msg", now - 4000, json!({"type":"task_started","turn_id":"t1"}))
        + &ev(
            "response_item",
            now - 3000,
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}),
        );
    std::fs::write(root.join(format!("codex/rollout-2026-10-09T10-00-00-{CODEX_ID}.jsonl")), x).unwrap();

    let p = line(json!({"type":"session","version":3,"id":"01a0","timestamp":t,"cwd":"/w/pi"}))
        + &line(
            json!({"type":"message","id":"u1","timestamp":t,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
        )
        + &line(
            json!({"type":"message","id":"a1","timestamp":t+1,"message":{"role":"assistant","content":[{"type":"text","text":"yo"}],"stopReason":"stop"}}),
        );
    std::fs::write(root.join(format!("pi/--w--/{PI_STEM}.jsonl")), p).unwrap();
    std::os::unix::fs::symlink(root.join("pi-elsewhere"), root.join(format!("pi/--w--/{PI_STEM}"))).unwrap();

    let db = root.join("opencode/opencode.db");
    let w = rusqlite::Connection::open(&db).unwrap();
    w.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT, directory TEXT, title TEXT,
           version TEXT, time_created INTEGER, time_updated INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
    )
    .unwrap();
    w.execute("INSERT INTO session VALUES ('ses_1','p',NULL,'s','/w/oc','OC',NULL,?1,?1)", [t]).unwrap();
    drop(w);

    let adapters: Vec<Arc<dyn Adapter>> = vec![
        Arc::new(JsonlAdapter::new(ClaudeFamily {
            info: HarnessInfo { id: "claude", name: "Claude Code" },
            roots: vec![root.join("claude")],
            live_dir: None,
        })),
        Arc::new(OpenCode { info: HarnessInfo { id: "opencode", name: "OpenCode" }, db }),
        Arc::new(JsonlAdapter::new(Codex { roots: vec![root.join("codex")] })),
        Arc::new(JsonlAdapter::new(PiFamily::new(HarnessInfo { id: "pi", name: "Pi" }, vec![root.join("pi")], None))),
    ];
    let opts = EngineOptions {
        cache_path: None,
        hot_poll: Duration::from_millis(50),
        data_dir: Some(root.join("data")),
        ..Default::default()
    };
    let engine = Engine::new(adapters, opts);
    engine.index();
    let fts = Fts::start(
        engine.clone(),
        FtsOptions {
            path: root.join("fts.sqlite"),
            pause: 0.0,
            reconcile: Duration::from_millis(300),
            ..Default::default()
        },
    )
    .unwrap();
    let trash = Arc::new(CheckedTrash {
        inner: DirTrash(root.join("trash")),
        log: root.join("data/archive/cleanup.log.jsonl"),
        checks: Mutex::new(Vec::new()),
    });
    let cleanup =
        Arc::new(Cleanup::new(engine.clone(), CleanupOptions { trash: trash.clone(), plan_ttl: ttl }).unwrap());
    World { dir, engine, trash, cleanup, fts: Arc::new(fts) }
}

async fn serve(w: &World, guard: GuardOptions) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let services = Services { fts: Some(w.fts.clone()), cleanup: Some(w.cleanup.clone()) };
    let router = uniflo_gateway::router_with(w.engine.clone(), guard, services);
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));
    addr
}

const WRITE: &[(&str, &str)] = &[("X-Uniflo-Write", "1")];

async fn call(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let host = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map_or(format!("127.0.0.1:{}", addr.port()), |(_, v)| v.to_string());
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("host")) {
        req += &format!("{k}: {v}\r\n");
    }
    if !body.is_empty() {
        req += &format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len());
    } else if method != "GET" {
        req += "Content-Length: 0\r\n";
    }
    req += "\r\n";
    req += &body;
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null))
}

async fn get(addr: std::net::SocketAddr, path: &str) -> Value {
    let (st, v) = call(addr, "GET", path, &[], None).await;
    assert_eq!(st, 200, "{path}: {v}");
    v
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn plan(addr: std::net::SocketAddr, keys: &[String]) -> Value {
    let (st, v) = call(addr, "POST", "/v1/cleanup/plan", WRITE, Some(json!({ "sessions": keys }))).await;
    assert_eq!(st, 200, "{v}");
    v
}

async fn until<F: std::future::Future<Output = bool>>(
    within: Duration,
    what: &str,
    mut f: impl FnMut() -> F,
) -> Duration {
    let t0 = Instant::now();
    while !f().await {
        assert!(t0.elapsed() < within, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    t0.elapsed()
}

fn keys_of(list: &Value) -> Vec<String> {
    list.as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap().to_owned()).collect()
}

fn archive_files(w: &World) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if w.root().join("data/archive").is_dir() {
        walk(&w.root().join("data/archive"), &mut v);
    }
    v.retain(|p| p.to_string_lossy().ends_with(".jsonl.zst"));
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_judges_each_kind_of_session() {
    let w = world(PLAN_TTL);
    let addr = serve(&w, GuardOptions::default()).await;
    let keys: Vec<String> = [
        w.key(),
        "opencode:ses_1".into(),
        format!("codex:{CODEX_ID}"),
        "pi:01a0".into(),
        format!("claude:{AGENT}"),
        "claude:nope".into(),
    ]
    .to_vec();
    let p = plan(addr, &keys).await;
    let by: std::collections::HashMap<String, Value> =
        p["sessions"].as_array().unwrap().iter().map(|c| (c["key"].as_str().unwrap().to_owned(), c.clone())).collect();
    assert_eq!(p["sessions"].as_array().unwrap().len(), 6, "one entry per requested key");

    let root = &by[&w.key()];
    assert_eq!(root["eligible"], true, "{root}");
    assert_eq!(root["children"], json!([format!("claude:{AGENT}")]));
    let paths: Vec<&str> = root["targets"].as_array().unwrap().iter().map(|t| t["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec![w.src().display().to_string(), w.side_dir().display().to_string()]);
    let side = &root["targets"][1];
    assert_eq!((side["dir"].as_bool(), side["files"].as_u64()), (Some(true), Some(3)), "sub-agent dir, meta, hook");
    let src_len = std::fs::metadata(w.src()).unwrap().len();
    assert!(src_len > 1_800_000, "about 2 MB: {src_len}");
    assert!(root["targets"][0]["file_id"].as_u64().is_some() && root["targets"][0]["mtime_ms"].as_i64() > Some(0));
    assert_eq!(root["targets"][0]["bytes"].as_u64(), Some(src_len));

    let reason = |k: &str| by[k]["reason"].as_str().map(str::to_owned);
    assert_eq!(reason("opencode:ses_1").as_deref(), Some("unsupported"));
    assert_eq!(reason(&format!("codex:{CODEX_ID}")).as_deref(), Some("working"));
    assert_eq!(reason("pi:01a0").as_deref(), Some("symlink"));
    assert_eq!(reason(&format!("claude:{AGENT}")).as_deref(), Some("subagent"));
    assert_eq!(reason("claude:nope").as_deref(), Some("unknown_session"));
    assert_eq!(by["opencode:ses_1"]["message"], "不支持清理");
    assert_eq!(by[&format!("codex:{CODEX_ID}")]["message"], "会话运行中");
    assert!(by["pi:01a0"]["message"].as_str().unwrap().starts_with("目标是符号链接"));
    assert!(by[&format!("claude:{AGENT}")]["message"].as_str().unwrap().starts_with("需随父会话一起清理"));
    assert!(by.values().filter(|c| c["key"] != w.key()).all(|c| c["eligible"] == false));

    let total: u64 = root["targets"].as_array().unwrap().iter().map(|t| t["bytes"].as_u64().unwrap()).sum();
    assert_eq!(p["freed_bytes"].as_u64(), Some(total));
    assert!(p["archive_bytes"].as_u64().unwrap() > 0 && p["archive_bytes"].as_u64().unwrap() < total);
    assert!(p["plan_id"].as_str().unwrap().len() >= 16);
    assert_eq!(p["expires_at"].as_i64().unwrap() - p["created_at"].as_i64().unwrap(), 600_000);
    assert!(w.src().is_file() && archive_files(&w).is_empty(), "planning changes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execute_archives_then_trashes_and_the_archive_stays_usable() {
    let w = world(PLAN_TTL);
    tokio::spawn(w.engine.clone().run());
    let addr = serve(&w, GuardOptions::default()).await;
    let (key, sub) = (w.key(), format!("claude:{AGENT}"));
    until(Duration::from_secs(15), "usage index", || async {
        get(addr, "/v1/usage").await["indexing"]["ready"] == true
    })
    .await;
    assert!(w.fts.wait_idle(Duration::from_secs(15)));
    let before_usage = [
        get(addr, &format!("/v1/sessions/{}/usage", enc(&key))).await["totals"].clone(),
        get(addr, &format!("/v1/sessions/{}/usage", enc(&sub))).await["totals"].clone(),
    ];
    assert!(before_usage[0]["cost_usd"].as_f64().unwrap() > 0.0 && before_usage[0]["steps"] == 3);
    let before_rows = get(addr, "/v1/usage?group_by=session").await["rows"].clone();
    let before_events = get(addr, &format!("/v1/sessions/{}/events?limit=1000&max_text=0", enc(&key))).await;
    let hit = get(addr, &format!("/v1/search?q={}", enc(NEEDLE))).await;
    assert_eq!(hit["results"][0]["session"], key.as_str());

    let mut files = Vec::new();
    walk(&w.src(), &mut files);
    walk(&w.side_dir(), &mut files);
    let source_bytes: u64 = files.iter().map(|f| std::fs::metadata(f).unwrap().len()).sum();
    let hashes: Vec<(PathBuf, String)> = files.iter().map(|f| (f.clone(), sha256_file(f).unwrap())).collect();

    let p = plan(addr, std::slice::from_ref(&key)).await;
    let id = p["plan_id"].as_str().unwrap();
    let (st, r) = call(addr, "POST", &format!("/v1/cleanup/plans/{id}/execute"), WRITE, None).await;
    assert_eq!(st, 200, "{r}");
    let res = &r["results"][0];
    assert_eq!((res["key"].as_str(), res["status"].as_str()), (Some(key.as_str()), Some("archived")), "{r}");

    // Into the injected trash, byte for byte, with the manifest logged before each move.
    assert!(!w.src().exists() && !w.side_dir().exists());
    let log = std::fs::read_to_string(w.root().join("data/archive/cleanup.log.jsonl")).unwrap();
    let manifest: Value =
        log.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).find(|v| v["event"] == "archived").unwrap();
    for (f, h) in &hashes {
        let moved = w.trash.inner.slot(f, 0);
        assert_eq!(&sha256_file(&moved).unwrap(), h, "{}", f.display());
        let rec = manifest["files"].as_array().unwrap().iter().find(|m| m["path"] == f.display().to_string()).unwrap();
        assert_eq!(rec["sha256"].as_str(), Some(h.as_str()));
    }
    let checks = w.trash.checks.lock().unwrap().clone();
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|(_, ok)| *ok), "log flushed before every move: {checks:?}");

    // Results match the disk; archives stay under 20% of the source.
    let archives = archive_files(&w);
    assert_eq!(archives.len(), 2, "the session and its sub-agent");
    let archive_bytes: u64 = archives.iter().map(|f| std::fs::metadata(f).unwrap().len()).sum();
    assert_eq!(res["freed_bytes"].as_u64(), Some(source_bytes));
    assert_eq!(res["archive_bytes"].as_u64(), Some(archive_bytes));
    assert_eq!((r["freed_bytes"].as_u64(), r["archive_bytes"].as_u64()), (Some(source_bytes), Some(archive_bytes)));
    assert!(archive_bytes * 5 <= source_bytes, "archive {archive_bytes} > 20% of {source_bytes}");
    eprintln!("archive {archive_bytes} B of source {source_bytes} B");
    assert_eq!(res["children"], json!([sub]));

    // Listed as archived, idle, once.
    let all = get(addr, "/v1/sessions?limit=1000").await;
    assert_eq!(keys_of(&all).iter().filter(|k| **k == key).count(), 1);
    let s = get(addr, &format!("/v1/sessions/{}", enc(&key))).await;
    assert_eq!((s["archived"].as_bool(), s["status"].as_str()), (Some(true), Some("idle")));
    assert!(s["source"].as_str().unwrap().ends_with(".jsonl.zst"));
    assert_eq!(get(addr, &format!("/v1/sessions/{}", enc(&sub))).await["archived"], true);
    let mut arch = keys_of(&get(addr, &format!("/v1/sessions?q={}", enc("is:archived"))).await);
    arch.sort();
    assert_eq!(arch, {
        let mut v = vec![key.clone(), sub.clone()];
        v.sort();
        v
    });
    let rest = keys_of(&get(addr, &format!("/v1/sessions?q={}&limit=1000", enc("!is:archived"))).await);
    assert!(!rest.contains(&key) && !rest.contains(&sub) && rest.contains(&"pi:01a0".to_string()));

    // Events come from the archive: same ids, long ones cut and flagged.
    let ev = get(addr, &format!("/v1/sessions/{}/events?limit=1000&max_text=0", enc(&key))).await;
    let ids = |v: &Value| v["events"].as_array().unwrap().iter().map(|e| e["id"].clone()).collect::<Vec<_>>();
    assert_eq!(ids(&ev), ids(&before_events));
    let out = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["call_id"] == "toolu_1" && e["kind"] == "tool_result")
        .unwrap();
    assert!(out["truncated"] == true && out["output"].as_str().unwrap().len() <= 2048);
    let write =
        ev["events"].as_array().unwrap().iter().find(|e| e["kind"] == "tool_call" && e["name"] == "Write").unwrap();
    assert!(write["truncated"] == true && write.to_string().len() < 4096, "base64 argument dropped");
    assert!(ev["events"].as_array().unwrap().iter().all(|e| e.to_string().len() < 20_000));
    let paged = get(addr, &format!("/v1/sessions/{}/events?limit=2", enc(&key))).await;
    assert_eq!(paged["events"].as_array().unwrap().len(), 2);
    assert!(paged["next_before"].as_u64().is_some());

    // Full-text: the existing index keeps the archived session, a fresh one reads the archive.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(w.fts.wait_idle(Duration::from_secs(10)));
    let hit = get(addr, &format!("/v1/search?q={}", enc(NEEDLE))).await;
    assert_eq!(hit["results"][0]["session"], key.as_str(), "{hit}");
    let fresh = Fts::start(
        w.engine.clone(),
        FtsOptions { path: w.root().join("fts2.sqlite"), pause: 0.0, ..Default::default() },
    )
    .unwrap();
    assert!(fresh.wait_idle(Duration::from_secs(10)));
    let params = uniflo_search::fts::SearchParams { q: "锁没问题".into(), ..Default::default() };
    assert_eq!(fresh.search(&params).unwrap().results[0].session, sub);

    // Usage totals unchanged.
    let after_usage = [
        get(addr, &format!("/v1/sessions/{}/usage", enc(&key))).await["totals"].clone(),
        get(addr, &format!("/v1/sessions/{}/usage", enc(&sub))).await["totals"].clone(),
    ];
    assert_eq!(after_usage, before_usage);
    assert_eq!(get(addr, "/v1/usage?group_by=session").await["rows"], before_rows);
    assert_eq!(s["usage"], before_usage[0]);

    // Archive management lists both; the root carries the tree's size.
    let list = get(addr, "/v1/archive").await;
    assert_eq!(list["archives"].as_array().unwrap().len(), 2);
    assert_eq!(list["bytes"].as_u64(), Some(archive_bytes));
    let root = list["archives"].as_array().unwrap().iter().find(|a| a["key"] == key.as_str()).unwrap();
    assert_eq!(root["source_bytes"].as_u64(), Some(source_bytes));
    assert_eq!(root["source"], w.src().display().to_string());

    // A second run of the same plan is refused.
    let (st, _) = call(addr, "POST", &format!("/v1/cleanup/plans/{id}/execute"), WRITE, None).await;
    assert_eq!(st, 404);

    // Deleting the root archive drops both sessions.
    let (st, d) = call(addr, "DELETE", &format!("/v1/archive/{}", enc(&key)), WRITE, None).await;
    assert_eq!(st, 200, "{d}");
    assert_eq!(d["removed"].as_array().unwrap().len(), 2);
    assert!(archive_files(&w).is_empty());
    let left = keys_of(&get(addr, "/v1/sessions?limit=1000").await);
    assert!(!left.contains(&key) && !left.contains(&sub));
    let (st, _) = call(addr, "DELETE", &format!("/v1/archive/{}", enc(&key)), WRITE, None).await;
    assert_eq!(st, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_source_and_expired_plans_are_refused() {
    let w = world(Duration::from_millis(400));
    let addr = serve(&w, GuardOptions::default()).await;
    let key = w.key();
    let p = plan(addr, std::slice::from_ref(&key)).await;
    let size = std::fs::metadata(w.src()).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(w.src())
        .unwrap()
        .write_all(json!({"type":"summary","summary":"later"}).to_string().as_bytes())
        .unwrap();
    let (st, r) =
        call(addr, "POST", &format!("/v1/cleanup/plans/{}/execute", p["plan_id"].as_str().unwrap()), WRITE, None).await;
    assert_eq!(st, 200);
    let res = &r["results"][0];
    assert_eq!((res["status"].as_str(), res["reason"].as_str()), (Some("failed"), Some("source_changed")), "{r}");
    assert_eq!(res["message"], "源文件已变化");
    assert!(w.src().is_file() && std::fs::metadata(w.src()).unwrap().len() > size, "not moved");
    assert!(w.side_dir().is_dir());
    assert!(archive_files(&w).is_empty(), "no archive written");
    assert!(w.trash.checks.lock().unwrap().is_empty());
    assert_eq!(get(addr, &format!("/v1/sessions/{}", enc(&key))).await["archived"], Value::Null);

    let p = plan(addr, std::slice::from_ref(&key)).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (st, r) =
        call(addr, "POST", &format!("/v1/cleanup/plans/{}/execute", p["plan_id"].as_str().unwrap()), WRITE, None).await;
    assert_eq!(st, 410, "{r}");
    assert!(w.src().is_file());
    let (st, _) = call(addr, "POST", "/v1/cleanup/plans/0000/execute", WRITE, None).await;
    assert_eq!(st, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restoring_from_the_trash_brings_the_source_back() {
    let w = world(PLAN_TTL);
    // Daemon defaults (30 s rescan): the restore must show up through the file watcher.
    tokio::spawn(w.engine.clone().run());
    let addr = serve(&w, GuardOptions::default()).await;
    let (key, sub) = (w.key(), format!("claude:{AGENT}"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let p = plan(addr, std::slice::from_ref(&key)).await;
    let (_, r) =
        call(addr, "POST", &format!("/v1/cleanup/plans/{}/execute", p["plan_id"].as_str().unwrap()), WRITE, None).await;
    assert_eq!(r["results"][0]["status"], "archived", "{r}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        get(addr, &format!("/v1/sessions/{}", enc(&key))).await["archived"],
        true,
        "a stale event does not resurrect it"
    );

    std::fs::rename(w.trash.inner.slot(&w.src(), 0), w.src()).unwrap();
    std::fs::rename(w.trash.inner.slot(&w.side_dir(), 0), w.side_dir()).unwrap();
    let took = until(Duration::from_secs(30), "restored session listed as a source", || async {
        let s = get(addr, &format!("/v1/sessions/{}", enc(&key))).await;
        s["archived"].is_null() && s["source"] == w.src().display().to_string()
    })
    .await;
    eprintln!("restored source listed after {} ms", took.as_millis());
    let all = keys_of(&get(addr, "/v1/sessions?limit=1000").await);
    assert_eq!(all.iter().filter(|k| **k == key).count(), 1, "listed once");
    let ev = get(addr, &format!("/v1/sessions/{}/events?limit=1000&max_text=0", enc(&key))).await;
    let out = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["call_id"] == "toolu_1" && e["kind"] == "tool_result")
        .unwrap();
    assert!(out["output"].as_str().unwrap().len() > 100_000, "served from the source again");

    let list = get(addr, "/v1/archive").await;
    let root = list["archives"].as_array().unwrap().iter().find(|a| a["key"] == key.as_str()).unwrap();
    assert_eq!(root["restored"], true);
    assert_eq!(archive_files(&w).len(), 2, "archive kept until the user deletes it");
    until(Duration::from_secs(35), "sub-agent back as a source", || async {
        get(addr, &format!("/v1/sessions/{}", enc(&sub))).await["archived"].is_null()
    })
    .await;
    let u = get(addr, &format!("/v1/sessions/{}/usage", enc(&key))).await;
    assert_eq!(u["totals"]["steps"], 3, "not double counted with the archive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_endpoints_need_every_condition() {
    let w = world(PLAN_TTL);
    let body = || Some(json!({"sessions": [format!("claude:{SID}")]}));
    let open = serve(&w, GuardOptions { cors_origins: vec!["*".into()], ..Default::default() }).await;
    let (st, v) =
        call(open, "POST", "/v1/cleanup/plan", &[("X-Uniflo-Write", "1"), ("Origin", "https://evil.example")], body())
            .await;
    assert_eq!(st, 403, "foreign origin, even with --cors-origin '*': {v}");
    let (st, _) = call(open, "POST", "/v1/cleanup/plan", &[], body()).await;
    assert_eq!(st, 403, "missing X-Uniflo-Write");
    let (st, v) =
        call(open, "POST", "/v1/cleanup/plan", &[("X-Uniflo-Write", "1"), ("Origin", "http://127.0.0.1:5173")], body())
            .await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = call(open, "POST", "/v1/cleanup/plan", WRITE, body()).await;
    assert_eq!(st, 200, "{v}");
    let (st, _) =
        call(open, "POST", "/v1/cleanup/plan", &[("X-Uniflo-Write", "1"), ("Host", "lan.example")], body()).await;
    assert_eq!(st, 403);
    assert_eq!(get(open, "/v1/health").await["read_only"], false);

    let tok = serve(&w, GuardOptions { token: Some("s3cret".into()), ..Default::default() }).await;
    let (st, _) = call(tok, "POST", "/v1/cleanup/plan", WRITE, body()).await;
    assert_eq!(st, 403, "token configured, none sent");
    let (st, _) =
        call(tok, "POST", "/v1/cleanup/plan", &[("X-Uniflo-Write", "1"), ("Authorization", "Bearer s3cret")], body())
            .await;
    assert_eq!(st, 200);

    let ro = serve(&w, GuardOptions { read_only: true, ..Default::default() }).await;
    let (st, v) = call(ro, "POST", "/v1/cleanup/plan", WRITE, body()).await;
    assert_eq!(st, 403, "{v}");
    let (st, _) = call(ro, "DELETE", &format!("/v1/archive/{}", enc(&w.key())), WRITE, None).await;
    assert_eq!(st, 403);
    assert_eq!(get(ro, "/v1/health").await["read_only"], true);
    assert_eq!(get(ro, "/v1/archive").await["archives"], json!([]), "reads still work");
}
