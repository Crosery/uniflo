//! `/v1/usage`, `/v1/sessions/{key}/usage`, `/v1/models`, `/v1/pricing` over real TCP with
//! the Claude and Codex adapters on synthetic files.

use serde_json::{Value, json};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_adapters::codex::Codex;
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::GuardOptions;

const DAY: i64 = 86_400_000;
const CODEX_ID: &str = "0199a000-0000-7000-8000-000000000001";

struct Srv {
    _dir: tempfile::TempDir,
    root: PathBuf,
    addr: std::net::SocketAddr,
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn user(uuid: &str, ts: i64, cwd: &Path, text: &str) -> String {
    line(json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":cwd,"message":{"role":"user","content":text}}))
}

fn step(id: &str, ts: i64, model: &str, u: [u64; 4]) -> String {
    line(json!({"type":"assistant","uuid":format!("{id}-l"),"timestamp":ts,"message":{"id":id,"model":model,
        "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":u[0],"output_tokens":u[1],"cache_read_input_tokens":u[2],"cache_creation_input_tokens":u[3]}}}))
}

fn codex(ty: &str, ts: i64, payload: Value) -> String {
    line(json!({"timestamp":ts,"type":ty,"payload":payload}))
}

fn price(id: &str, input: f64, output: f64, cache_read: f64) -> Value {
    json!({"id":id,"provider":"test","prices":[{"from":0,"input":input,"output":output,"cache_read":cache_read}],"context_limit":200000})
}

/// Two harnesses, three models (`model-c` unpriced), two git repos (`repo1` with two cwds),
/// steps on three UTC days: turn A on D-3 12:00, turn B on D-2 23:30, one more an hour ago.
async fn start() -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    for d in ["repo1/.git", "repo1/sub1", "repo1/sub2", "repo2/.git", "data/pricing", "claude/-w", "codex"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("data/pricing/overrides.json"),
        json!([price("model-a", 3.0, 15.0, 0.3), price("model-b", 1.0, 5.0, 0.1)]).to_string(),
    )
    .unwrap();
    let base = now_ms() / DAY * DAY;
    let t = [base - 3 * DAY + DAY / 2, base - DAY - DAY / 48, now_ms() - 3_600_000];
    let (sub1, sub2, repo2) = (root.join("repo1/sub1"), root.join("repo1/sub2"), root.join("repo2"));

    let mut c1 = user("u1", t[0], &sub1, "first");
    c1 += &step("m1", t[0] + 1000, "model-a", [100, 20, 1000, 50]);
    c1 += &step("m2", t[0] + 2000, "model-a", [10, 5, 1200, 0]);
    c1 += &user("u2", t[1], &sub1, "second");
    c1 += &step("m3", t[1] + 1000, "model-b", [200, 10, 3000, 0]);
    c1 += &step("m4", t[1] + 2000, "model-b", [5, 7, 3300, 0]);
    std::fs::write(root.join("claude/-w/c1.jsonl"), c1).unwrap();
    let c2 = user("v1", t[2], &sub2, "third") + &step("n1", t[2] + 1000, "model-b", [50, 5, 0, 0]);
    std::fs::write(root.join("claude/-w/c2.jsonl"), c2).unwrap();
    // A prompt still waiting for its reply: a working session, polled while hot.
    std::fs::write(root.join("claude/-w/c3.jsonl"), user("w1", now_ms(), &sub2, "live")).unwrap();

    let mut x = codex("session_meta", t[0], json!({"id":CODEX_ID,"cwd":repo2,"timestamp":t[0]}));
    x += &codex("turn_context", t[0], json!({"model":"model-c","cwd":repo2}));
    for (i, ts) in [t[0], t[2]].into_iter().enumerate() {
        x += &codex("event_msg", ts, json!({"type":"task_started","turn_id":format!("t{i}")}));
        let u = json!({"input_tokens":1000,"cached_input_tokens":400,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":1030});
        x += &codex("token_usage_record", ts + 500, json!({"response_id":format!("r{i}"),"usage":u}));
        x += &codex("event_msg", ts + 900, json!({"type":"task_complete","turn_id":format!("t{i}")}));
    }
    std::fs::write(root.join(format!("codex/rollout-2026-10-01T12-00-00-{CODEX_ID}.jsonl")), x).unwrap();

    let adapters: Vec<Arc<dyn Adapter>> = vec![
        Arc::new(JsonlAdapter::new(ClaudeFamily {
            info: HarnessInfo { id: "claude", name: "Claude Code" },
            roots: vec![root.join("claude")],
            live_dir: None,
        })),
        Arc::new(JsonlAdapter::new(Codex { roots: vec![root.join("codex")] })),
    ];
    let opts = EngineOptions {
        cache_path: None,
        hot_poll: Duration::from_millis(50),
        data_dir: Some(root.join("data")),
        ..Default::default()
    };
    let engine = Engine::new(adapters, opts);
    engine.index();
    tokio::spawn(engine.clone().run());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(uniflo_gateway::serve(
        listener,
        uniflo_gateway::router(engine, GuardOptions::default()),
        std::future::pending(),
    ));
    let srv = Srv { _dir: dir, root, addr };
    let t0 = Instant::now();
    while get(srv.addr, "/v1/usage").await.1["indexing"]["ready"] != true {
        assert!(t0.elapsed() < Duration::from_secs(10), "usage index never became ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    srv
}

async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", addr.port());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = &raw[split + 4..];
    (status, serde_json::from_slice(body).unwrap_or(Value::Null))
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b':' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn u(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap_or_else(|| panic!("{k} missing in {v}"))
}

fn rows_by_key(r: &Value) -> std::collections::BTreeMap<String, Value> {
    r["rows"].as_array().unwrap().iter().map(|x| (x["key"].as_str().unwrap().to_owned(), x.clone())).collect()
}

/// Every additive column of the rows sums to `totals`.
fn assert_rows_add_up(r: &Value) {
    let rows = r["rows"].as_array().unwrap();
    for k in ["steps", "prompts", "input", "output", "cache_read", "cache_write", "reasoning", "unpriced_steps"] {
        assert_eq!(rows.iter().map(|x| u(x, k)).sum::<u64>(), u(&r["totals"], k), "{k} of {}", r["group_by"]);
    }
    let cost: f64 = rows.iter().filter_map(|x| x["cost_usd"].as_f64()).sum();
    assert!((cost - r["totals"]["cost_usd"].as_f64().unwrap()).abs() < 1e-9, "cost of {}", r["group_by"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_groups_add_up_project_dir_window_and_tz() {
    let srv = start().await;
    let repo1 = srv.root.join("repo1").to_string_lossy().into_owned();
    let repo2 = srv.root.join("repo2").to_string_lossy().into_owned();
    for g in ["harness", "model", "project", "cwd", "dir", "day", "hour", "weekday", "session"] {
        let (status, r) = get(srv.addr, &format!("/v1/usage?group_by={g}")).await;
        assert_eq!(status, 200, "{g}: {r}");
        assert_rows_add_up(&r);
        assert_eq!(u(&r["totals"], "steps"), 7, "{g}");
        assert_eq!(u(&r["totals"], "unpriced_steps"), 2, "model-c has no price");
    }

    let (_, r) = get(srv.addr, "/v1/usage?group_by=model").await;
    let m = rows_by_key(&r);
    assert_eq!(m.keys().map(String::as_str).collect::<Vec<_>>(), ["", "model-a", "model-b", "model-c"]);
    assert_eq!((u(&m[""], "steps"), u(&m[""], "prompts")), (0, 1), "c3's prompt has no model call yet");
    assert!(m["model-c"]["cost_usd"].is_null() && u(&m["model-c"], "unpriced_steps") == 2);
    let a =
        (100.0 * 3.0 + 20.0 * 15.0 + 1000.0 * 0.3 + 50.0 * 3.0 * 1.25 + 10.0 * 3.0 + 5.0 * 15.0 + 1200.0 * 0.3) / 1e6;
    assert!((m["model-a"]["cost_usd"].as_f64().unwrap() - a).abs() < 1e-12);
    assert_eq!((u(&m["model-c"], "input"), u(&m["model-c"], "cache_read")), (1200, 800), "codex input minus cached");

    let (_, r) = get(srv.addr, "/v1/usage?group_by=project").await;
    let p = rows_by_key(&r);
    assert_eq!(p.keys().cloned().collect::<Vec<_>>(), [repo1.clone(), repo2], "two cwds of repo1 are one project");
    assert_eq!((u(&p[&repo1], "steps"), u(&p[&repo1], "sessions")), (5, 3));

    let (_, r) = get(srv.addr, &format!("/v1/usage?group_by=dir&under={}", enc(&repo1))).await;
    assert_rows_add_up(&r);
    let d = rows_by_key(&r);
    let labels: Vec<_> = d.values().map(|x| (x["label"].as_str().unwrap().to_owned(), u(x, "steps"))).collect();
    assert_eq!(labels, [("sub1".to_owned(), 4), ("sub2".to_owned(), 1)]);

    for (q, steps) in [("h:codex since:1d", 1), ("h:claude since:1d", 1), ("h:claude", 5), ("since:1d", 2)] {
        let (_, r) = get(srv.addr, &format!("/v1/usage?q={}", enc(q))).await;
        assert_eq!(u(&r["totals"], "steps"), steps, "{q}");
    }
    let (status, _) = get(srv.addr, "/v1/usage?group_by=nope").await;
    assert_eq!(status, 400);
    let (status, _) = get(srv.addr, "/v1/usage?tz=Mars/Base").await;
    assert_eq!(status, 400);

    // Turn B (D-2 23:30 UTC) is already D-1 in +08:00.
    let (_, utc) = get(srv.addr, "/v1/usage?group_by=day&tz=UTC").await;
    let (_, east) = get(srv.addr, &format!("/v1/usage?group_by=day&tz={}", enc("+08:00"))).await;
    let (utc, east) = (rows_by_key(&utc), rows_by_key(&east));
    let day_b = utc.keys().nth(1).unwrap().clone();
    assert_eq!(u(&utc[&day_b], "steps"), 2);
    assert!(!east.contains_key(&day_b), "{day_b} moves to the next day east of UTC");
    assert_eq!(utc.keys().next(), east.keys().next());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_step_detail_turns_and_context() {
    let srv = start().await;
    let (status, d) = get(srv.addr, "/v1/sessions/claude:c1/usage").await;
    assert_eq!(status, 200, "{d}");
    let steps = d["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 4);
    for s in steps {
        let ctx = u(s, "input") + u(s, "cache_read") + u(s, "cache_write");
        assert_eq!(u(s, "context_tokens"), ctx);
        assert_eq!(u(s, "context_limit"), 200_000);
        assert!((s["context_pct"].as_f64().unwrap() - ctx as f64 * 100.0 / 200_000.0).abs() < 1e-9);
        assert_eq!(s["cost_source"], "catalog");
    }
    let turns = d["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    for (t, part) in turns.iter().zip([&steps[..2], &steps[2..]]) {
        for k in ["input", "output", "cache_read", "cache_write"] {
            assert_eq!(u(t, k), part.iter().map(|s| u(s, k)).sum::<u64>(), "turn {k}");
        }
        let cost: f64 = part.iter().map(|s| s["cost_usd"].as_f64().unwrap()).sum();
        assert!((t["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
    }
    let (_, s) = get(srv.addr, "/v1/sessions/claude:c1").await;
    assert_eq!(u(&s["usage"], "last_context_tokens"), u(&steps[3], "context_tokens"));
    assert_eq!(u(&s["usage"], "steps"), 4);
    assert_eq!(get(srv.addr, "/v1/sessions/claude:nope/usage").await.0, 404);

    let (_, models) = get(srv.addr, "/v1/models").await;
    let models = models.as_array().unwrap();
    let a = models.iter().find(|m| m["model"] == "model-a").unwrap();
    assert_eq!((a["match"].as_str(), u(a, "context_limit"), u(a, "steps")), (Some("exact"), 200_000, 2));
    let c = models.iter().find(|m| m["model"] == "model-c").unwrap();
    assert_eq!((c["match"].as_str(), c["cost_usd"].is_null()), (Some("none"), true));

    let (_, p) = get(srv.addr, "/v1/pricing").await;
    assert_eq!(
        (p["source"].as_str(), u(&p, "overrides"), p["sync_enabled"].as_bool()),
        (Some("snapshot"), 2, Some(false))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_usage_is_pushed_live() {
    let srv = start().await;
    let mut s = TcpStream::connect(srv.addr).await.unwrap();
    let req = format!("GET /v1/stream.ndjson?types=session HTTP/1.1\r\nHost: localhost:{}\r\n\r\n", srv.addr.port());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(s);
    let mut l = String::new();
    loop {
        l.clear();
        r.read_line(&mut l).await.unwrap();
        if l == "\r\n" {
            break;
        }
    }
    let before = get(srv.addr, "/v1/sessions/claude:c3").await.1;
    assert_eq!(before["status"], "work");
    let steps = before["usage"]["steps"].as_u64().unwrap_or(0);
    let cost = before["usage"]["cost_usd"].as_f64().unwrap_or(0.0);
    let mut f = std::fs::OpenOptions::new().append(true).open(srv.root.join("claude/-w/c3.jsonl")).unwrap();
    f.write_all(step("n2", now_ms(), "model-b", [1000, 100, 0, 0]).as_bytes()).unwrap();
    let t0 = Instant::now();
    let pushed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            l.clear();
            assert!(r.read_line(&mut l).await.unwrap() > 0, "stream closed");
            let Ok(v) = serde_json::from_str::<Value>(l.trim().trim_start_matches(|c: char| c.is_ascii_hexdigit()))
            else {
                continue;
            };
            if v["session"]["key"] == "claude:c3" && v["session"]["usage"]["steps"].as_u64() == Some(steps + 1) {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("no session envelope with the new step within 2 s; last line {l:?}");
    });
    let new_cost = pushed["session"]["usage"]["cost_usd"].as_f64().unwrap();
    assert!((new_cost - cost - (1000.0 * 1.0 + 100.0 * 5.0) / 1e6).abs() < 1e-12);
    eprintln!("pushed after {} ms", t0.elapsed().as_millis());
}
