//! Engine integration: real filesystem, real watcher, real timing.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;
use uniflo_core::{
    Adapter, Cx, Engine, EngineOptions, HarnessInfo, HistoryQuery, JsonlAdapter, LineDecoder, LiveSession, SourceId,
};
use uniflo_schema::{Body, Envelope, Status};

/// `{"t":"u"|"a"|"end"|"tool","x":..,"ts":..}` per line; file stem = session id.
struct Toy {
    root: PathBuf,
    live: Arc<Mutex<Option<Vec<LiveSession>>>>,
}

impl LineDecoder for Toy {
    type State = ();
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "toy", name: "Toy" }
    }
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
    fn is_source(&self, p: &Path) -> bool {
        p.starts_with(&self.root) && p.extension().is_some_and(|e| e == "jsonl")
    }
    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }
    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let ts = v["ts"].as_i64().unwrap_or(0);
        let x = v["x"].as_str().unwrap_or("").to_owned();
        let body = match v["t"].as_str() {
            Some("u") => Body::UserMessage { text: x, synthetic: false },
            Some("a") => Body::AssistantMessage { text: x, model: None },
            Some("tool") => Body::ToolCall { call_id: "c".into(), name: x, input: Value::Null },
            Some("end") => Body::TurnEnd { reason: None },
            _ => return,
        };
        cx.emit_at(ts, body);
    }
    fn live(&self) -> Option<Vec<LiveSession>> {
        self.live.lock().unwrap().clone()
    }
}

struct Harness {
    dir: tempfile::TempDir,
    engine: Arc<Engine>,
    live: Arc<Mutex<Option<Vec<LiveSession>>>>,
}

fn now() -> i64 {
    uniflo_core::util::now_ms()
}

fn line(t: &str, x: &str) -> String {
    format!("{{\"t\":\"{t}\",\"x\":\"{x}\",\"ts\":{}}}\n", now())
}

fn setup(cache: Option<PathBuf>, dir: Option<tempfile::TempDir>) -> Harness {
    let dir = dir.unwrap_or_else(|| tempfile::tempdir().unwrap());
    let live = Arc::new(Mutex::new(None));
    let adapter: Arc<dyn Adapter> =
        Arc::new(JsonlAdapter::new(Toy { root: dir.path().join("root"), live: live.clone() }));
    std::fs::create_dir_all(dir.path().join("root")).unwrap();
    let opts = EngineOptions {
        cache_path: cache,
        hot_poll: Duration::from_millis(50),
        live_poll: Duration::from_millis(50),
        rescan: Duration::from_millis(500),
        ..Default::default()
    };
    Harness { engine: Engine::new(vec![adapter], opts), dir, live }
}

impl Harness {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join("root").join(name)
    }
    fn append(&self, name: &str, s: &str) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.path(name)).unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
}

async fn next_matching(rx: &mut Receiver<Arc<Envelope>>, within: Duration, f: impl Fn(&Envelope) -> bool) -> Envelope {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(env)) if f(&env) => return (*env).clone(),
            Ok(Ok(_)) => continue,
            other => panic!("no matching envelope within {within:?}: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn appended_lines_stream_live_with_low_latency() {
    let h = setup(None, None);
    h.append("s1.jsonl", &line("u", "hello"));
    let report = h.engine.index();
    assert_eq!((report.files, report.sessions), (1, 1));
    let (_, mut rx, _) = h.engine.subscribe(None);
    tokio::spawn(h.engine.clone().run());
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut lat = Vec::new();
    for i in 0..5 {
        let t0 = Instant::now();
        h.append("s1.jsonl", &line("a", &format!("reply{i}")));
        let env = next_matching(&mut rx, Duration::from_secs(3), |e| matches!(e, Envelope::Event { .. })).await;
        lat.push(t0.elapsed());
        let Envelope::Event { event, .. } = env else { unreachable!() };
        assert_eq!(event.session, "toy:s1");
        assert!(matches!(event.body, Body::AssistantMessage { ref text, .. } if *text == format!("reply{i}")));
    }
    lat.sort();
    eprintln!("append→envelope latency: {lat:?}");
    assert!(lat[2] < Duration::from_millis(500), "median latency too high: {lat:?}");

    // Turn end flips status and publishes a session snapshot.
    h.append("s1.jsonl", &line("end", ""));
    let env = next_matching(
        &mut rx,
        Duration::from_secs(3),
        |e| matches!(e, Envelope::Session { session, .. } if session.status == Status::Idle),
    )
    .await;
    assert!(
        matches!(env, Envelope::Session { ref session, .. } if session.status_reason.as_deref() == Some("turn_end"))
    );
    let hist = h.engine.history("toy:s1", &HistoryQuery { before: None, limit: 100 }).unwrap();
    assert_eq!(hist.len(), 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_and_deleted_sessions_are_announced() {
    let h = setup(None, None);
    h.engine.index();
    let (_, mut rx, _) = h.engine.subscribe(None);
    tokio::spawn(h.engine.clone().run());
    tokio::time::sleep(Duration::from_millis(200)).await;

    h.append("fresh.jsonl", &line("u", "new session"));
    let env = next_matching(&mut rx, Duration::from_secs(3), |e| matches!(e, Envelope::Session { .. })).await;
    let Envelope::Session { session, .. } = env else { unreachable!() };
    assert_eq!(session.key, "toy:fresh");
    assert_eq!(session.status, Status::Work);
    assert_eq!(session.preview.as_deref(), Some("new session"));
    let env = next_matching(&mut rx, Duration::from_secs(3), |e| matches!(e, Envelope::Event { .. })).await;
    assert!(
        matches!(env, Envelope::Event { event, .. } if matches!(&event.body, Body::UserMessage { text, .. } if text == "new session"))
    );

    std::fs::remove_file(h.path("fresh.jsonl")).unwrap();
    next_matching(
        &mut rx,
        Duration::from_secs(3),
        |e| matches!(e, Envelope::Removed { key, .. } if key == "toy:fresh"),
    )
    .await;
    assert!(h.engine.session("toy:fresh").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_since_seq_is_gap_free() {
    let h = setup(None, None);
    h.append("r.jsonl", &line("u", "x"));
    h.engine.index();
    tokio::spawn(h.engine.clone().run());
    let (_, mut rx, _) = h.engine.subscribe(None);
    for i in 0..3 {
        h.append("r.jsonl", &line("a", &format!("{i}")));
        next_matching(&mut rx, Duration::from_secs(3), |e| matches!(e, Envelope::Event { event, .. } if matches!(&event.body, Body::AssistantMessage { text, .. } if *text == i.to_string()))).await;
    }
    let (backlog, _, complete) = h.engine.subscribe(Some(0));
    assert!(complete);
    let seqs: Vec<u64> = backlog.iter().map(|e| e.seq()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    let mid = seqs[seqs.len() / 2];
    let (tail, _, _) = h.engine.subscribe(Some(mid));
    assert_eq!(tail.first().map(|e| e.seq()), Some(mid + 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_exit_ends_work_and_pid_is_exposed() {
    let h = setup(None, None);
    h.append("p.jsonl", &line("tool", "long_build"));
    *h.live.lock().unwrap() = Some(vec![LiveSession { id: "p".into(), pid: 4242, status: None }]);
    h.engine.index();
    let s = h.engine.session("toy:p").unwrap();
    assert_eq!((s.status, s.pid), (Status::Work, Some(4242)));
    let (_, mut rx, _) = h.engine.subscribe(None);
    tokio::spawn(h.engine.clone().run());
    *h.live.lock().unwrap() = Some(Vec::new());
    let env = next_matching(&mut rx, Duration::from_secs(3), |e| matches!(e, Envelope::Session { .. })).await;
    let Envelope::Session { session, .. } = env else { unreachable!() };
    assert_eq!((session.status, session.pid, session.status_reason.as_deref()), (Status::Idle, None, Some("exited")));
}

#[test]
fn warm_restart_reads_only_new_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache/index.json");
    let h = setup(Some(cache.clone()), Some(dir));
    for i in 0..20 {
        h.append(&format!("s{i}.jsonl"), &line("u", &format!("q{i}")));
    }
    let r1 = h.engine.index();
    assert_eq!((r1.read, r1.restored, r1.sessions), (20, 0, 20));
    h.engine.save_cache().unwrap();

    h.append("s3.jsonl", &line("end", ""));
    let Harness { dir, .. } = h;
    let h2 = setup(Some(cache), Some(dir));
    let r2 = h2.engine.index();
    assert_eq!((r2.read, r2.restored, r2.sessions), (1, 19, 20));
    assert_eq!(h2.engine.session("toy:s3").unwrap().status, Status::Idle);
    assert_eq!(h2.engine.session("toy:s4").unwrap().preview.as_deref(), Some("q4"));
    assert_eq!(h2.engine.stats().bytes, line("end", "").len() as u64);
}

#[test]
fn stale_work_without_process_goes_idle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let old = now() - 3_600_000;
    std::fs::write(root.join("old.jsonl"), format!("{{\"t\":\"tool\",\"x\":\"x\",\"ts\":{old}}}\n")).unwrap();
    let adapter: Arc<dyn Adapter> = Arc::new(JsonlAdapter::new(Toy { root, live: Arc::new(Mutex::new(None)) }));
    let e = Engine::new(vec![adapter], EngineOptions { cache_path: None, ..Default::default() });
    e.index();
    let s = e.session("toy:old").unwrap();
    assert_eq!((s.status, s.status_reason.as_deref()), (Status::Idle, Some("stale")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filesystem_watcher_alone_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let adapter: Arc<dyn Adapter> =
        Arc::new(JsonlAdapter::new(Toy { root: root.clone(), live: Arc::new(Mutex::new(None)) }));
    let hour = Duration::from_secs(3600);
    let opts = EngineOptions { cache_path: None, hot_poll: hour, live_poll: hour, rescan: hour, ..Default::default() };
    let engine = Engine::new(vec![adapter], opts);
    engine.index();
    let (_, mut rx, _) = engine.subscribe(None);
    tokio::spawn(engine.clone().run());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    std::fs::write(root.join("w.jsonl"), line("u", "via fsevents")).unwrap();
    next_matching(&mut rx, Duration::from_secs(3), |e| matches!(e, Envelope::Event { .. })).await;
    eprintln!("watcher-only latency: {:?}", t0.elapsed());
}
