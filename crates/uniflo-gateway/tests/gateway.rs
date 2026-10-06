//! Gateway over real TCP: REST shapes, streaming transports, guard rules.

use serde_json::{Value, json};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_gateway::GuardOptions;

struct Srv {
    _dir: tempfile::TempDir,
    file: PathBuf,
    addr: std::net::SocketAddr,
}

fn rec(v: Value) -> String {
    format!("{v}\n")
}

fn user(uuid: &str, text: &str) -> String {
    rec(
        json!({"type":"user","uuid":uuid,"timestamp":"2026-10-02T12:00:00Z","cwd":"/w/demo","message":{"role":"user","content":text}}),
    )
}

fn assistant(uuid: &str, text: &str) -> String {
    rec(
        json!({"type":"assistant","uuid":uuid,"timestamp":"2026-10-02T12:00:01Z","message":{"id":uuid,"model":"m","content":[{"type":"text","text":text}],"stop_reason":"end_turn"}}),
    )
}

async fn start(guard: GuardOptions) -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let file = root.join("-w-demo/sess-1.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut body = String::new();
    for i in 0..30 {
        body += &user(&format!("u{i}"), &format!("question {i}"));
        body += &assistant(&format!("a{i}"), &format!("answer {i}"));
    }
    std::fs::write(&file, body).unwrap();
    std::fs::write(root.join("-w-demo/other.jsonl"), user("x", "refactor the gateway")).unwrap();
    let adapter: Arc<dyn Adapter> = Arc::new(JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![root],
        live_dir: None,
    }));
    let opts = EngineOptions { cache_path: None, hot_poll: Duration::from_millis(50), ..Default::default() };
    let engine = Engine::new(vec![adapter], opts);
    engine.index();
    tokio::spawn(engine.clone().run());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = uniflo_gateway::router(engine, guard);
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));
    Srv { _dir: dir, file, addr }
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

async fn get(addr: std::net::SocketAddr, path: &str, extra: &[(&str, &str)]) -> Resp {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n", addr.port());
    for (k, v) in extra {
        if k.eq_ignore_ascii_case("host") {
            req = req.replace(&format!("Host: 127.0.0.1:{}", addr.port()), &format!("Host: {v}"));
        } else {
            req += &format!("{k}: {v}\r\n");
        }
    }
    req += "\r\n";
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

/// Open a streaming GET and return a line reader positioned at the body.
async fn open_stream(addr: std::net::SocketAddr, path: &str) -> BufReader<TcpStream> {
    open_stream_with(addr, path, &[]).await
}

async fn open_stream_with(addr: std::net::SocketAddr, path: &str, extra: &[(&str, &str)]) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let headers: String = extra.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost:{}\r\n{headers}\r\n", addr.port()).as_bytes())
        .await
        .unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    loop {
        line.clear();
        r.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            return r;
        }
    }
}

/// Next body line that contains `needle` (chunk-size lines are skipped naturally).
async fn wait_line(r: &mut BufReader<TcpStream>, needle: &str, within: Duration) -> String {
    let deadline = Instant::now() + within;
    let mut line = String::new();
    loop {
        line.clear();
        let left = deadline.saturating_duration_since(Instant::now());
        let n = tokio::time::timeout(left, r.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("timeout waiting for {needle}"))
            .unwrap();
        assert!(n > 0, "stream closed");
        if line.contains(needle) {
            return line;
        }
    }
}

fn append(p: &PathBuf, s: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
    f.write_all(s.as_bytes()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_endpoints_shapes_and_paging() {
    let s = start(GuardOptions::default()).await;
    let h = get(s.addr, "/v1/health", &[]).await;
    assert_eq!(h.status, 200);
    assert_eq!(h.json()["ok"], true);
    // No background check scheduled in tests: update fields present but inactive.
    assert_eq!(h.json()["update_available"], false);
    assert_eq!(h.json()["latest_version"], serde_json::Value::Null);

    let hs = get(s.addr, "/v1/harnesses", &[]).await.json();
    assert_eq!(hs[0]["id"], "claude");
    assert_eq!(hs[0]["sessions"], 2);

    let list = get(s.addr, "/v1/sessions?q=gateway", &[]).await;
    assert!(list.header("x-uniflo-seq").is_some());
    let v = list.json();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["key"], "claude:other");
    assert_eq!(v[0]["cwd"], "/w/demo");

    let nd = get(s.addr, "/v1/sessions?format=ndjson&q=h:claude", &[]).await;
    assert_eq!(nd.header("content-type"), Some("application/x-ndjson"));
    assert_eq!(nd.body.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count(), 2);

    let one = get(s.addr, "/v1/sessions/claude:sess-1", &[]).await.json();
    assert_eq!(one["status"], "idle");
    assert_eq!(one["preview"], "question 0");
    assert_eq!(get(s.addr, "/v1/sessions/claude:nope", &[]).await.status, 404);

    // Page backwards through 60 user/assistant messages (+30 turn ends).
    let mut before: Option<u64> = None;
    let mut texts = Vec::new();
    loop {
        let path = match before {
            Some(b) => format!("/v1/sessions/claude:sess-1/events?limit=25&before={b}"),
            None => "/v1/sessions/claude:sess-1/events?limit=25".into(),
        };
        let page = get(s.addr, &path, &[]).await.json();
        let evs = page["events"].as_array().unwrap().clone();
        assert!(!evs.is_empty(), "next_before must be null once the start is reached, not lead to an empty page");
        for e in evs.iter().rev() {
            if let Some(t) = e["text"].as_str() {
                texts.push(t.to_owned());
            }
        }
        before = page["next_before"].as_u64();
        if before.is_none() {
            break;
        }
    }
    assert_eq!(texts.len(), 60);
    assert_eq!(texts.last().unwrap(), "question 0");
    assert_eq!(texts.first().unwrap(), "answer 29");

    let trunc = get(s.addr, "/v1/sessions/claude:sess-1/events?limit=1&max_text=3", &[]).await.json();
    let last = trunc["events"].as_array().unwrap().iter().find(|e| e["kind"] == "assistant_message").unwrap().clone();
    assert_eq!(last["text"], "ans");
    assert_eq!(last["truncated"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ndjson_and_sse_streams_deliver_appends() {
    let s = start(GuardOptions::default()).await;
    let mut nd = open_stream(s.addr, "/v1/stream.ndjson?session=claude:sess-1&types=event").await;
    wait_line(&mut nd, r#""type":"hello""#, Duration::from_secs(2)).await;
    let mut sse = open_stream(s.addr, "/v1/stream?harness=claude&kinds=user_message").await;
    wait_line(&mut sse, "event: hello", Duration::from_secs(2)).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let t0 = Instant::now();
    append(&s.file, &user("live1", "streamed question"));
    let got = wait_line(&mut nd, "streamed question", Duration::from_secs(3)).await;
    let latency = t0.elapsed();
    let env: Value = serde_json::from_str(got.trim()).unwrap();
    assert_eq!(env["type"], "event");
    assert_eq!(env["event"]["session"], "claude:sess-1");
    assert_eq!(env["event"]["kind"], "user_message");
    let seq = env["seq"].as_u64().unwrap();

    wait_line(&mut sse, "event: event", Duration::from_secs(3)).await;
    let data = wait_line(&mut sse, "data:", Duration::from_secs(1)).await;
    assert!(data.contains("streamed question"));
    eprintln!("append→ndjson latency {latency:?}");
    assert!(latency < Duration::from_millis(500));

    // Reconnect with since=seq-1 replays exactly that envelope.
    let mut again = open_stream(s.addr, &format!("/v1/stream.ndjson?since={}&types=event", seq - 1)).await;
    let replay = wait_line(&mut again, "streamed question", Duration::from_secs(2)).await;
    assert!(replay.contains(&format!("\"seq\":{seq}")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_reconnect_resumes_from_last_event_id_not_stale_since() {
    let s = start(GuardOptions::default()).await;
    let mut nd = open_stream(s.addr, "/v1/stream.ndjson?types=event").await;
    wait_line(&mut nd, r#""type":"hello""#, Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    append(&s.file, &user("seen", "already delivered"));
    let got = wait_line(&mut nd, "already delivered", Duration::from_secs(3)).await;
    let seq = serde_json::from_str::<Value>(got.trim()).unwrap()["seq"].as_u64().unwrap();

    // A browser EventSource reconnects with its original URL (`since=0`) plus `Last-Event-ID`.
    let id = seq.to_string();
    let mut sse = open_stream_with(s.addr, "/v1/stream?since=0&types=event", &[("Last-Event-ID", &id)]).await;
    wait_line(&mut sse, "hello", Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    append(&s.file, &user("fresh", "after reconnect"));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut line = String::new();
    loop {
        line.clear();
        let left = deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(left, sse.read_line(&mut line)).await.expect("timeout").unwrap();
        assert!(!line.contains("already delivered"), "replayed an envelope before Last-Event-ID");
        if line.contains("after reconnect") {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demo_page_is_served() {
    let s = start(GuardOptions::default()).await;
    let r = get(s.addr, "/demo", &[]).await;
    assert_eq!(r.status, 200);
    let ct = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.as_str());
    assert!(ct.is_some_and(|v| v.starts_with("text/html")), "{ct:?}");
    let body = String::from_utf8_lossy(&r.body);
    for endpoint in
        ["/v1/health", "/v1/harnesses", "/v1/stats", "/v1/sessions", "/v1/stream", "/v1/ws", "/v1/stream.ndjson"]
    {
        assert!(body.contains(endpoint), "demo does not use {endpoint}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_delivers_envelopes() {
    let s = start(GuardOptions::default()).await;
    let mut sock = TcpStream::connect(s.addr).await.unwrap();
    let req = format!(
        "GET /v1/ws?types=event HTTP/1.1\r\nHost: localhost:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        s.addr.port()
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    assert!(line.contains("101"), "{line}");
    loop {
        line.clear();
        r.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
    }
    async fn frame(r: &mut BufReader<TcpStream>) -> String {
        let mut h = [0u8; 2];
        r.read_exact(&mut h).await.unwrap();
        let mut len = (h[1] & 0x7f) as usize;
        if len == 126 {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).await.unwrap();
            len = u16::from_be_bytes(b) as usize;
        } else if len == 127 {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).await.unwrap();
            len = u64::from_be_bytes(b) as usize;
        }
        let mut p = vec![0u8; len];
        r.read_exact(&mut p).await.unwrap();
        String::from_utf8(p).unwrap()
    }
    assert!(frame(&mut r).await.contains(r#""type":"hello""#));
    append(&s.file, &assistant("live2", "over websocket"));
    let msg = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let f = frame(&mut r).await;
            if f.contains("over websocket") {
                return f;
            }
        }
    })
    .await
    .expect("ws event");
    assert!(msg.contains(r#""type":"event""#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_blocks_foreign_hosts_origins_and_missing_tokens() {
    let s = start(GuardOptions { token: Some("s3cret".into()), ..Default::default() }).await;
    assert_eq!(get(s.addr, "/v1/health", &[]).await.status, 401);
    assert_eq!(get(s.addr, "/v1/health", &[("Authorization", "Bearer wrong")]).await.status, 401);
    assert_eq!(get(s.addr, "/v1/health", &[("Authorization", "Bearer s3cret")]).await.status, 200);
    assert_eq!(get(s.addr, "/v1/health?token=s3cret", &[]).await.status, 200);
    assert_eq!(get(s.addr, "/v1/health?token=s3cret", &[("Host", "evil.example:80")]).await.status, 403);
    assert_eq!(get(s.addr, "/v1/health?token=s3cret", &[("Origin", "https://evil.example")]).await.status, 403);
    let ok = get(s.addr, "/v1/health?token=s3cret", &[("Origin", "http://localhost:5173")]).await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.header("access-control-allow-origin"), Some("http://localhost:5173"));
    assert_eq!(ok.header("access-control-expose-headers"), Some("x-uniflo-seq"));
}
