//! Cross-agent message bus routes over real TCP: send, inbox, ack, SSE push stream,
//! write-guard enforcement (`X-Uniflo-Write: 1`, `--read-only`), and high-concurrency stress.

use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_core::message::MessageBus;
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::{GuardOptions, Services};

struct Srv {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

async fn start(guard: GuardOptions, with_bus: bool) -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    let proj = root.join("-w-proj");
    std::fs::create_dir_all(&proj).unwrap();
    let now = now_ms();
    std::fs::write(
        proj.join("sess-a.jsonl"),
        line(json!({"type":"user","uuid":"u1","timestamp":now - 500,"cwd":"/w/proj","message":{"role":"user","content":"working on api"}})),
    )
    .unwrap();
    std::fs::write(
        proj.join("sess-b.jsonl"),
        line(json!({"type":"user","uuid":"u2","timestamp":now - 5_000,"cwd":"/w/other","message":{"role":"user","content":"other task"}}))
            + &line(json!({"type":"assistant","uuid":"a2","timestamp":now - 4_000,"message":{"id":"a2","model":"m","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}})),
    )
    .unwrap();

    let adapter: Arc<dyn Adapter> = Arc::new(JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![root],
        live_dir: None,
    }));
    let engine = Engine::new(vec![adapter], EngineOptions { cache_path: None, usage: false, ..Default::default() });
    engine.index();
    let messages = with_bus.then(|| MessageBus::open(Some(&dir.path().join("data"))).unwrap());
    let router = uniflo_gateway::router_with(engine, guard, Services { fts: None, cleanup: None, messages });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));
    Srv { _dir: dir, addr }
}

async fn req(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let payload = body.map(|b| serde_json::to_vec(b).unwrap()).unwrap_or_default();
    let mut raw_req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n", addr.port());
    for (k, v) in headers {
        raw_req += &format!("{k}: {v}\r\n");
    }
    if body.is_some() {
        raw_req += &format!("Content-Type: application/json\r\nContent-Length: {}\r\n", payload.len());
    }
    raw_req += "\r\n";
    s.write_all(raw_req.as_bytes()).await.unwrap();
    if !payload.is_empty() {
        s.write_all(&payload).await.unwrap();
    }
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status: u16 = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let v = serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null);
    (status, v)
}

const WRITE: &[(&str, &str)] = &[("X-Uniflo-Write", "1")];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_inbox_ack_sse_push_and_write_guard() {
    let srv = start(GuardOptions::default(), true).await;
    let addr = srv.addr;

    // Without X-Uniflo-Write: 1, POST is blocked by write::check (403).
    let (st, _) = req(addr, "POST", "/v1/messages", &[], Some(&json!({"to":"claude:sess-a","text":"nope"}))).await;
    assert_eq!(st, 403);

    // Open live SSE push stream before sending.
    let mut sse = TcpStream::connect(addr).await.unwrap();
    let get_sse =
        format!("GET /v1/messages/stream?to=claude:sess-a HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", addr.port());
    sse.write_all(get_sse.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(sse);
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).await.unwrap();
        if l == "\r\n" {
            break;
        }
    }

    // Send by cwd: target (resolves to working `claude:sess-a`).
    let t0 = Instant::now();
    let (st, sent) = req(
        addr,
        "POST",
        "/v1/messages",
        WRITE,
        Some(&json!({"to":"cwd:/w/proj","from":"claude:sess-b","text":"please check PR #7"})),
    )
    .await;
    assert_eq!(st, 200, "{sent}");
    assert_eq!(sent["target"]["key"], "claude:sess-a");
    assert_eq!(sent["target"]["delivery"], "hook");
    let msg_id = sent["message"]["id"].as_str().unwrap().to_owned();

    // SSE stream receives the message immediately without polling.
    let sse_data = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).await.unwrap();
            if let Some(d) = l.trim_end().strip_prefix("data:") {
                return serde_json::from_str::<Value>(d.trim_start()).unwrap();
            }
        }
    })
    .await
    .expect("SSE message push timed out");
    assert_eq!(sse_data["id"], msg_id);
    assert_eq!(sse_data["text"], "please check PR #7");
    assert!(t0.elapsed() < Duration::from_secs(1));

    // Undelivered inbox sees it; marking delivered clears undelivered but keeps unread.
    let (st, inbox) = req(addr, "GET", "/v1/messages?to=claude:sess-a&undelivered=true", &[], None).await;
    assert_eq!(st, 200);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["unread"], 1);

    let (st, ack) =
        req(addr, "POST", "/v1/messages/ack", WRITE, Some(&json!({"ids":[msg_id],"state":"delivered"}))).await;
    assert_eq!(st, 200);
    assert_eq!(ack["updated"], 1);

    let (_, undeliv) = req(addr, "GET", "/v1/messages?to=claude:sess-a&undelivered=true", &[], None).await;
    assert!(undeliv["messages"].as_array().unwrap().is_empty());
    assert_eq!(undeliv["unread"], 1);

    let (st, ack) = req(addr, "POST", "/v1/messages/ack", WRITE, Some(&json!({"to":"claude:sess-a"}))).await;
    assert_eq!(st, 200);
    assert_eq!(ack["updated"], 1);
    let (_, after) = req(addr, "GET", "/v1/messages?to=claude:sess-a&unread=true", &[], None).await;
    assert!(after["messages"].as_array().unwrap().is_empty());
    assert_eq!(after["unread"], 0);

    // Read-only daemon allows GET inbox but blocks POST send/ack with 403.
    let ro = start(GuardOptions { read_only: true, ..Default::default() }, true).await;
    assert_eq!(req(ro.addr, "GET", "/v1/messages", &[], None).await.0, 200);
    assert_eq!(
        req(ro.addr, "POST", "/v1/messages", WRITE, Some(&json!({"to":"claude:sess-a","text":"hi"}))).await.0,
        403
    );

    // Gateway without MessageBus answers 503.
    let off = start(GuardOptions::default(), false).await;
    assert_eq!(req(off.addr, "GET", "/v1/messages", &[], None).await.0, 503);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn high_concurrency_message_send_and_inbox_reads() {
    let srv = start(GuardOptions::default(), true).await;
    let addr = srv.addr;

    let mut handles = Vec::new();
    for i in 0..64 {
        handles.push(tokio::spawn(async move {
            let (st, v) = req(
                addr,
                "POST",
                "/v1/messages",
                WRITE,
                Some(&json!({"to":"claude:sess-a","from":format!("agent-{i}"),"text":format!("msg-{i}")})),
            )
            .await;
            assert_eq!(st, 200, "send {i}: {v}");
        }));
        handles.push(tokio::spawn(async move {
            let (st, _) = req(addr, "GET", "/v1/messages?to=claude:sess-a&limit=100", &[], None).await;
            assert_eq!(st, 200, "inbox {i}");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let (st, inbox) = req(addr, "GET", "/v1/messages?to=claude:sess-a&limit=100", &[], None).await;
    assert_eq!(st, 200);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 64);
    assert_eq!(inbox["unread"], 64);
}
