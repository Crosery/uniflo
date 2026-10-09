//! Full-text index against real adapters over synthetic transcripts.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_adapters::gemini::Gemini;
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Engine, EngineOptions, HarnessInfo, JsonlAdapter};
use uniflo_schema::search::{SearchOrder, SearchResponse};
use uniflo_search::fts::{Fts, FtsOptions, SearchParams};

const DAY: i64 = 86_400_000;

fn claude_like(id: &'static str, root: PathBuf) -> Arc<dyn Adapter> {
    Arc::new(JsonlAdapter::new(ClaudeFamily { info: HarnessInfo { id, name: id }, roots: vec![root], live_dir: None }))
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        for d in ["claude/-w-demo", "qoder/-w-ops", "gemini/proj/chats"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        Env { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn fts_path(&self) -> PathBuf {
        self.path("cache/fts-v1.sqlite")
    }

    fn engine(&self) -> Arc<Engine> {
        let adapters = vec![
            claude_like("claude", self.path("claude")),
            claude_like("qoder", self.path("qoder")),
            Arc::new(JsonlAdapter::new(Gemini::new(self.path("gemini"), self.path("projects.json"))))
                as Arc<dyn Adapter>,
        ];
        let opts = EngineOptions { cache_path: None, hot_poll: Duration::from_millis(50), ..Default::default() };
        let engine = Engine::new(adapters, opts);
        engine.index();
        engine
    }

    fn fts(&self, engine: &Arc<Engine>, opts: FtsOptions) -> Fts {
        Fts::start(engine.clone(), FtsOptions { path: self.fts_path(), ..opts }).unwrap()
    }
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn user(uuid: &str, ts: i64, text: &str) -> String {
    line(json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":"/w/demo","message":{"role":"user","content":text}}))
}

fn assistant(uuid: &str, ts: i64, block: Value) -> String {
    line(json!({"type":"assistant","uuid":uuid,"timestamp":ts,"message":{"id":uuid,"model":"m","content":[block]}}))
}

fn tool_result(uuid: &str, ts: i64, call: &str, output: &str) -> String {
    line(json!({"type":"user","uuid":uuid,"timestamp":ts,"message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":call,"content":output}]}}))
}

fn search(fts: &Fts, q: &str) -> SearchResponse {
    search_with(fts, SearchParams { q: q.into(), ..Default::default() })
}

fn search_with(fts: &Fts, p: SearchParams) -> SearchResponse {
    fts.search(&p).unwrap_or_else(|e| panic!("{}: {e}", p.q))
}

fn sessions(r: &SearchResponse) -> Vec<&str> {
    r.results.iter().map(|s| s.session.as_str()).collect()
}

fn events(r: &SearchResponse) -> Vec<(&str, &str)> {
    r.results.iter().flat_map(|s| s.hits.iter().map(move |h| (s.session.as_str(), h.event.as_str()))).collect()
}

fn idle(fts: &Fts) {
    assert!(fts.wait_idle(Duration::from_secs(20)), "index did not settle");
}

/// Scenario: 中文与代码子串命中.
#[test]
fn chinese_code_and_truncated_tool_output() {
    let env = Env::new();
    let t = now_ms() - 60_000;
    let mut big = "x".repeat(3 * 1024);
    big.push_str(" needle3k ");
    big.push_str(&"y".repeat(5 * 1024 - 10));
    big.push_str(" needle8k ");
    big.push_str(&"z".repeat(10 * 1024 - big.len()));
    assert_eq!(big.len(), 10 * 1024);
    let body = [
        user("u1", t, "请帮我修复缓存击穿问题，热点 key 过期时数据库被打满"),
        assistant(
            "a1",
            t + 1,
            json!({"type":"tool_use","id":"c1","name":"Edit","input":{"file_path":"/w/demo/src/rel.rs","new_string":"fn parse_releases(raw: &str) -> Vec<Release> {"}}),
        ),
        tool_result("r1", t + 2, "c1", "error[E0382]: borrow of moved value: `raw`"),
        assistant("a2", t + 3, json!({"type":"tool_use","id":"c2","name":"Bash","input":{"command":"cat big.log"}})),
        tool_result("r2", t + 4, "c2", &big),
        assistant("a3", t + 5, json!({"type":"text","text":"缓存击穿已修复。"})),
    ]
    .concat();
    std::fs::write(env.path("claude/-w-demo/s1.jsonl"), body).unwrap();
    std::fs::write(env.path("claude/-w-demo/s2.jsonl"), user("v1", t, "unrelated session about logging")).unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);

    let r = search(&fts, "缓存击穿");
    assert_eq!(r.order, SearchOrder::Relevance);
    assert_eq!(sessions(&r), vec!["claude:s1"]);
    let mut ids: Vec<&str> = events(&r).into_iter().map(|(_, e)| e).collect();
    ids.sort();
    assert_eq!(ids, vec!["a3#0", "u1"]);
    assert!(r.results[0].hits.iter().all(|h| h.snippet.contains("\u{2}缓存击穿\u{3}")), "{:?}", r.results[0].hits);
    assert!(!r.indexing && r.progress.done == r.progress.total && r.progress.total == 2);

    let r = search(&fts, "parse_rel");
    assert_eq!(events(&r), vec![("claude:s1", "a1#0")]);
    assert_eq!(r.results[0].hits[0].kind, "tool_call");
    assert!(r.results[0].hits[0].snippet.contains("\u{2}parse_rel\u{3}"), "{:?}", r.results[0].hits[0].snippet);

    let r = search(&fts, "E0382");
    assert_eq!(events(&r), vec![("claude:s1", "r1#0")]);
    assert!(r.results[0].hits[0].snippet.contains("\u{2}E0382\u{3}"));

    let r = search(&fts, "needle3k");
    assert_eq!(events(&r), vec![("claude:s1", "r2#0")], "3 KB offset is inside the 4 KB cut");
    assert!(search(&fts, "needle8k").results.is_empty(), "8 KB offset is past the 4 KB cut");

    // Case-insensitive, short terms fall back to LIKE and order by time.
    assert_eq!(events(&search(&fts, "e0382")), vec![("claude:s1", "r1#0")]);
    let r = search(&fts, "缓存");
    assert_eq!(r.order, SearchOrder::Recent);
    let hits: Vec<&str> = r.results[0].hits.iter().map(|h| h.event.as_str()).collect();
    assert_eq!(hits, vec!["a3#0", "u1"], "newest first");
    assert!(r.results[0].hits[0].snippet.contains("\u{2}缓存\u{3}"));
    let r = search_with(
        &fts,
        SearchParams { q: "缓存击穿".into(), kinds: Some(vec!["user_message".into()]), ..Default::default() },
    );
    assert_eq!(events(&r), vec![("claude:s1", "u1")]);
    assert!(matches!(
        fts.search(&SearchParams { q: "-only".into(), ..Default::default() }),
        Err(uniflo_search::fts::SearchError::Query(_))
    ));
}

/// Scenario: 过滤、排除、短语与排序.
#[test]
fn filter_exclusion_phrase_and_recency() {
    let env = Env::new();
    let now = now_ms();
    std::fs::write(env.path("claude/-w-demo/today.jsonl"), user("t1", now - 60_000, "please deploy script now"))
        .unwrap();
    std::fs::write(env.path("qoder/-w-ops/old.jsonl"), user("o1", now - 10 * DAY, "please deploy script now")).unwrap();
    std::fs::write(
        env.path("claude/-w-demo/split.jsonl"),
        user("s1", now - 5 * DAY, "deploy the new script after the rollback plan"),
    )
    .unwrap();
    // Unrelated sessions keep "deploy" rare enough for a meaningful bm25 idf.
    for i in 0..10 {
        std::fs::write(env.path(&format!("claude/-w-demo/f{i}.jsonl")), user("x", now - DAY, "build and test only"))
            .unwrap();
    }
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);

    let r = search(&fts, "deploy");
    assert_eq!(r.total, 3);
    let order = sessions(&r);
    let pos = |k: &str| order.iter().position(|x| *x == k).unwrap();
    assert!(pos("claude:today") < pos("qoder:old"), "same text: today's session ranks first: {order:?}");
    assert!(r.results[pos("claude:today")].score > r.results[pos("qoder:old")].score);

    let f = |filter: &str| {
        search_with(&fts, SearchParams { q: "deploy".into(), filter: Some(filter.into()), ..Default::default() })
    };
    assert_eq!(sessions(&f("h:qoder")), vec!["qoder:old"]);
    assert_eq!(f("h:qoder").filter.as_deref(), Some("h:qoder"));
    assert_eq!(sessions(&f("since:2d")), vec!["claude:today"]);
    assert_eq!(f("in:demo !h:claude").total, 1);

    let mut phrase = sessions(&search(&fts, "\"deploy script\"")).into_iter().map(str::to_owned).collect::<Vec<_>>();
    phrase.sort();
    assert_eq!(phrase, vec!["claude:today", "qoder:old"], "phrase needs the words adjacent");
    let excl = search(&fts, "deploy -rollback");
    assert_eq!(excl.total, 2);
    assert!(!sessions(&excl).contains(&"claude:split"));

    let page = search_with(&fts, SearchParams { q: "deploy".into(), limit: 1, offset: 1, ..Default::default() });
    assert_eq!((page.total, page.results.len()), (3, 1));
    assert_eq!(page.results[0].session, r.results[1].session);
}

/// Short terms scan the newest sessions first, stop once the page is settled, and give up at the
/// time budget with what they found.
#[test]
fn short_terms_stop_early_and_within_their_budget() {
    let env = Env::new();
    let now = now_ms();
    // 30 sessions mention 缓存 three times each, c00 the most recent; only the oldest has 鳕鱼.
    for s in 0..30i64 {
        let t = now - (s + 1) * 600_000;
        let body: String = (0..3).map(|i| user(&format!("u{i}"), t + i, &format!("第 {i} 次排查缓存"))).collect();
        std::fs::write(env.path(&format!("claude/-w-demo/c{s:02}.jsonl")), body).unwrap();
    }
    std::fs::write(env.path("claude/-w-demo/old.jsonl"), user("o1", now - 30 * DAY, "晚饭吃鳕鱼")).unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);

    let page =
        |offset: usize| search_with(&fts, SearchParams { q: "缓存".into(), limit: 5, offset, ..Default::default() });
    let r = page(0);
    assert_eq!(r.order, SearchOrder::Recent);
    assert_eq!(sessions(&r), vec!["claude:c00", "claude:c01", "claude:c02", "claude:c03", "claude:c04"]);
    let hits: Vec<&str> = r.results[0].hits.iter().map(|h| h.event.as_str()).collect();
    assert_eq!(hits, vec!["u2", "u1", "u0"], "newest first");
    assert!(!r.partial);
    assert!(r.total < 30 && r.scanned_until.is_some(), "stopped early: {} sessions, {:?}", r.total, r.scanned_until);
    assert_eq!(sessions(&page(5)), vec!["claude:c05", "claude:c06", "claude:c07", "claude:c08", "claude:c09"]);

    // A rare term reads everything: complete, nothing cut.
    let r = search(&fts, "鳕鱼");
    assert_eq!(sessions(&r), vec!["claude:old"]);
    assert!(r.total == 1 && !r.partial && r.scanned_until.is_none(), "{r:?}");
    drop(fts);

    // Out of time before reaching it: whatever was found, flagged as partial.
    let fts = env.fts(&engine, FtsOptions { like_budget: Duration::ZERO, ..Default::default() });
    idle(&fts);
    let r = search(&fts, "鳕鱼");
    assert!(r.partial && r.results.is_empty() && r.scanned_until.is_some(), "{r:?}");
    // Terms of 3+ characters never take the budgeted scan.
    let r = search(&fts, "排查缓存");
    assert!(r.total == 30 && !r.partial && r.scanned_until.is_none(), "{r:?}");
}

/// Scenario: 增量更新与 partial 覆盖.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_appends_and_partial_overwrites() {
    let env = Env::new();
    let t = now_ms() - 60_000;
    let claude_file = env.path("claude/-w-demo/live.jsonl");
    std::fs::write(&claude_file, user("u1", t, "first message")).unwrap();
    let gemini_file = env.path("gemini/proj/chats/session-g1.jsonl");
    let header = line(json!({"sessionId":"g1","startTime":"2026-10-09T08:00:00Z"}));
    let call = |cmd: &str| {
        line(json!({"id":"m1","type":"gemini","timestamp":"2026-10-09T08:00:01Z","content":"",
            "toolCalls":[{"id":"c1","name":"run_shell_command","args":{"command":cmd},"status":"executing"}]}))
    };
    std::fs::write(&gemini_file, format!("{header}{}", call("echo alpha"))).unwrap();
    let engine = env.engine();
    tokio::spawn(engine.clone().run());
    let fts = tokio::task::block_in_place(|| {
        let fts = env.fts(&engine, FtsOptions::default());
        idle(&fts);
        fts
    });
    assert_eq!(events(&search(&fts, "alpha")), vec![("gemini:session-g1", "m1:c0")]);

    let poll = |q: &'static str, want: usize| {
        let t0 = Instant::now();
        loop {
            let n = search(&fts, q).results.iter().map(|s| s.hits.len()).sum::<usize>();
            if n == want {
                return t0.elapsed();
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "{q}: {n} hits, want {want}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    std::fs::OpenOptions::new()
        .append(true)
        .open(&claude_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, user("u2", t + 5, "the zephyrquartz crystal").as_bytes()))
        .unwrap();
    let took = tokio::task::block_in_place(|| poll("zephyrquartz", 1));
    assert!(took < Duration::from_secs(2), "new event searchable after {took:?}");
    assert_eq!(events(&search(&fts, "zephyrquartz")), vec![("claude:live", "u2")]);

    // Same event id streamed again with longer text: the row is replaced, not duplicated.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&gemini_file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, call("echo alphabet").as_bytes()))
        .unwrap();
    tokio::task::block_in_place(|| poll("alphabet", 1));
    assert_eq!(events(&search(&fts, "alphabet")), vec![("gemini:session-g1", "m1:c0")]);
    assert_eq!(events(&search(&fts, "alpha")), vec![("gemini:session-g1", "m1:c0")], "no stale partial hit");
    let snippet = search(&fts, "alpha").results[0].hits[0].snippet.replace(['\u{2}', '\u{3}'], "");
    assert!(snippet.contains("echo alphabet"), "{snippet:?}");

    // A removed source takes its rows with it.
    std::fs::remove_file(&claude_file).unwrap();
    engine.notify_path(claude_file.clone()).await;
    tokio::task::block_in_place(|| poll("zephyrquartz", 0));
}

fn many_sessions(env: &Env, sessions: usize, per: usize) {
    let t = now_ms() - DAY;
    for s in 0..sessions {
        let body: String = (0..per)
            .map(|i| user(&format!("u{i}"), t + i as i64, &format!("message {i} of batch{s} with marker{s}x{i}")))
            .collect();
        std::fs::write(env.path(&format!("claude/-w-demo/b{s}.jsonl")), body).unwrap();
    }
}

/// Scenario: 后台构建、格式升级与关闭 (format and restart half; the HTTP half is in the gateway tests).
#[test]
fn background_build_format_bump_and_incremental_restart() {
    let env = Env::new();
    many_sessions(&env, 40, 50);
    let engine = env.engine();

    // Throttled hard so the backlog is observable right after start.
    let fts = env.fts(&engine, FtsOptions { pause: 20.0, page: 100, ..Default::default() });
    let st = fts.status();
    assert!(st.indexing && st.progress.total == 40 && st.progress.done < 40, "{st:?}");
    assert!(!st.rebuilt);
    let r = search(&fts, "batch0");
    assert!(r.indexing && r.progress.total == 40);
    drop(fts);

    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);
    let st = fts.status();
    assert_eq!((st.progress.events, st.progress.done, st.progress.total), (2000, 40, 40));
    assert!(st.bytes > 0 && st.errors == 0, "{st:?}");
    assert_eq!(events(&search(&fts, "marker7x49")), vec![("claude:b7", "u49")]);
    drop(fts);
    let sentinel = |set: bool| {
        let c = rusqlite::Connection::open(env.fts_path()).unwrap();
        if set {
            c.execute("INSERT INTO meta (k, v) VALUES ('sentinel', '1')", []).unwrap();
        }
        c.query_row("SELECT count(*) FROM meta WHERE k = 'sentinel'", [], |r| r.get::<_, i64>(0)).unwrap()
    };
    assert_eq!(sentinel(true), 1);

    // Same format, nothing changed: no backlog, file kept.
    let fts = env.fts(&engine, FtsOptions::default());
    let st = fts.status();
    assert!(!st.indexing && !st.rebuilt && st.progress.done == 40, "{st:?}");
    drop(fts);
    assert_eq!(sentinel(false), 1);

    // One session grew while no index was running: only it is caught up.
    let f = env.path("claude/-w-demo/b3.jsonl");
    let mut body = std::fs::read_to_string(&f).unwrap();
    body.push_str(&user("u50", now_ms(), "late addition quasarflux"));
    std::fs::write(&f, body).unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    let st = fts.status();
    assert_eq!((st.progress.total, st.progress.total - st.progress.done), (40, 1), "{st:?}");
    idle(&fts);
    assert_eq!(fts.status().progress.events, 2001);
    assert_eq!(events(&search(&fts, "quasarflux")), vec![("claude:b3", "u50")]);
    drop(fts);

    // Format bump: the index is discarded and rebuilt from scratch.
    let fts = env.fts(&engine, FtsOptions { format: uniflo_search::fts::FTS_FORMAT + 1, ..Default::default() });
    let st = fts.status();
    assert!(st.rebuilt && st.indexing && st.progress.done == 0, "{st:?}");
    idle(&fts);
    assert_eq!(fts.status().progress.events, 2001);
    assert!(fts.status().build_ms.is_some());
    drop(fts);
    assert_eq!(sentinel(false), 0, "old file replaced");
}

#[test]
fn pinned_sessions_survive_reconciliation() {
    let env = Env::new();
    std::fs::write(env.path("claude/-w-demo/s.jsonl"), user("u1", now_ms(), "ordinary text")).unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions { reconcile: Duration::from_millis(50), ..Default::default() });
    idle(&fts);
    let mut archived = engine.session("claude:s").unwrap();
    archived.key = "claude:archived".into();
    archived.id = "archived".into();
    archived.title = Some("old work".into());
    let ev = uniflo_schema::Event {
        id: "x1".into(),
        session: archived.key.clone(),
        ts: now_ms(),
        pos: Some(1),
        partial: false,
        truncated: false,
        body: uniflo_schema::Body::UserMessage { text: "archived nebulawhisk".into(), synthetic: false },
    };
    fts.index_session(archived, vec![ev]);
    idle(&fts);
    std::thread::sleep(Duration::from_millis(200));
    let r = search(&fts, "nebulawhisk");
    assert_eq!(events(&r), vec![("claude:archived", "x1")]);
    assert_eq!(r.results[0].title.as_deref(), Some("old work"));
    assert_eq!(
        search_with(
            &fts,
            SearchParams { q: "nebulawhisk".into(), filter: Some("h:claude".into()), ..Default::default() }
        )
        .total,
        1
    );
    fts.remove_session("claude:archived");
    idle(&fts);
    assert!(search(&fts, "nebulawhisk").results.is_empty());
}
