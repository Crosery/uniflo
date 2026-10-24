//! Full-text index against real adapters over synthetic transcripts.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
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
        self.path("cache/fts-v2.sqlite")
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

/// A session key's sorted set (order-independent comparisons for multi-term/phrase/exclusion/
/// kind/short-term parity: ADR-0011 only guarantees the same rows match across `hot_days`
/// configurations, not identical ordering once more than one term's idf is involved).
fn keyset(r: &SearchResponse) -> Vec<String> {
    let mut v: Vec<String> = sessions(r).into_iter().map(str::to_owned).collect();
    v.sort();
    v
}

/// Injectable clock for the writer (see [`FtsOptions::now`]): ages a session deterministically
/// without faking file mtimes.
fn now_clock(start: i64) -> (uniflo_search::fts::NowFn, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(start));
    let c = clock.clone();
    let now: uniflo_search::fts::NowFn = Arc::new(move || c.load(Ordering::SeqCst));
    (now, clock)
}

fn wait_for(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
    let t0 = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if t0.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Raw introspection against the live index file: safe to run concurrently with the writer
/// (readers never wait on WAL), matching how `Fts`'s own read pool works.
fn is_cold(env: &Env, key: &str) -> bool {
    let c = rusqlite::Connection::open(env.fts_path()).unwrap();
    uniflo_search::fts::store::load_sessions(&c).unwrap().into_iter().any(|(k, r)| k == key && r.cold)
}

fn session_missing(env: &Env, key: &str) -> bool {
    let c = rusqlite::Connection::open(env.fts_path()).unwrap();
    !uniflo_search::fts::store::load_sessions(&c).unwrap().into_iter().any(|(k, _)| k == key)
}

fn free_ratio(env: &Env) -> f64 {
    let c = rusqlite::Connection::open(env.fts_path()).unwrap();
    uniflo_search::fts::store::free_ratio(&c).unwrap()
}

fn page_count(env: &Env) -> i64 {
    let c = rusqlite::Connection::open(env.fts_path()).unwrap();
    c.pragma_query_value(None, "page_count", |r| r.get(0)).unwrap()
}

/// Best-effort `wal_checkpoint(TRUNCATE)` from a side connection, so file size reflects the
/// current page count regardless of the writer's own auto-checkpoint timing.
fn checkpoint(env: &Env) {
    let c = rusqlite::Connection::open(env.fts_path()).unwrap();
    for _ in 0..20 {
        if c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// A long term with a short one: the index narrows the rows, the short term filters them in the
/// same newest-first scan, with the same early stop and budget as a short term alone.
#[test]
fn mixed_terms_stop_early_and_within_their_budget() {
    let env = Env::new();
    let now = now_ms();
    // c00..c29 say "cache fn", c00 the most recent; five newer sessions say cache without fn; only
    // the oldest has zq.
    for s in 0..30i64 {
        let t = now - (s + 10) * 600_000;
        let body: String = (0..3).map(|i| user(&format!("u{i}"), t + i, &format!("cache fn step {i}"))).collect();
        std::fs::write(env.path(&format!("claude/-w-demo/c{s:02}.jsonl")), body).unwrap();
    }
    for s in 0..5i64 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/n{s}.jsonl")),
            user("u0", now - (s + 1) * 600_000, "cache only"),
        )
        .unwrap();
    }
    std::fs::write(env.path("claude/-w-demo/old.jsonl"), user("o1", now - 30 * DAY, "cache zq")).unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);

    let page = |offset: usize| {
        search_with(&fts, SearchParams { q: "cache fn".into(), limit: 5, offset, ..Default::default() })
    };
    let r = page(0);
    assert_eq!(r.order, SearchOrder::Recent);
    assert_eq!(sessions(&r), vec!["claude:c00", "claude:c01", "claude:c02", "claude:c03", "claude:c04"]);
    let hits: Vec<&str> = r.results[0].hits.iter().map(|h| h.event.as_str()).collect();
    assert_eq!(hits, vec!["u2", "u1", "u0"], "newest first");
    assert!(r.results[0].hits[0].snippet.contains("\u{2}fn\u{3}"), "{:?}", r.results[0].hits[0].snippet);
    assert!(!r.partial);
    assert!(r.total < 30 && r.scanned_until.is_some(), "stopped early: {} sessions, {:?}", r.total, r.scanned_until);
    assert_eq!(sessions(&page(5)), vec!["claude:c05", "claude:c06", "claude:c07", "claude:c08", "claude:c09"]);

    let r = search(&fts, "cache zq");
    assert_eq!(sessions(&r), vec!["claude:old"]);
    assert!(r.total == 1 && !r.partial && r.scanned_until.is_none(), "{r:?}");
    // A short exclusion filters the same scan.
    let r = search(&fts, "cache -fn");
    assert_eq!(r.order, SearchOrder::Recent);
    assert_eq!(sessions(&r), vec!["claude:n0", "claude:n1", "claude:n2", "claude:n3", "claude:n4", "claude:old"]);
    assert!(!r.partial && r.scanned_until.is_none(), "{r:?}");
    drop(fts);

    let fts = env.fts(&engine, FtsOptions { like_budget: Duration::ZERO, ..Default::default() });
    idle(&fts);
    let r = search(&fts, "cache zq");
    assert!(r.partial && r.results.is_empty() && r.scanned_until.is_some(), "{r:?}");
}

/// Equal scores page in one fixed order (score, then activity, then session key): no session
/// repeats or goes missing across pages, and asking again gives the same answer.
#[test]
fn equal_scores_page_in_a_stable_order() {
    let env = Env::new();
    let t = now_ms() - DAY;
    for s in 0..25 {
        std::fs::write(env.path(&format!("claude/-w-demo/e{s:02}.jsonl")), user("u1", t, "same tiebreak text 平局"))
            .unwrap();
    }
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);

    for q in ["tiebreak", "平局"] {
        let all = search_with(&fts, SearchParams { q: q.into(), limit: 200, ..Default::default() });
        let keys: Vec<String> = sessions(&all).into_iter().map(str::to_owned).collect();
        let want: Vec<String> = (0..25).map(|s| format!("claude:e{s:02}")).collect();
        assert_eq!(keys, want, "{q}: ties fall back to the session key");
        for _ in 0..3 {
            let paged: Vec<String> = (0..25)
                .step_by(7)
                .flat_map(|offset| {
                    let r = search_with(&fts, SearchParams { q: q.into(), limit: 7, offset, ..Default::default() });
                    sessions(&r).into_iter().map(str::to_owned).collect::<Vec<_>>()
                })
                .collect();
            assert_eq!(paged, keys, "{q}");
        }
    }
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
    // After the backlog: segments merged, pages read in.
    let t0 = Instant::now();
    while fts.status().warming {
        assert!(t0.elapsed() < Duration::from_secs(20), "still warming");
        std::thread::sleep(Duration::from_millis(20));
    }
    let c = rusqlite::Connection::open(env.fts_path()).unwrap();
    assert!(!uniflo_search::fts::store::scattered(&c).unwrap());
    drop(c);
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

/// Scenario: 同一语料在全热/混合/全冷三种 `hot_days` 配置下得到一致的搜索结果（ADR-0011）.
/// 单词查询的 idf 只统计热层，因此跨配置的绝对分数会不同，但单词查询的相对顺序不受影响（同一个
/// 常数乘子不改变排序）；多词/短语/排除/kind 过滤/短词查询只要求命中的会话集合与命中事件一致。
#[test]
fn search_parity_across_hot_mixed_and_cold_tiers() {
    let env = Env::new();
    let now = now_ms();
    let t = |days: f64| now - (days * DAY as f64) as i64;
    let dep = |text_user: &str, text_tool: &str, ts: i64| -> String {
        [
            user("u1", ts, text_user),
            assistant("a1", ts + 1, json!({"type":"tool_use","id":"c1","name":"Bash","input":{"command":text_tool}})),
        ]
        .concat()
    };
    // o1..o4 share identical text and a marker exclusive to them, at ages that all land on the
    // same side of the default 7-day boundary in "mixed" too (all > 7 days): bm25 ties among the
    // four, so only recency (hot tiebreak key, or the cold recency multiplier) orders them, and
    // because all four always move tier *together* across configs, that order cannot flip from
    // one config to another the way it could if some of the four were hot and others cold.
    for (name, age) in [("dep-o1", 8.0), ("dep-o2", 10.0), ("dep-o3", 15.0), ("dep-o4", 40.0)] {
        std::fs::write(
            env.path(&format!("claude/-w-demo/{name}.jsonl")),
            dep("deploy the cache rollback script now depordermarker", "deploy cache rollback script", t(age)),
        )
        .unwrap();
    }
    std::fs::write(
        env.path("claude/-w-demo/dep-norb.jsonl"),
        dep("deploy the cache script; later consider a rollback separately", "deploy cache script", t(5.0)),
    )
    .unwrap();
    std::fs::write(
        env.path("claude/-w-demo/dep-clean.jsonl"),
        dep("deploy the cache script, nothing else", "deploy cache script", t(6.0)),
    )
    .unwrap();
    // Keeps "deploy"'s idf meaningful instead of pegged at its floor; none of these match it.
    for i in 0..10 {
        std::fs::write(env.path(&format!("claude/-w-demo/filler{i}.jsonl")), user("x", t(1.0), "build and test only"))
            .unwrap();
    }
    let engine = env.engine();
    let configs: [(&str, u32); 3] =
        [("all_hot", 10_000), ("mixed", uniflo_search::fts::DEFAULT_HOT_DAYS), ("all_cold", 0)];
    let by_config: Vec<(&str, Fts)> = configs
        .into_iter()
        .map(|(name, hot_days)| {
            let fts = Fts::start(
                engine.clone(),
                FtsOptions { path: env.path(&format!("cache/{name}.sqlite")), hot_days, ..FtsOptions::default() },
            )
            .unwrap();
            idle(&fts);
            (name, fts)
        })
        .collect();

    let want_order = vec!["claude:dep-o1", "claude:dep-o2", "claude:dep-o3", "claude:dep-o4"];
    let mut ref_snippets: Option<Vec<Vec<String>>> = None;
    for (name, fts) in &by_config {
        let r = search(fts, "depordermarker");
        assert_eq!(sessions(&r), want_order, "{name}: single-term order");
        let snippets: Vec<Vec<String>> =
            r.results.iter().map(|s| s.hits.iter().map(|h| h.snippet.clone()).collect()).collect();
        match &ref_snippets {
            None => ref_snippets = Some(snippets),
            Some(want) => assert_eq!(&snippets, want, "{name}: snippets differ from all_hot"),
        }
    }

    let cases: Vec<(&str, SearchParams)> = vec![
        ("deploy rollback (AND)", SearchParams { q: "deploy rollback".into(), ..Default::default() }),
        ("\"rollback script\" (phrase)", SearchParams { q: "\"rollback script\"".into(), ..Default::default() }),
        ("deploy -rollback (exclusion)", SearchParams { q: "deploy -rollback".into(), ..Default::default() }),
        (
            "deploy kinds=tool_call",
            SearchParams { q: "deploy".into(), kinds: Some(vec!["tool_call".into()]), ..Default::default() },
        ),
        ("ca (short term)", SearchParams { q: "ca".into(), ..Default::default() }),
        ("deploy ca (mixed)", SearchParams { q: "deploy ca".into(), ..Default::default() }),
    ];
    for (label, params) in &cases {
        type SetAndHits = (Vec<String>, Vec<(String, String)>);
        let mut reference: Option<SetAndHits> = None;
        for (name, fts) in &by_config {
            let r = search_with(fts, params.clone());
            let keys = keyset(&r);
            let mut hits: Vec<(String, String)> =
                events(&r).into_iter().map(|(s, e)| (s.to_owned(), e.to_owned())).collect();
            hits.sort();
            match &reference {
                None => reference = Some((keys, hits)),
                Some((want_keys, want_hits)) => {
                    assert_eq!(&keys, want_keys, "{label} @ {name}: session set differs from all_hot");
                    assert_eq!(&hits, want_hits, "{label} @ {name}: hits differ from all_hot");
                }
            }
        }
    }
}

/// Scenario: 相关性排序遇到大量冷会话候选时提前停止分页，但仍能在预算内给出精确 total，并且分页结果
/// 与预算无关（稳定），通过测试专用计数器验证真的跳过了大部分冷层验证.
#[test]
fn early_termination_limits_cold_verification_and_matches_exact_total() {
    let env = Env::new();
    let now = now_ms();
    // 5 "near" sessions: recent, and the term repeated so their bm25 is high enough to set a
    // demanding threshold. 41 "far" sessions: much older, term appearing once, so their score
    // upper bound (idf·(k1+1)·recency) falls below that threshold once recency decays enough.
    // hot_days: 0 puts the whole corpus (even the near sessions) in the cold tier this test is
    // about, and keeps idf (hot-tier-only) a known constant (1.0, the hot tier is empty).
    for s in 0..5i64 {
        let text = format!("{} important recent session", "zephyrbatch ".repeat(5));
        std::fs::write(env.path(&format!("claude/-w-demo/near{s}.jsonl")), user("u1", now - (s + 1) * DAY, &text))
            .unwrap();
    }
    for s in 0..41i64 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/far{s:02}.jsonl")),
            user("u1", now - (200 + s) * DAY, "zephyrbatch handled long ago"),
        )
        .unwrap();
    }
    let engine = env.engine();

    let fts_exact = Fts::start(
        engine.clone(),
        FtsOptions { path: env.fts_path(), hot_days: 0, total_budget: Duration::from_secs(2), ..FtsOptions::default() },
    )
    .unwrap();
    idle(&fts_exact);
    let exact = search_with(&fts_exact, SearchParams { q: "zephyrbatch".into(), limit: 5, ..Default::default() });
    assert_eq!(exact.total, 46, "{exact:?}");
    assert!(!exact.partial && exact.scanned_until.is_none(), "{exact:?}");

    let fts_fast = Fts::start(
        engine.clone(),
        FtsOptions {
            path: env.path("cache/fast.sqlite"),
            hot_days: 0,
            total_budget: Duration::ZERO,
            // Batches of 4: at most 3 sessions verified past the stop point.
            verify_threads: 4,
            ..FtsOptions::default()
        },
    )
    .unwrap();
    idle(&fts_fast);
    let before = fts_fast.cold_verified_count();
    let fast = search_with(&fts_fast, SearchParams { q: "zephyrbatch".into(), limit: 5, ..Default::default() });
    let verified = fts_fast.cold_verified_count() - before;
    assert!(verified <= 10, "expected early stop to skip most of the 41 far candidates, verified {verified}");
    assert_eq!(sessions(&fast), sessions(&exact), "the page itself does not depend on the exactness budget");
    assert!(fast.total < 46 && fast.scanned_until.is_some(), "{fast:?}");

    // Repeating the same query is deterministic: no drift in the early-stopped page.
    let fast_again = search_with(&fts_fast, SearchParams { q: "zephyrbatch".into(), limit: 5, ..Default::default() });
    assert_eq!(sessions(&fast_again), sessions(&fast), "stable, repeatable paging under the exactness budget");
}

/// Scenario: 会话随时间推进完成 hot→cold 压缩、压缩后仍可搜索；新内容到来时提升回热层且不产生重复行；
/// 源文件被删除后两层都被清理.
#[test]
fn compaction_promotion_and_delete_move_rows_between_tiers() {
    let env = Env::new();
    let real_now = now_ms();
    let (now_fn, clock) = now_clock(real_now);
    let t0 = real_now - 60_000;
    let f = env.path("claude/-w-demo/age.jsonl");
    std::fs::write(&f, user("u1", t0, "an aging session about deploy script")).unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions { hot_days: 1, now: now_fn, ..FtsOptions::default() });
    idle(&fts);
    assert_eq!(sessions(&search(&fts, "deploy")), vec!["claude:age"]);
    assert!(!is_cold(&env, "claude:age"));

    // Advance the clock two days: the next idle turn compacts the session into the cold tier.
    clock.store(real_now + 2 * DAY, Ordering::SeqCst);
    assert!(wait_for(|| is_cold(&env, "claude:age"), Duration::from_secs(10)), "expected compaction to run");
    assert_eq!(sessions(&search(&fts, "deploy")), vec!["claude:age"], "still searchable cold");
    let snippet = search(&fts, "deploy").results[0].hits[0].snippet.clone();
    assert!(snippet.contains("deploy"), "{snippet}");

    // New content arrives (dated at the same advanced clock, so it counts as recent, not old):
    // the writer promotes the session back to hot; old and new text are both searchable and the
    // old rows are not duplicated.
    let mut body = std::fs::read_to_string(&f).unwrap();
    body.push_str(&user("u2", clock.load(Ordering::SeqCst), "promoted addition about zzyyqqrotation"));
    std::fs::write(&f, body).unwrap();
    drop(fts);
    let resumed_at = clock.load(Ordering::SeqCst);
    let engine = env.engine();
    let (now_fn, _clock) = now_clock(resumed_at);
    let fts = env.fts(&engine, FtsOptions { hot_days: 1, now: now_fn, ..FtsOptions::default() });
    idle(&fts);
    assert!(!is_cold(&env, "claude:age"), "promoted back to hot");
    assert_eq!(sessions(&search(&fts, "deploy")), vec!["claude:age"]);
    assert_eq!(sessions(&search(&fts, "zzyyqqrotation")), vec!["claude:age"]);
    assert_eq!(events(&search(&fts, "deploy")).len(), 1, "no duplicate row after promotion");

    // The source file disappears: a fresh engine snapshot no longer serves it, and the writer
    // removes it from both tiers.
    std::fs::remove_file(&f).unwrap();
    drop(fts);
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);
    assert!(search(&fts, "deploy").results.is_empty());
    assert!(session_missing(&env, "claude:age"));
}

/// Scenario: 全新构建时，已经超过 `hot_days` 的会话直接写入冷层，活跃会话保持热层（ADR-0011"全新构建"）.
#[test]
fn fresh_build_writes_old_sessions_straight_to_cold() {
    let env = Env::new();
    let now = now_ms();
    std::fs::write(env.path("claude/-w-demo/recent.jsonl"), user("u1", now - 3_600_000, "a recent session")).unwrap();
    std::fs::write(env.path("claude/-w-demo/aged.jsonl"), user("u1", now - 40 * DAY, "an already old session"))
        .unwrap();
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);

    assert!(!is_cold(&env, "claude:recent"), "active session stays hot on a fresh build");
    assert!(is_cold(&env, "claude:aged"), "a session already past hot_days goes straight to cold");
    let st = fts.status();
    assert!(st.hot_events.unwrap_or(0) > 0 && st.cold_events.unwrap_or(0) > 0, "{st:?}");
    assert_eq!(sessions(&search(&fts, "session")), vec!["claude:recent", "claude:aged"]);
}

/// Scenario: 短词(<3 字符)扫描跨越 hot/cold 边界的会话，保持与纯热层一致的 order/budget/partial/
/// scanned_until 语义；同一分页内可以同时包含热、冷两层的会话.
#[test]
fn short_term_scan_crosses_hot_and_cold_tiers() {
    let env = Env::new();
    let now = now_ms();
    // c00 is the most recent; ages cross the default 7-day hot/cold boundary around c22/c23.
    for s in 0..30i64 {
        let t = now - ((s + 1) as f64 * 0.3 * DAY as f64) as i64;
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
    assert_eq!(sessions(&r), vec!["claude:c00", "claude:c01", "claude:c02", "claude:c03", "claude:c04"]);
    assert!(!r.partial);
    assert!(r.total < 30 && r.scanned_until.is_some(), "stopped early: {} sessions, {:?}", r.total, r.scanned_until);

    assert!(is_cold(&env, "claude:c23") && !is_cold(&env, "claude:c22"), "expected a boundary between c22 and c23");
    let cross = page(20);
    assert_eq!(
        sessions(&cross),
        vec!["claude:c20", "claude:c21", "claude:c22", "claude:c23", "claude:c24"],
        "a page can straddle the tier boundary"
    );
    for s in &cross.results {
        assert!(!s.hits.is_empty() && s.hits[0].snippet.contains("缓存"), "{s:?}");
    }

    // A rare term read entirely from the cold tier: complete, nothing cut.
    let r = search(&fts, "鳕鱼");
    assert_eq!(sessions(&r), vec!["claude:old"]);
    assert!(is_cold(&env, "claude:old"));
    assert!(r.total == 1 && !r.partial && r.scanned_until.is_none(), "{r:?}");
    drop(fts);

    let fts = env.fts(&engine, FtsOptions { like_budget: Duration::ZERO, ..Default::default() });
    idle(&fts);
    let r = search(&fts, "鳕鱼");
    assert!(r.partial && r.results.is_empty() && r.scanned_until.is_some(), "{r:?}");
}

/// Scenario: 相同查询命中冷验证缓存，重复查询不必再次解压/验证；一次无关的冷层写入推进全局
/// `cold_gen`，使缓存失效，下一次相同查询重新验证全部候选.
#[test]
fn repeat_cold_query_is_cached_until_a_cold_write_invalidates_it() {
    let env = Env::new();
    let now = now_ms();
    for s in 0..8i64 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/flux{s}.jsonl")),
            user("u1", now - (s + 10) * DAY, "fluxcapacitor seen long ago"),
        )
        .unwrap();
    }
    for s in 0..2i64 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/filler{s}.jsonl")),
            user("x", now - DAY, "unrelated recent chat"),
        )
        .unwrap();
    }
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions::default());
    idle(&fts);
    for s in 0..8i64 {
        assert!(is_cold(&env, &format!("claude:flux{s}")), "flux{s} should start cold");
    }

    let before1 = fts.cold_verified_count();
    let r1 = search(&fts, "fluxcapacitor");
    let verified1 = fts.cold_verified_count() - before1;
    assert_eq!(verified1, 8, "first query verifies every cold candidate");
    assert_eq!(r1.total, 8);

    let before2 = fts.cold_verified_count();
    let r2 = search(&fts, "fluxcapacitor");
    let verified2 = fts.cold_verified_count() - before2;
    assert_eq!(verified2, 0, "repeat query hits the cold verification cache");
    assert_eq!(keyset(&r2), keyset(&r1));

    // An unrelated cold write bumps the global cold generation, invalidating the cache.
    let mut pinned = engine.session("claude:filler0").unwrap();
    pinned.key = "claude:pinned-trigger".into();
    pinned.id = "pinned-trigger".into();
    pinned.updated_at = now - 20 * DAY;
    let ev = uniflo_schema::Event {
        id: "p1".into(),
        session: pinned.key.clone(),
        ts: now - 20 * DAY,
        pos: Some(1),
        partial: false,
        truncated: false,
        body: uniflo_schema::Body::UserMessage { text: "unrelated pinned marker".into(), synthetic: false },
    };
    fts.index_session(pinned, vec![ev]);
    idle(&fts);
    assert!(is_cold(&env, "claude:pinned-trigger"));

    let before3 = fts.cold_verified_count();
    let r3 = search(&fts, "fluxcapacitor");
    let verified3 = fts.cold_verified_count() - before3;
    assert_eq!(verified3, 8, "a cold write invalidates the cache: every candidate is re-verified");
    assert_eq!(keyset(&r3), keyset(&r1));
}

/// Scenario: 压缩把大段热层文本搬进压缩冷块后，空闲轮次的 `incremental_vacuum` 回收空闲页，文件
/// 的页数在 checkpoint 之后真的变小.
#[test]
fn incremental_vacuum_shrinks_file_after_compaction() {
    let env = Env::new();
    let real_now = now_ms();
    let (now_fn, clock) = now_clock(real_now);
    for s in 0..20i64 {
        let filler = "deploy log line filler text ".repeat(1000); // ~29 KB per session.
        std::fs::write(
            env.path(&format!("claude/-w-demo/big{s}.jsonl")),
            user("u1", real_now - 60_000, &format!("{filler} marker{s}")),
        )
        .unwrap();
    }
    let engine = env.engine();
    let fts = env.fts(&engine, FtsOptions { hot_days: 1, now: now_fn, ..FtsOptions::default() });
    idle(&fts);
    checkpoint(&env);
    let before = page_count(&env);

    clock.store(real_now + 2 * DAY, Ordering::SeqCst);
    for s in 0..20i64 {
        assert!(
            wait_for(|| is_cold(&env, &format!("claude:big{s}")), Duration::from_secs(10)),
            "big{s} did not compact"
        );
    }
    // Matches production's own stop condition (`store::vacuum_step` reclaims while `r > 0.05`):
    // on a small file the ratio can land exactly on the boundary.
    assert!(
        wait_for(|| free_ratio(&env) <= 0.05, Duration::from_secs(15)),
        "expected incremental_vacuum to reclaim free pages: {}",
        free_ratio(&env)
    );
    checkpoint(&env);
    let after = page_count(&env);
    assert!(after < before, "expected the file to shrink after compaction + vacuum: {before} -> {after} pages");
}

/// Scenario: 压缩进行期间并发查询不 panic、不产生重复命中.
#[test]
fn search_survives_concurrent_compaction() {
    let env = Env::new();
    let real_now = now_ms();
    let (now_fn, clock) = now_clock(real_now);
    for s in 0..20i64 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/cc{s:02}.jsonl")),
            user("u1", real_now - (s + 1) * 60_000, "concurrentmarker steady state text"),
        )
        .unwrap();
    }
    let engine = env.engine();
    let fts = Arc::new(env.fts(&engine, FtsOptions { hot_days: 1, now: now_fn, ..FtsOptions::default() }));
    idle(&fts);
    assert_eq!(search(&fts, "concurrentmarker").total, 20);

    clock.store(real_now + 2 * DAY, Ordering::SeqCst);
    let searcher = {
        let fts = fts.clone();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut dup = false;
            while t0.elapsed() < Duration::from_secs(3) {
                let r = search(&fts, "concurrentmarker");
                let mut keys: Vec<&str> = sessions(&r);
                let before = keys.len();
                keys.sort();
                keys.dedup();
                dup |= keys.len() != before;
            }
            dup
        })
    };
    let dup = searcher.join().expect("search thread panicked");
    assert!(!dup, "a result listed the same session twice during concurrent compaction");
    for s in 0..20i64 {
        assert!(wait_for(|| is_cold(&env, &format!("claude:cc{s:02}")), Duration::from_secs(10)));
    }
    assert_eq!(search(&fts, "concurrentmarker").total, 20, "all still searchable after compaction");
}

type Scored = Vec<(String, f64, Vec<String>)>;

fn scored(r: &SearchResponse) -> Scored {
    r.results.iter().map(|s| (s.session.clone(), s.score, s.hits.iter().map(|h| h.event.clone()).collect())).collect()
}

/// Same sessions, order and hits; scores equal up to the recency weight's drift between the two
/// calls (it uses the wall clock at query time: about 1e-9 relative per millisecond).
fn assert_same_ranking(a: &SearchResponse, b: &SearchResponse, what: &str) {
    let (a, b) = (scored(a), scored(b));
    let strip = |v: &Scored| v.iter().map(|(s, _, h)| (s.clone(), h.clone())).collect::<Vec<_>>();
    assert_eq!(strip(&a), strip(&b), "{what}");
    for ((s, x, _), (_, y, _)) in a.iter().zip(&b) {
        assert!((x - y).abs() <= 1e-7 * x.abs().max(y.abs()), "{what}: {s} scored {x} vs {y}");
    }
}

/// Scenario: 冷层并行核验（多线程分批）与单线程逐个核验给出逐位一致的结果（会话、顺序、分数、命中、
/// total）；缓存命中后热层统计变化（新会话改变 idf 和平均长度），复用的冷层结果按当前统计重新计分，
/// 与一个没有缓存的新实例完全一致.
#[test]
fn parallel_cold_verification_and_cache_reuse_match_a_fresh_single_thread_run() {
    let env = Env::new();
    let now = now_ms();
    for s in 0..60i64 {
        let words = ["alpha", "beta", "gamma"][s as usize % 3];
        let text = format!(
            "{} parallelmarker {words} {}",
            "parallelmarker ".repeat(s as usize % 4),
            "pad ".repeat(s as usize)
        );
        let body = [
            user("u1", now - (s + 10) * DAY, &text),
            tool_result("t1", now - (s + 10) * DAY + 1, "c1", &format!("parallelmarker output {words}")),
        ]
        .concat();
        std::fs::write(env.path(&format!("claude/-w-demo/par{s:02}.jsonl")), body).unwrap();
    }
    std::fs::write(env.path("claude/-w-demo/hot0.jsonl"), user("u1", now - DAY, "parallelmarker alpha today")).unwrap();
    // Hot rows without the term keep its (hot-tier) idf off the floor, so the shift below moves it.
    for i in 0..10 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/filler{i}.jsonl")),
            user("x", now - DAY, "build and test only"),
        )
        .unwrap();
    }
    let engine = env.engine();
    let start = |name: &str, threads: usize| {
        let fts = Fts::start(
            engine.clone(),
            FtsOptions {
                path: env.path(&format!("cache/{name}.sqlite")),
                verify_threads: threads,
                total_budget: Duration::from_secs(5),
                ..FtsOptions::default()
            },
        )
        .unwrap();
        idle(&fts);
        fts
    };
    let one = start("one", 1);
    let many = start("many", 8);
    let queries: Vec<SearchParams> = vec![
        SearchParams { q: "parallelmarker".into(), limit: 7, ..Default::default() },
        SearchParams { q: "parallelmarker".into(), limit: 7, offset: 14, ..Default::default() },
        SearchParams { q: "parallelmarker alpha".into(), ..Default::default() },
        SearchParams { q: "\"parallelmarker output\"".into(), ..Default::default() },
        SearchParams { q: "parallelmarker -gamma".into(), ..Default::default() },
        SearchParams { q: "parallelmarker".into(), kinds: Some(vec!["tool_result".into()]), ..Default::default() },
        SearchParams { q: "parallelmarker".into(), kinds: Some(vec!["no_such_kind".into()]), ..Default::default() },
        SearchParams { q: "parallelmarker".into(), filter: Some("since:40d".into()), ..Default::default() },
    ];
    for p in &queries {
        let (a, b) = (search_with(&one, p.clone()), search_with(&many, p.clone()));
        assert_same_ranking(&a, &b, &format!("{p:?}"));
        assert_eq!((a.total, a.scanned_until), (b.total, b.scanned_until), "{p:?}");
    }
    assert_eq!(search_with(&one, queries[6].clone()).total, 0, "an unknown kind matches nothing");

    // Change the hot tier's statistics without touching the cold tier: the cached verification
    // is reused (nothing re-verified) yet scores follow the new statistics.
    let shift = |fts: &Fts| {
        let mut extra = engine.session("claude:hot0").unwrap();
        extra.key = "claude:statsshift".into();
        extra.id = "statsshift".into();
        let ev = uniflo_schema::Event {
            id: "s1".into(),
            session: extra.key.clone(),
            ts: now - DAY,
            pos: Some(1),
            partial: false,
            truncated: false,
            body: uniflo_schema::Body::UserMessage {
                text: format!("{} statsshift", "parallelmarker ".repeat(30)),
                synthetic: false,
            },
        };
        fts.index_session(extra, vec![ev]);
        idle(fts);
        assert_eq!(search(fts, "statsshift").total, 1);
    };
    shift(&many);
    let fresh = start("fresh", 1);
    shift(&fresh);
    for p in &queries {
        let before = many.cold_verified_count();
        let cached = search_with(&many, p.clone());
        assert_eq!(many.cold_verified_count(), before, "{p:?}: served from the cold cache");
        let want = search_with(&fresh, p.clone());
        assert_same_ranking(&cached, &want, &format!("{p:?}: cached cold results rescored"));
        assert_eq!(cached.total, want.total, "{p:?}");
    }
}

/// Scenario: 冷层查询词含单引号、需要 Unicode 折叠的字符、组合字符和表情时，与全热配置命中同一批
/// 会话（冷层候选块查询曾把词直接拼进 SQL 字符串，单引号会让查询报错）.
#[test]
fn cold_terms_with_quotes_and_unicode_match_like_the_hot_tier() {
    let env = Env::new();
    let now = now_ms();
    for (i, text) in [
        "please don't stop the deploy",
        "ISTANBUL İstanbul trip notes",
        "STRASSE straße sign",
        "ΣΙΓΜΑ σιγμα letters",
        "fullwidth ＡＢＣ text",
        "emoji 👍🏽 approved",
        "e\u{301}cole combining",
        "it's a quote ' alone",
        "ſéſtrange folded long s",
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(
            env.path(&format!("claude/-w-demo/uni{i}.jsonl")),
            user("u1", now - (20 + i as i64) * DAY, text),
        )
        .unwrap();
    }
    let engine = env.engine();
    let open = |name: &str, hot_days: u32| {
        let fts = Fts::start(
            engine.clone(),
            FtsOptions { path: env.path(&format!("cache/{name}.sqlite")), hot_days, ..FtsOptions::default() },
        )
        .unwrap();
        idle(&fts);
        fts
    };
    let hot = open("hot", 10_000);
    let cold = open("cold", 0);
    for q in [
        "don't",
        "\"don't stop\"",
        "it's",
        "istanbul",
        "straße",
        "σιγμα",
        "ａｂｃ",
        "👍🏽 approved",
        "e\u{301}cole",
        "strange",
    ] {
        let (h, c) = (search(&hot, q), search(&cold, q));
        assert_eq!(keyset(&c), keyset(&h), "{q}");
        assert_eq!(events(&c), events(&h), "{q}");
    }
    assert_eq!(search(&cold, "don't").total, 1);
    // `ſ` folds to `s` (one byte shorter): the cold snippet still highlights the original text.
    let strange = search(&cold, "strange");
    assert_eq!(strange.total, 1);
    assert!(strange.results[0].hits[0].snippet.contains("\u{2}ſtrange\u{3}"), "{strange:?}");
}

/// Scenario: 版本号变化触发全量刷新时，内容没变的冷会话（压缩时按到达顺序编号，刷新按新到旧编号）
/// 不会被整块重写：冷层代号 `cold_gen` 不变.
#[test]
fn version_refresh_leaves_unchanged_compacted_sessions_alone() {
    let env = Env::new();
    let real_now = now_ms();
    let (now_fn, clock) = now_clock(real_now);
    let file = env.path("claude/-w-demo/renum.jsonl");
    std::fs::write(
        &file,
        [user("u1", real_now - 60_000, "renumbered first"), user("u2", real_now - 50_000, "second")].concat(),
    )
    .unwrap();
    let opts = || FtsOptions { hot_days: 1, now: now_fn.clone(), ..FtsOptions::default() };
    let cold_gen = || {
        let c = rusqlite::Connection::open(env.fts_path()).unwrap();
        uniflo_search::fts::store::counters(&c).unwrap().cold_gen
    };
    drop({
        let fts = env.fts(&env.engine(), opts());
        idle(&fts);
        fts
    });
    // An incremental catch-up numbers the new row after the backfilled ones, unlike a fresh
    // newest-first pass.
    let mut body = std::fs::read_to_string(&file).unwrap();
    body.push_str(&user("u3", real_now - 40_000, "renumbered third"));
    std::fs::write(&file, body).unwrap();
    let engine = env.engine();
    drop({
        let fts = env.fts(&engine, opts());
        idle(&fts);
        assert_eq!(events(&search(&fts, "renumbered")).len(), 2);
        clock.store(real_now + 2 * DAY, Ordering::SeqCst);
        assert!(wait_for(|| is_cold(&env, "claude:renum"), Duration::from_secs(10)), "compacted");
        idle(&fts);
        fts
    });
    let before = cold_gen();
    {
        let c = rusqlite::Connection::open(env.fts_path()).unwrap();
        c.execute("UPDATE meta SET v = '0.0.0-old' WHERE k = 'version'", []).unwrap();
    }
    let fts = env.fts(&engine, opts());
    idle(&fts);
    assert_eq!(cold_gen(), before, "refresh found the same content and did not rewrite the blocks");
    assert!(is_cold(&env, "claude:renum"));
    assert_eq!(events(&search(&fts, "renumbered")).len(), 2);
}

const V1_SCHEMA: &str = "CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE sess (sid INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE, done_upd INTEGER, done_pos INTEGER,
    pinned INTEGER NOT NULL DEFAULT 0, meta TEXT);
CREATE TABLE docs (id INTEGER PRIMARY KEY, sid INTEGER NOT NULL, event TEXT NOT NULL, ts INTEGER NOT NULL,
    pos INTEGER, text TEXT NOT NULL, UNIQUE (sid, event));
CREATE VIRTUAL TABLE docs_fts USING fts5(text, content='docs', content_rowid='id', tokenize='trigram case_sensitive 0');";

/// Scenario: 旧版 `fts-v1.sqlite` 由后台线程迁移（`Fts::start` 不等迁移）；完成后删除旧文件、记录并持久化
/// 释放的字节数；迁移来的内容随后按源文件刷新（只在旧索引里的词消失），引擎里已不存在的会话被清理.
#[test]
fn v1_index_migrates_in_the_background_then_refreshes_from_sources() {
    let env = Env::new();
    let now = now_ms();
    let age = |s: i64| if s % 2 == 0 { DAY } else { 30 * DAY };
    for s in 0..40i64 {
        std::fs::write(
            env.path(&format!("claude/-w-demo/mig{s:02}.jsonl")),
            user("u1", now - age(s), "migratedmarker current"),
        )
        .unwrap();
    }
    let engine = env.engine();
    let old_path = env.path("cache/fts-v1.sqlite");
    std::fs::create_dir_all(old_path.parent().unwrap()).unwrap();
    {
        let c = rusqlite::Connection::open(&old_path).unwrap();
        c.execute_batch(V1_SCHEMA).unwrap();
        c.execute("INSERT INTO meta (k, v) VALUES ('tag', 'uniflo-fts/format1/uniflo0.1.0/schema1')", []).unwrap();
        let user_kind = uniflo_search::fts::store::kind_code("user_message").unwrap();
        let keys: Vec<(String, i64)> = (0..40i64)
            .map(|s| (format!("claude:mig{s:02}"), now - age(s)))
            .chain([("claude:gone".to_owned(), now)])
            .collect();
        for (i, (key, ts)) in keys.iter().enumerate() {
            let sid = i as i64 + 1;
            let id = uniflo_search::fts::store::row_id(sid, 0, user_kind);
            let text = "migratedmarker legacyonlyword";
            c.execute("INSERT INTO sess (sid, key, done_upd) VALUES (?1, ?2, ?3)", rusqlite::params![sid, key, ts])
                .unwrap();
            c.execute(
                "INSERT INTO docs (id, sid, event, ts, text) VALUES (?1, ?2, 'u1', ?3, ?4)",
                rusqlite::params![id, sid, ts, text],
            )
            .unwrap();
            c.execute("INSERT INTO docs_fts (rowid, text) VALUES (?1, ?2)", rusqlite::params![id, text]).unwrap();
        }
    }
    let old_bytes = std::fs::metadata(&old_path).unwrap().len();

    let t0 = Instant::now();
    let fts = env.fts(&engine, FtsOptions::default());
    let started = t0.elapsed();
    assert!(wait_for(|| !fts.status().migrating, Duration::from_secs(20)), "migration finished");
    idle(&fts);
    assert!(started < Duration::from_secs(5), "start does not wait for the migration: {started:?}");
    let st = fts.status();
    assert_eq!(st.reclaimed_bytes, old_bytes, "{st:?}");
    assert!(!old_path.exists(), "old index deleted");
    assert_eq!(search(&fts, "migratedmarker").total, 40);
    assert_eq!(search(&fts, "legacyonlyword").total, 0, "migrated rows refreshed from their sources");
    assert!(session_missing(&env, "claude:gone"), "a session the engine no longer has is dropped");
    assert!(is_cold(&env, "claude:mig01") && !is_cold(&env, "claude:mig00"), "tiers by age");
    drop(fts);
    let fts = env.fts(&engine, FtsOptions::default());
    assert_eq!(fts.status().reclaimed_bytes, old_bytes, "kept across restarts");
}
