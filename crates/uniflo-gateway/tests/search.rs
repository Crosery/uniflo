//! `/v1/search` and `events?around=` over real TCP with a full-text index on synthetic data.

use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::GuardOptions;
use uniflo_search::fts::{Fts, FtsOptions};

fn user(uuid: &str, ts: i64, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":"/w/demo","message":{"role":"user","content":text}})
    )
}

fn assistant(uuid: &str, ts: i64, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"assistant","uuid":uuid,"timestamp":ts,"message":{"id":uuid,"model":"m","content":[{"type":"text","text":text}],"stop_reason":"end_turn"}})
    )
}

fn engine(root: &Path) -> Arc<Engine> {
    let adapter: Arc<dyn Adapter> = Arc::new(JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![root.to_path_buf()],
        live_dir: None,
    }));
    let engine = Engine::new(vec![adapter], EngineOptions { cache_path: None, ..Default::default() });
    engine.index();
    engine
}

async fn serve(engine: Arc<Engine>, fts: Option<Arc<Fts>>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = uniflo_gateway::router_with_fts(engine, GuardOptions::default(), fts);
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));
    addr
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
    let mut body = raw[split + 4..].to_vec();
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut b = &body[..];
        while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
            let n = usize::from_str_radix(std::str::from_utf8(&b[..i]).unwrap().trim(), 16).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&b[i + 2..i + 2 + n]);
            b = &b[i + 4 + n..];
        }
        body = out;
    }
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
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

/// Scenario: 定位到事件窗口 (plus the REST shape of `/v1/search`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_hit_opens_a_centred_window() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("-w-demo")).unwrap();
    let t = now_ms() - 3_600_000;
    let mut body = String::new();
    for i in 0..60 {
        let q = if i == 30 { "where is the pivotal fixture".to_owned() } else { format!("question {i}") };
        body += &user(&format!("u{i}"), t + i * 10, &q);
        body += &assistant(&format!("a{i}"), t + i * 10 + 1, &format!("answer {i}"));
    }
    std::fs::write(root.join("-w-demo/long.jsonl"), body).unwrap();
    let engine = engine(&root);
    let fts = tokio::task::block_in_place(|| {
        let fts = Fts::start(engine.clone(), FtsOptions { path: dir.path().join("fts.sqlite"), ..Default::default() })
            .unwrap();
        assert!(fts.wait_idle(Duration::from_secs(10)));
        Arc::new(fts)
    });
    let addr = serve(engine, Some(fts)).await;

    let (st, r) = get(addr, "/v1/search?q=pivotal").await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["indexing"], false);
    assert_eq!(r["total"], 1);
    let hit = &r["results"][0];
    assert_eq!(
        (hit["session"].as_str(), hit["harness"].as_str(), hit["cwd"].as_str()),
        (Some("claude:long"), Some("claude"), Some("/w/demo"))
    );
    assert_eq!(hit["hits"][0]["event"], "u30");
    assert_eq!(hit["hits"][0]["kind"], "user_message");
    assert!(hit["hits"][0]["snippet"].as_str().unwrap().contains("\u{2}pivotal\u{3}"));

    let (st, w) = get(addr, &format!("/v1/sessions/{}/events?around=u30&limit=20", enc("claude:long"))).await;
    assert_eq!(st, 200, "{w}");
    assert_eq!(w["around"], "u30");
    let ids: Vec<&str> = w["events"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 20);
    let at = ids.iter().position(|i| *i == "u30").unwrap();
    assert_eq!(at, 9, "{ids:?}");
    // Each turn is u{i}, a{i}#0, a{i}:end: 9 events before u30, 10 after.
    assert_eq!((ids[0], ids[19]), ("u27", "a33#0"), "{ids:?}");
    assert!(w["next_before"].is_u64(), "older events exist");

    let (st, _) = get(addr, &format!("/v1/sessions/{}/events?around=nope", enc("claude:long"))).await;
    assert_eq!(st, 404);
    let (st, e) = get(addr, "/v1/search?q=-only").await;
    assert_eq!(st, 400, "{e}");
    let (st, _) = get(addr, "/v1/search").await;
    assert_eq!(st, 400);
    let (st, s) = get(addr, "/v1/stats").await;
    assert_eq!(st, 200);
    assert_eq!(s["fts"]["indexing"], false);
    assert!(s["fts"]["progress"]["events"].as_u64().unwrap() >= 120);
    assert!(s["sessions"].is_u64(), "engine counters still at the top level");
}

/// Scenario: 后台构建、格式升级与关闭 — the HTTP half: health answers at once while the
/// backfill of 2000 events is still running, and search reports progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_is_immediate_while_the_index_builds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(root.join("-w-demo")).unwrap();
    let t = now_ms() - 86_400_000;
    for s in 0..40 {
        let body: String = (0..25)
            .flat_map(|i| {
                [user(&format!("u{i}"), t + i, &format!("batch{s} q{i}")), assistant(&format!("a{i}"), t + i, "ok")]
            })
            .collect();
        std::fs::write(root.join(format!("-w-demo/b{s}.jsonl")), body).unwrap();
    }
    let engine = engine(&root);
    let fts = tokio::task::block_in_place(|| {
        let opts = FtsOptions { path: dir.path().join("fts.sqlite"), pause: 20.0, page: 100, ..Default::default() };
        Arc::new(Fts::start(engine.clone(), opts).unwrap())
    });
    let addr = serve(engine, Some(fts.clone())).await;

    let t0 = Instant::now();
    let (st, h) = get(addr, "/v1/health").await;
    assert_eq!((st, h["ok"].as_bool()), (200, Some(true)));
    assert!(t0.elapsed() < Duration::from_millis(500), "health took {:?}", t0.elapsed());
    let (st, r) = get(addr, "/v1/search?q=batch1").await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["indexing"], true, "{r}");
    assert_eq!(r["progress"]["total"], 40);
    assert!(r["progress"]["done"].as_u64().unwrap() < 40);

    assert!(tokio::task::block_in_place(|| fts.wait_idle(Duration::from_secs(60))));
    let (_, r) = get(addr, "/v1/search?q=batch1").await;
    assert_eq!(r["indexing"], false);
    assert_eq!(r["progress"]["events"], 2000);
    assert_eq!(r["total"], 11, "batch1 and batch10..batch19");
}

#[tokio::test]
async fn search_without_index_is_503() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let addr = serve(engine(&root), None).await;
    let (st, e) = get(addr, "/v1/search?q=anything").await;
    assert_eq!(st, 503);
    assert!(e["error"].as_str().unwrap().contains("--no-fts"), "{e}");
    let (_, s) = get(addr, "/v1/stats").await;
    assert!(s["fts"].is_null());
}
