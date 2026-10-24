//! One end-to-end test against a real gateway: a hand-written Claude Code session under a
//! sandboxed `UNIFLO_HOME`, served in-process by `uniflo-gateway` over a real TCP socket, driven
//! entirely through `uniflo_client::Client`. Synthetic data only — never a real session, the
//! running daemon on 7311, or the real cache (`AGENTS.md`'s testing rules).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uniflo_adapters::claude::claude;
use uniflo_client::{Client, EventsQuery, StreamOptions, UsageQuery};
use uniflo_core::{Adapter, Engine, EngineOptions};
use uniflo_gateway::GuardOptions;

/// One short conversation, in the real Claude Code JSONL shape (`crates/uniflo-adapters/src/claude.rs`
/// tests show the same `message.usage.*_tokens` field names).
fn write_session(home: &Path) -> PathBuf {
    let dir = home.join(".claude/projects/-tmp-uniflo-client-e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("sess-1.jsonl");
    let mut body = String::new();
    body += &format!(
        "{}\n",
        serde_json::json!({
            "type": "user", "uuid": "u0", "timestamp": "2026-10-24T09:00:00Z",
            "cwd": "/tmp/uniflo-client-e2e",
            "message": {"role": "user", "content": "缓存击穿怎么修？"},
        })
    );
    body += &format!(
        "{}\n",
        serde_json::json!({
            "type": "assistant", "uuid": "a0", "timestamp": "2026-10-24T09:00:01Z",
            "message": {
                "id": "a0", "model": "claude-x",
                "content": [{"type": "text", "text": "加一层互斥锁"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 120, "output_tokens": 40,
                    "cache_read_input_tokens": 5, "cache_creation_input_tokens": 0,
                },
            },
        })
    );
    std::fs::write(&file, body).unwrap();
    file
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_round_trips_every_read_route_against_a_real_gateway() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: this is the only test in this binary (one process per integration-test file under
    // cargo), so no other test observes this process-global env var concurrently.
    unsafe {
        std::env::set_var("UNIFLO_HOME", &home);
    }
    write_session(&home);

    let adapter: Arc<dyn Adapter> = Arc::new(claude());
    let opts = EngineOptions {
        cache_path: None,
        data_dir: Some(tmp.path().join("data")),
        hot_poll: Duration::from_millis(50),
        ..Default::default()
    };
    let engine = Engine::new(vec![adapter], opts);
    engine.index();
    // One-shot ledger build (no `run()` loop needed for a read-only round trip), matching
    // `docs/architecture.md`'s embedding guidance: "不跑 run() 时用 index_usage() 一次性建好用量账本".
    engine.index_usage();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = uniflo_gateway::router(engine, GuardOptions::default());
    tokio::spawn(uniflo_gateway::serve(listener, router, std::future::pending()));

    let client = Client::new(&format!("http://{addr}")).unwrap().with_timeout(Duration::from_secs(5));

    let health = client.health().unwrap();
    assert!(health.ok, "{health:?}");
    assert_eq!(health.sessions, 1);

    let harnesses = client.harnesses().unwrap();
    assert!(harnesses.iter().any(|h| h.id == "claude"), "{harnesses:?}");

    let sessions = client.sessions("", 10).unwrap();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let key = sessions[0].key.clone();
    assert_eq!(key, "claude:sess-1");

    let session = client.session(&key).unwrap();
    assert_eq!(session.key, key);
    assert_eq!(session.harness, "claude");

    let page = client.events(&key, &EventsQuery::default()).unwrap();
    assert_eq!(page.session, key);
    let has_user = page.events.iter().any(|e| {
        matches!(&e.body, uniflo_client::uniflo_schema::Body::UserMessage { text, .. } if text.contains("缓存击穿"))
    });
    let has_assistant = page.events.iter().any(|e| {
        matches!(&e.body, uniflo_client::uniflo_schema::Body::AssistantMessage { text, .. } if text.contains("互斥锁"))
    });
    assert!(has_user && has_assistant, "{:?}", page.events);

    let detail = client.session_usage(&key).unwrap();
    assert_eq!(detail.session, key);
    assert!(detail.steps.iter().any(|s| s.input == 120 && s.output == 40), "{:?}", detail.steps);

    let report = client.usage(&UsageQuery { group_by: Some("harness".into()), ..Default::default() }).unwrap();
    assert_eq!(report.group_by, "harness");
    assert!(report.rows.iter().any(|r| r.key == "claude" && r.steps >= 1), "{:?}", report.rows);

    let models = client.models().unwrap();
    assert!(models.iter().any(|m| m.model == "claude-x"), "{models:?}");

    let resume = client.resume(&key).unwrap();
    assert_eq!(resume.harness, "claude");
    assert!(resume.supported, "{resume:?}");

    // The router here has no FTS index attached (`router`, not `router_with_fts`), so `/v1/search`
    // answers 503 — a real, expected error path for this client, not something to skip.
    let err = client.search("缓存", &Default::default()).unwrap_err();
    match err {
        uniflo_client::Error::Http { status, .. } => assert_eq!(status, 503),
        other => panic!("expected a 503 Error::Http, got {other:?}"),
    }

    let mut stream = client.stream(&StreamOptions::default()).unwrap();
    match stream.next() {
        Some(Ok(uniflo_client::Envelope::Hello { .. })) => {}
        other => panic!("expected a hello envelope first, got {other:?}"),
    }
}
