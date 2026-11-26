//! High-concurrency integration tests across REST endpoints, live writes, FTS, and cache saves.

use serde_json::{Value, json};
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::{GuardOptions, Services};
use uniflo_search::fts::{Fts, FtsOptions};

const DAY: i64 = 86_400_000;

struct Srv {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    fts: Arc<Fts>,
    claude_dir: PathBuf,
    addr: SocketAddr,
}

fn rec(v: Value) -> String {
    format!("{v}\n")
}

fn user(uuid: &str, ts: i64, cwd: &Path, text: &str) -> String {
    rec(json!({
        "type": "user",
        "uuid": uuid,
        "timestamp": ts,
        "cwd": cwd,
        "message": { "role": "user", "content": text }
    }))
}

fn assistant(id: &str, ts: i64, model: &str, text: &str, tokens: [u64; 4], stop: Option<&str>) -> String {
    rec(json!({
        "type": "assistant",
        "uuid": format!("{id}-l"),
        "timestamp": ts,
        "message": {
            "id": id,
            "model": model,
            "content": [{ "type": "text", "text": text }],
            "stop_reason": stop,
            "usage": {
                "input_tokens": tokens[0],
                "output_tokens": tokens[1],
                "cache_read_input_tokens": tokens[2],
                "cache_creation_input_tokens": tokens[3]
            }
        }
    }))
}

fn price(id: &str, input: f64, output: f64) -> Value {
    json!({
        "id": id,
        "provider": "test",
        "prices": [{ "from": 0, "input": input, "output": output, "cache_read": 0.1 }],
        "context_limit": 200000
    })
}

const TOPICS: [&str; 8] = ["mercury", "venus", "earth", "mars", "jupiter", "saturn", "uranus", "neptune"];

async fn start(num_sessions: usize) -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let claude_dir = root.join("claude/-w-bench");
    let repo_dir = root.join("repo");
    let pricing_dir = root.join("data/pricing");
    let cache_dir = root.join("cache");
    for d in [&claude_dir, &repo_dir.join(".git"), &pricing_dir, &cache_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        pricing_dir.join("overrides.json"),
        json!([price("model-fast", 1.0, 5.0), price("model-pro", 3.0, 15.0)]).to_string(),
    )
    .unwrap();

    let now = now_ms();
    for i in 0..num_sessions {
        let mut body = String::new();
        let ts = now - ((i % 10) as i64) * DAY - (i as i64) * 1_000;
        let model = if i % 2 == 0 { "model-fast" } else { "model-pro" };
        let topic = TOPICS[i % TOPICS.len()];
        for turn in 0..4 {
            let t = ts + turn * 2_000;
            body += &user(
                &format!("u-{i}-{turn}"),
                t,
                &repo_dir,
                &format!("session {i} turn {turn} gateway {topic} needle"),
            );
            // Make a handful of sessions appear actively working (no stop_reason, recent ts).
            let stop = if i < 4 && turn == 3 { None } else { Some("end_turn") };
            let reply_ts = if i < 4 && turn == 3 { now - 500 } else { t + 500 };
            body += &assistant(
                &format!("a-{i}-{turn}"),
                reply_ts,
                model,
                &format!("handled {topic} request {i} step {turn}"),
                [1_000, 500, 200, 100],
                stop,
            );
        }
        std::fs::write(claude_dir.join(format!("sess-{i:03}.jsonl")), body).unwrap();
    }

    let adapter: Arc<dyn Adapter> = Arc::new(JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![root.join("claude")],
        live_dir: None,
    }));
    let opts = EngineOptions {
        cache_path: Some(cache_dir.join("index-v1.json")),
        data_dir: Some(root.join("data")),
        hot_poll: Duration::from_millis(40),
        ..Default::default()
    };
    let engine = Engine::new(vec![adapter], opts);
    engine.index();
    engine.index_usage();
    let fts = Arc::new(
        Fts::start(engine.clone(), FtsOptions { path: cache_dir.join("fts-v2.sqlite"), ..Default::default() }).unwrap(),
    );
    // Wait briefly until initial FTS build finishes so concurrent search tests assert exact counts.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let st = fts.status();
        if !st.indexing && st.progress.done >= num_sessions {
            break;
        }
        assert!(Instant::now() < deadline, "initial FTS build timed out: {st:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::spawn(engine.clone().run());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = uniflo_gateway::router_with(
        engine.clone(),
        GuardOptions::default(),
        Services { fts: Some(fts.clone()), cleanup: None, messages: None },
    );
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));
    Srv { _dir: dir, engine, fts, claude_dir, addr }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
}

async fn get(addr: SocketAddr, path: &str) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", addr.port());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.lines();
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    let mut body = raw[split + 4..].to_vec();
    if headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked")) {
        body = dechunk(&body);
    }
    Resp { status, headers, body }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn high_concurrency_mixed_read_endpoints_stay_consistent() {
    let srv = start(48).await;
    let addr = srv.addr;

    let mut paths: Vec<&'static str> = Vec::new();
    for _ in 0..16 {
        paths.push("/v1/health");
        paths.push("/v1/sessions");
        paths.push("/v1/sessions?q=s:work");
        paths.push("/v1/sessions?q='jupiter");
    }
    for _ in 0..8 {
        paths.push("/v1/harnesses");
        paths.push("/v1/stats");
        paths.push("/v1/sessions/claude:sess-000");
        paths.push("/v1/sessions/claude:sess-000/events?limit=20");
        paths.push("/v1/usage?group_by=harness");
        paths.push("/v1/usage?group_by=day&since=30d");
        paths.push("/v1/models");
        paths.push("/v1/search?q=jupiter");
    }

    let handles: Vec<_> = paths
        .into_iter()
        .map(|p| {
            tokio::spawn(async move {
                let r = get(addr, p).await;
                (p, r)
            })
        })
        .collect();

    let mut sessions_body: Option<Vec<u8>> = None;
    let mut usage_body: Option<Vec<u8>> = None;
    let mut search_body: Option<Vec<u8>> = None;
    for h in handles {
        let (path, r) = h.await.unwrap();
        assert_eq!(r.status, 200, "path={path}");
        let v = r.json();
        match path {
            "/v1/health" => {
                assert_eq!(v["ok"], true);
                assert_eq!(v["sessions"], 48);
            }
            "/v1/sessions" => {
                assert_eq!(v.as_array().unwrap().len(), 48);
                assert!(r.header("x-uniflo-seq").is_some());
                match &sessions_body {
                    Some(prev) => assert_eq!(*prev, r.body, "concurrent /v1/sessions payloads must agree"),
                    None => sessions_body = Some(r.body),
                }
            }
            "/v1/sessions?q=s:work" => {
                assert_eq!(v.as_array().unwrap().len(), 4);
            }
            "/v1/sessions?q='jupiter" => {
                assert_eq!(v.as_array().unwrap().len(), 6);
            }
            "/v1/harnesses" => {
                assert_eq!(v[0]["id"], "claude");
                assert_eq!(v[0]["sessions"], 48);
            }
            "/v1/stats" => {
                assert_eq!(v["sessions"], 48);
                assert_eq!(v["fts"]["indexing"], false);
                assert_eq!(v["fts"]["progress"]["done"], 48);
            }
            "/v1/sessions/claude:sess-000" => {
                assert_eq!(v["key"], "claude:sess-000");
            }
            "/v1/sessions/claude:sess-000/events?limit=20" => {
                // 4 user + 4 assistant + 4 usage + 3 TurnEnd (turn 3 has stop_reason=None).
                assert_eq!(v["events"].as_array().unwrap().len(), 15);
            }
            "/v1/usage?group_by=harness" => {
                // 48 sessions * 4 steps * 1000 input tokens = 192,000.
                assert_eq!(v["totals"]["input"], 192_000);
                match &usage_body {
                    Some(prev) => assert_eq!(*prev, r.body, "concurrent /v1/usage payloads must agree"),
                    None => usage_body = Some(r.body),
                }
            }
            "/v1/usage?group_by=day&since=30d" => {
                assert_eq!(v["totals"]["input"], 192_000);
                assert!(!v["rows"].as_array().unwrap().is_empty());
            }
            "/v1/models" => {
                assert_eq!(v.as_array().unwrap().len(), 2);
            }
            "/v1/search?q=jupiter" => {
                assert_eq!(v["total"], 6);
                match &search_body {
                    Some(prev) => assert_eq!(*prev, r.body, "concurrent /v1/search payloads must agree"),
                    None => search_body = Some(r.body),
                }
            }
            other => unreachable!("unexpected path {other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_reads_with_live_writes_invalidate_cached_responses() {
    let srv = start(24).await;
    let addr = srv.addr;
    let target = srv.claude_dir.join("sess-000.jsonl");
    let repo_dir = srv._dir.path().join("repo");

    // Warm the response cache on the pre-write generation.
    let before = get(addr, "/v1/sessions?q=liveinvalidationmarker").await;
    assert_eq!(before.status, 200);
    assert!(before.json().as_array().unwrap().is_empty());
    let seq_before: u64 = before.header("x-uniflo-seq").unwrap().parse().unwrap();

    // Open a live SSE stream so broadcast/stream contention is exercised concurrently too.
    let mut sse = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET /v1/stream HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.port());
    sse.write_all(req.as_bytes()).await.unwrap();
    let mut sse_reader = BufReader::new(sse);
    loop {
        let mut line = String::new();
        sse_reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for i in 0..24 {
        let stop = stop.clone();
        readers.push(tokio::spawn(async move {
            let endpoints = [
                "/v1/health",
                "/v1/sessions",
                "/v1/sessions?q=liveinvalidationmarker",
                "/v1/sessions/claude:sess-000",
                "/v1/sessions/claude:sess-000/events?limit=20",
                "/v1/usage?group_by=harness",
            ];
            let mut count = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let p = endpoints[(i + count) % endpoints.len()];
                let r = get(addr, p).await;
                assert_eq!(r.status, 200, "reader {i} path {p}");
                count += 1;
            }
            count
        }));
    }

    // Perform live appends on an existing session and add a brand-new session while readers hammer the gateway.
    for step in 0..5 {
        let ts = now_ms() + step * 10;
        let mut f = std::fs::OpenOptions::new().append(true).open(&target).unwrap();
        write!(f, "{}", user(&format!("live-u-{step}"), ts, &repo_dir, "live prompt")).unwrap();
        write!(
            f,
            "{}",
            assistant(&format!("live-a-{step}"), ts + 1, "model-fast", "live ack", [500, 250, 0, 0], Some("end_turn"))
        )
        .unwrap();
        srv.engine.notify_path(target.clone()).await;
    }
    let fresh = srv.claude_dir.join("sess-live.jsonl");
    std::fs::write(
        &fresh,
        user("fresh-u", now_ms() + 100, &repo_dir, "final liveinvalidationmarker payload")
            + &assistant("fresh-a", now_ms() + 101, "model-fast", "ok", [500, 250, 0, 0], Some("end_turn")),
    )
    .unwrap();
    srv.engine.notify_path(fresh).await;

    stop.store(true, Ordering::Relaxed);
    let mut total_reads = 0usize;
    for h in readers {
        total_reads += h.await.unwrap();
    }
    assert!(total_reads >= 24);

    // Immediately after writes complete, cached `/v1/sessions?q=...` must reflect the new generation.
    let after = get(addr, "/v1/sessions?q=liveinvalidationmarker").await;
    assert_eq!(after.status, 200);
    let hits = after.json();
    let arr = hits.as_array().unwrap();
    assert_eq!(arr.len(), 1, "response cache must invalidate immediately on state bump");
    assert_eq!(arr[0]["key"], "claude:sess-live");
    let seq_after: u64 = after.header("x-uniflo-seq").unwrap().parse().unwrap();
    assert!(seq_after > seq_before);

    // Events endpoint must also reflect all appended turns (15 original + 5 * 4 live = 35, capped by limit=40).
    let evs = get(addr, "/v1/sessions/claude:sess-000/events?limit=40").await;
    assert_eq!(evs.status, 200);
    assert_eq!(evs.json()["events"].as_array().unwrap().len(), 35);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_distinct_fts_queries_and_live_indexing_succeed() {
    let srv = start(40).await;
    let addr = srv.addr;
    let target = srv.claude_dir.join("sess-001.jsonl");
    let repo_dir = srv._dir.path().join("repo");

    // Trigger live FTS indexing in parallel with 24 concurrent distinct search queries + stats calls.
    let engine = srv.engine.clone();
    let writer = tokio::spawn(async move {
        for step in 0..4 {
            let mut f = std::fs::OpenOptions::new().append(true).open(&target).unwrap();
            write!(f, "{}", user(&format!("fts-u-{step}"), now_ms() + step, &repo_dir, &format!("freshftsword{step}")))
                .unwrap();
            engine.notify_path(target.clone()).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    let mut handles = Vec::new();
    for i in 0..24 {
        let q = format!("/v1/search?q={}", TOPICS[i % TOPICS.len()]);
        handles.push(tokio::spawn(async move { (q.clone(), get(addr, &q).await) }));
        if i % 3 == 0 {
            handles.push(tokio::spawn(async move { ("/v1/stats".into(), get(addr, "/v1/stats").await) }));
        }
    }

    writer.await.unwrap();
    for h in handles {
        let (path, r) = h.await.unwrap();
        assert_eq!(r.status, 200, "path={path}");
        let v = r.json();
        if path.starts_with("/v1/search") {
            assert_eq!(v["total"], 5, "40 sessions / 8 topics = 5 hits for {path}");
        } else {
            assert_eq!(v["sessions"], 40);
        }
    }
    assert_eq!(srv.fts.status().errors, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn lock_free_cache_saves_do_not_stall_concurrent_readers() {
    let srv = start(40).await;
    let addr = srv.addr;
    let engine = srv.engine.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_saver = stop.clone();
    let saver = tokio::task::spawn_blocking(move || {
        let mut saves = 0usize;
        while !stop_saver.load(Ordering::Relaxed) && saves < 8 {
            engine.save_cache().unwrap();
            engine.save_usage_cache().unwrap();
            saves += 1;
        }
        saves
    });

    let mut readers = Vec::new();
    for i in 0..32 {
        let path = match i % 4 {
            0 => "/v1/health",
            1 => "/v1/sessions",
            2 => "/v1/usage?group_by=harness",
            _ => "/v1/models",
        };
        readers.push(tokio::spawn(async move {
            let started = Instant::now();
            let r = get(addr, path).await;
            assert_eq!(r.status, 200, "path={path}");
            started.elapsed()
        }));
    }

    let mut max_lat = Duration::ZERO;
    for h in readers {
        max_lat = max_lat.max(h.await.unwrap());
    }
    stop.store(true, Ordering::Relaxed);
    let saves = saver.await.unwrap();
    assert!(saves >= 1);
    // Sanity bound generous enough for debug builds while still catching multi-second lock stalls.
    assert!(max_lat < Duration::from_secs(5), "unexpected reader stall during cache save: {max_lat:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn singleflight_bursts_and_relative_time_queries_stay_accurate() {
    let srv = start(32).await;
    let addr = srv.addr;

    // 32 identical concurrent requests on non-relative + relative-time endpoints.
    let mut handles = Vec::new();
    for _ in 0..16 {
        handles.push(tokio::spawn(async move { get(addr, "/v1/sessions?q=s:idle").await }));
        handles.push(tokio::spawn(async move { get(addr, "/v1/sessions?q=since:30m").await }));
        handles.push(tokio::spawn(async move { get(addr, "/v1/usage?group_by=day&since=3d").await }));
    }

    let mut idle_body: Option<Vec<u8>> = None;
    for (idx, h) in handles.into_iter().enumerate() {
        let r = h.await.unwrap();
        assert_eq!(r.status, 200, "req {idx}");
        if idx % 3 == 0 {
            assert_eq!(r.json().as_array().unwrap().len(), 28);
            match &idle_body {
                Some(prev) => assert_eq!(*prev, r.body),
                None => idle_body = Some(r.body),
            }
        }
    }
}
