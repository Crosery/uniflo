//! Families, cost parts and the call chain over real TCP: `Session.family` and its stream
//! envelopes, `cost_parts` / `price` in `/v1/sessions/{key}/usage`, and
//! `/v1/sessions/{key}/trajectory` on synthetic Claude files with a sub-agent sidecar.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use uniflo_adapters::claude::ClaudeFamily;
use uniflo_core::util::now_ms;
use uniflo_core::{Adapter, Cx, Engine, EngineOptions, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_gateway::GuardOptions;
use uniflo_schema::{Body, Usage};

/// Minimal harness with parents: `<id>^<parent>.jsonl`, each line one usage event
/// `{"id","ts","input","cost"?}` of `model-a`.
struct Tree {
    root: PathBuf,
}

impl LineDecoder for Tree {
    type State = ();

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "tree", name: "Tree" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl")
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let stem = p.file_stem()?.to_str()?;
        Some(match stem.split_once('^') {
            Some((id, parent)) => SourceId { id: id.into(), parent: Some(parent.into()) },
            None => SourceId { id: stem.into(), parent: None },
        })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let u = Usage {
            input: v["input"].as_u64().unwrap_or(0),
            output: v["output"].as_u64().unwrap_or(0),
            cache_read: v["cache_read"].as_u64().unwrap_or(0),
            cache_write: v["cache_write"].as_u64().unwrap_or(0),
            model: Some("model-a".into()),
            cost_usd: v["cost"].as_f64(),
            ..Default::default()
        };
        cx.emit(v["id"].as_str().unwrap_or("x"), v["ts"].as_i64().unwrap_or(0), Body::Usage(u));
    }
}

struct Srv {
    _dir: tempfile::TempDir,
    root: PathBuf,
    addr: std::net::SocketAddr,
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

async fn serve(dir: tempfile::TempDir, adapters: impl FnOnce(&Path) -> Vec<Arc<dyn Adapter>>) -> Srv {
    let root = dir.path().to_path_buf();
    let adapters = adapters(&root);
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

/// A temp dir with the test price catalog: `model-a` input 3 / output 15 / cache_read 0.3, no
/// cache_write price, and a tier above 200 000 prompt tokens.
fn workspace(sub: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for d in sub.iter().chain(&["data/pricing"]) {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let model = json!({"id":"model-a","provider":"test","context_limit":1000000,"prices":[{"from":0,
        "input":3.0,"output":15.0,"cache_read":0.3,
        "tiers":[{"above":200000,"input":6.0,"output":22.5,"cache_read":0.6}]}]});
    std::fs::write(dir.path().join("data/pricing/overrides.json"), json!([model]).to_string()).unwrap();
    dir
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
    (status, serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null))
}

fn u(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap_or_else(|| panic!("{k} missing in {v}"))
}

fn f(v: &Value, k: &str) -> f64 {
    v[k].as_f64().unwrap_or_else(|| panic!("{k} missing in {v}"))
}

fn tree_file(root: &Path, name: &str, lines: &[Value]) {
    std::fs::write(
        root.join("tree").join(format!("{name}.jsonl")),
        lines.iter().cloned().map(line).collect::<String>(),
    )
    .unwrap();
}

fn tree_adapters(root: &Path) -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(JsonlAdapter::new(Tree { root: root.join("tree") }))]
}

async fn sessions(addr: std::net::SocketAddr) -> BTreeMap<String, Value> {
    let (status, v) = get(addr, "/v1/sessions?limit=100").await;
    assert_eq!(status, 200, "{v}");
    v.as_array().unwrap().iter().map(|s| (s["key"].as_str().unwrap().to_owned(), s.clone())).collect()
}

/// Subscribe to `session` envelopes; returns the reader positioned after the response head.
async fn subscribe(addr: std::net::SocketAddr) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET /v1/stream.ndjson?types=session HTTP/1.1\r\nHost: localhost:{}\r\n\r\n", addr.port());
    s.write_all(req.as_bytes()).await.unwrap();
    let mut r = BufReader::new(s);
    let mut l = String::new();
    loop {
        l.clear();
        r.read_line(&mut l).await.unwrap();
        if l == "\r\n" {
            return r;
        }
    }
}

/// Read `session` envelopes until `want` of the `keys` were seen with `pred` true, or time runs out.
async fn envelopes_until(
    r: &mut BufReader<TcpStream>,
    within: Duration,
    keys: &[&str],
    pred: impl Fn(&Value) -> bool,
) -> BTreeMap<String, Value> {
    let mut seen: BTreeMap<String, Value> = BTreeMap::new();
    let mut l = String::new();
    let res = tokio::time::timeout(within, async {
        while keys.iter().any(|k| !seen.contains_key(*k)) {
            l.clear();
            assert!(r.read_line(&mut l).await.unwrap() > 0, "stream closed");
            let Some(json) = l.find('{').map(|i| &l[i..]) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(json.trim()) else { continue };
            let s = &v["session"];
            if let Some(k) = s["key"].as_str().filter(|k| keys.contains(k)).filter(|_| pred(s)) {
                seen.insert(k.to_owned(), s.clone());
            }
        }
    })
    .await;
    assert!(res.is_ok(), "envelopes missing within {within:?}: have {:?}, want {keys:?}", seen.keys());
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn family_sums_every_level_and_follows_a_grandchild_live() {
    let dir = workspace(&["tree"]);
    // Recent enough to be polled while hot, so the test does not wait on the file watcher.
    let t = now_ms() - 5_000;
    // P ─┬─ C1 ── C11,  P ── C2,  S alone.
    let cost = |input: u64| (input * 3 + 100 * 15) as f64 / 1e6;
    let inputs = [("P", 1000u64), ("C1^P", 2000), ("C2^P", 3000), ("C11^C1", 4000), ("S", 5000)];
    for (name, input) in inputs {
        tree_file(dir.path(), name, &[json!({"id":"s1","ts":t,"input":input,"output":100})]);
    }
    let srv = serve(dir, tree_adapters).await;
    let all = sessions(srv.addr).await;
    let usage_cost = |k: &str| f(&all[k]["usage"], "cost_usd");

    let p = &all["tree:P"]["family"];
    assert_eq!(u(p, "sessions"), 3);
    let four = ["tree:P", "tree:C1", "tree:C2", "tree:C11"];
    assert!((f(p, "cost_usd") - four.iter().map(|k| usage_cost(k)).sum::<f64>()).abs() < 1e-12);
    assert_eq!(u(p, "steps"), 4);
    assert_eq!(u(p, "input"), 1000 + 2000 + 3000 + 4000);
    assert_eq!(u(p, "output"), 400);
    let c1 = &all["tree:C1"]["family"];
    assert_eq!(u(c1, "sessions"), 1);
    assert!((f(c1, "cost_usd") - usage_cost("tree:C1") - usage_cost("tree:C11")).abs() < 1e-12, "{c1} {all:?}");
    for k in ["tree:C2", "tree:C11", "tree:S"] {
        assert!(all[k].get("family").is_none(), "{k}: {}", all[k]);
    }

    let mut stream = subscribe(srv.addr).await;
    let extra = (7000 * 3 + 50 * 15) as f64 / 1e6;
    let mut file = std::fs::OpenOptions::new().append(true).open(srv.root.join("tree/C11^C1.jsonl")).unwrap();
    file.write_all(line(json!({"id":"s2","ts":now_ms(),"input":7000,"output":50})).as_bytes()).unwrap();
    let t0 = Instant::now();
    let want = [("tree:C11", usage_cost("tree:C11")), ("tree:C1", f(c1, "cost_usd")), ("tree:P", f(p, "cost_usd"))];
    let keys: Vec<&str> = want.iter().map(|(k, _)| *k).collect();
    // C11's own envelope carries the new step; its ancestors' families carry the new amount.
    let seen = envelopes_until(&mut stream, Duration::from_secs(2), &keys, |s| {
        let (own, fam) = (s["usage"]["steps"].as_u64(), s["family"]["steps"].as_u64());
        match s["key"].as_str().unwrap() {
            "tree:C11" => own == Some(2),
            "tree:C1" => fam == Some(3),
            _ => fam == Some(5),
        }
    })
    .await;
    eprintln!("ancestors pushed after {} ms", t0.elapsed().as_millis());
    for (k, before) in want {
        let s = &seen[k];
        let now = if k == "tree:C11" { f(&s["usage"], "cost_usd") } else { f(&s["family"], "cost_usd") };
        assert!((now - before - extra).abs() < 1e-12, "{k}: {before} -> {now}");
    }
    assert_eq!(u(&seen["tree:P"]["family"], "sessions"), 3);
    assert!(
        (f(&seen["tree:P"]["family"], "cost_usd") - (cost(1000) + cost(2000) + cost(3000) + cost(4000) + extra)).abs()
            < 1e-12
    );
    // And the list agrees with the stream.
    let after = sessions(srv.addr).await;
    assert_eq!(after["tree:P"]["family"], seen["tree:P"]["family"]);
    assert!(after["tree:S"].get("family").is_none());

    // A sub-session that goes away leaves its parent's family; a new one joins it.
    std::fs::remove_file(srv.root.join("tree/C2^P.jsonl")).unwrap();
    let gone =
        envelopes_until(&mut stream, Duration::from_secs(3), &["tree:P"], |s| s["family"]["sessions"] == 2).await;
    assert_eq!(u(&gone["tree:P"]["family"], "steps"), 4);
    tree_file(&srv.root, "C3^P", &[json!({"id":"s1","ts":now_ms(),"input":10,"output":1})]);
    // First as a session without usage yet (counts as 0), then with its step.
    let joined = envelopes_until(&mut stream, Duration::from_secs(3), &["tree:P"], |s| s["family"]["steps"] == 5).await;
    assert_eq!(u(&joined["tree:P"]["family"], "sessions"), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_cost_parts_and_price_follow_the_pricing_path() {
    let dir = workspace(&["tree"]);
    let t = now_ms() - 60_000;
    tree_file(
        dir.path(),
        "C",
        &[
            json!({"id":"plain","ts":t,"input":1000,"output":500,"cache_read":10000,"cache_write":2000}),
            json!({"id":"big","ts":t+1,"input":150000,"output":100,"cache_read":60000}),
            json!({"id":"own","ts":t+2,"input":10,"output":10,"cost":0.5}),
        ],
    );
    let srv = serve(dir, tree_adapters).await;
    let (status, d) = get(srv.addr, "/v1/sessions/tree:C/usage").await;
    assert_eq!(status, 200, "{d}");
    let steps = d["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    for s in &steps[..2] {
        let sum: f64 = ["input", "output", "cache_read", "cache_write"].iter().map(|k| f(&s["cost_parts"], k)).sum();
        assert!((sum - f(s, "cost_usd")).abs() < 1e-9, "{s}");
    }
    let (plain, big, own) = (&steps[0], &steps[1], &steps[2]);
    assert_eq!(plain["price"], json!({"input":3.0,"output":15.0,"cache_read":0.3,"cache_write":3.75}));
    assert!((f(&plain["cost_parts"], "cache_write") - 2000.0 * 3.75 / 1e6).abs() < 1e-12);
    assert!((f(&plain["cost_parts"], "input") - 3000.0 / 1e6).abs() < 1e-12);
    assert_eq!(u(&big["price"], "tier_above"), 200_000);
    assert_eq!(
        (f(&big["price"], "input"), f(&big["price"], "cache_write")),
        (6.0, 7.5),
        "tier price, write = input × 1.25"
    );
    assert!((f(&big["cost_parts"], "cache_read") - 60000.0 * 0.6 / 1e6).abs() < 1e-12);
    assert_eq!((own["cost_source"].as_str(), f(own, "cost_usd")), (Some("harness"), 0.5));
    assert!(own.get("cost_parts").is_none() && own.get("price").is_none(), "{own}");
    assert!(plain["price"].get("tier_above").is_none());
}

// ------------------------------------------------------------------ trajectory

fn claude_user(uuid: &str, ts: i64, content: Value, extra: Value) -> String {
    let mut v =
        json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":"/w","message":{"role":"user","content":content}});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    line(v)
}

/// One content line of an API message; lines of a message share `mid` and repeat its usage.
fn claude_block(uuid: &str, ts: i64, mid: &str, block: Value, usage: [u64; 4]) -> String {
    line(json!({"type":"assistant","uuid":uuid,"timestamp":ts,"message":{"id":mid,"model":"model-a",
        "content":[block],"usage":{"input_tokens":usage[0],"output_tokens":usage[1],
        "cache_read_input_tokens":usage[2],"cache_creation_input_tokens":usage[3]}}}))
}

fn tool_result(uuid: &str, ts: i64, id: &str, out: &str, is_error: bool) -> String {
    claude_user(uuid, ts, json!([{"type":"tool_result","tool_use_id":id,"content":out,"is_error":is_error}]), json!({}))
}

fn claude_adapters(root: &Path) -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![root.join("claude")],
        live_dir: None,
    }))]
}

/// Turn 1: step 1 = thinking + text + a Bash call (one API message on three lines), step 2 = a
/// `Task` call that fails and started sub-agent `agent-a1`. Turn 2: a harness-injected message,
/// then step 3 = a Bash call with no result yet.
fn write_claude_session(root: &Path, t: i64) {
    let dir = root.join("claude/-w");
    std::fs::create_dir_all(dir.join("S/subagents")).unwrap();
    let long = "é".repeat(250);
    let mut s = claude_user("u1", t, json!(long), json!({}));
    s += &claude_block("m1a", t + 1000, "m1", json!({"type":"thinking","thinking":"hmm"}), [100, 1, 1000, 50]);
    s += &claude_block("m1b", t + 1500, "m1", json!({"type":"text","text":"Let me look."}), [100, 40, 1000, 50]);
    s += &claude_block(
        "m1c",
        t + 2000,
        "m1",
        json!({"type":"tool_use","id":"toolu_bash","name":"Bash","input":{"command":"ls -la"}}),
        [100, 40, 1000, 50],
    );
    s += &tool_result("r1", t + 3000, "toolu_bash", "total 8", false);
    s += &claude_block(
        "m2a",
        t + 4000,
        "m2",
        json!({"type":"tool_use","id":"toolu_task","name":"Task","input":{"description":"dig","prompt":"dig deep"}}),
        [10, 20, 1200, 0],
    );
    s += &tool_result("r2", t + 9000, "toolu_task", "agent crashed", true);
    s += &claude_user("u2", t + 10_000, json!("second prompt"), json!({}));
    s += &claude_user("inj", t + 10_500, json!("<system-reminder>be brief</system-reminder>"), json!({}));
    s += &claude_block(
        "m3a",
        t + 11_000,
        "m3",
        json!({"type":"tool_use","id":"toolu_run","name":"Bash","input":{"command":"sleep 100"}}),
        [5, 7, 3300, 0],
    );
    std::fs::write(dir.join("S.jsonl"), s).unwrap();

    let mut a = claude_user("a-u1", t + 5000, json!("dig deep"), json!({"isSidechain":true}));
    a += &claude_block("a-m1a", t + 6000, "a-m1", json!({"type":"text","text":"found"}), [30, 5, 0, 0]);
    std::fs::write(dir.join("S/subagents/agent-a1.jsonl"), a).unwrap();
    std::fs::write(
        dir.join("S/subagents/agent-a1.meta.json"),
        json!({"agentType":"general","toolUseId":"toolu_task","spawnDepth":1,"finished":true}).to_string(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trajectory_groups_turns_steps_and_items_and_hangs_the_subagent_on_its_call() {
    let dir = workspace(&["claude"]);
    let t = now_ms() - 120_000;
    write_claude_session(dir.path(), t);
    let srv = serve(dir, claude_adapters).await;

    let (status, tr) = get(srv.addr, "/v1/sessions/claude:S/trajectory").await;
    assert_eq!(status, 200, "{tr}");
    assert_eq!(tr["session"], "claude:S");
    let turns = tr["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "the injected message opened no turn: {tr}");
    assert_eq!((u(&turns[0], "turn"), u(&turns[1], "turn")), (1, 2));
    assert_eq!(turns[0]["prompt"].as_str().unwrap().chars().count(), 250, "under the 300 cap, counted in chars");
    assert_eq!(turns[0]["event"], "u1");
    assert_eq!(turns[1]["prompt"], "second prompt");
    assert_eq!(u(&turns[0], "started_at") as i64, t);
    assert!(u(&turns[0], "ended_at") >= t as u64 + 9000);

    // Same steps, same numbers as /usage, indexed from 1 across turns.
    let (_, usage) = get(srv.addr, "/v1/sessions/claude:S/usage").await;
    let usage_steps = usage["steps"].as_array().unwrap();
    assert_eq!(usage_steps.len(), 3, "three lines of m1 are one step");
    let steps: Vec<&Value> = turns.iter().flat_map(|t| t["steps"].as_array().unwrap()).collect();
    assert_eq!(steps.len(), 3);
    for (i, (s, us)) in steps.iter().zip(usage_steps).enumerate() {
        assert_eq!(u(s, "index"), i as u64 + 1);
        for k in [
            "event",
            "ts",
            "model",
            "input",
            "output",
            "cache_read",
            "cache_write",
            "reasoning",
            "cost_usd",
            "cost_source",
            "context_tokens",
            "context_pct",
            "cost_parts",
            "price",
        ] {
            assert_eq!(s[k], us[k], "step {} {k}", i + 1);
        }
    }
    assert_eq!(u(steps[0], "output"), 40, "the latest line of the message wins");
    assert_eq!((turns[0]["steps"].as_array().unwrap().len(), turns[1]["steps"].as_array().unwrap().len()), (2, 1));

    // Step 1: reasoning, text, a successful Bash call.
    let kinds = |s: &Value| {
        s["items"].as_array().unwrap().iter().map(|i| i["kind"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(kinds(steps[0]), ["reasoning", "text", "tool"]);
    let items = steps[0]["items"].as_array().unwrap();
    assert_eq!((items[0]["chars"].as_u64(), items[0]["event"].as_str()), (Some(3), Some("m1a#0")));
    assert_eq!((items[1]["chars"].as_u64(), items[1]["preview"].as_str()), (Some(12), Some("Let me look.")));
    let bash = &items[2];
    assert_eq!((bash["name"].as_str(), bash["call_id"].as_str()), (Some("Bash"), Some("toolu_bash")));
    assert_eq!(bash["input_preview"], "{\"command\":\"ls -la\"}");
    assert_eq!((bash["is_error"].as_bool(), bash["output_preview"].as_str()), (Some(false), Some("total 8")));
    assert_eq!(u(bash, "ended_at") as i64, t + 3000);
    assert!(bash.get("running").is_none() && bash.get("children").is_none(), "{bash}");
    assert_eq!(u(steps[0], "started_at") as i64, t, "first step starts with its turn");

    // Step 2: the failed Task call that started the sub-agent.
    assert_eq!(kinds(steps[1]), ["tool"]);
    let task = &steps[1]["items"][0];
    assert_eq!((task["is_error"].as_bool(), task["children"].clone()), (Some(true), json!(["claude:agent-a1"])));
    assert_eq!(u(steps[1], "started_at") as i64, t + 3000, "previous step's last record: the Bash result");
    let kids = tr["children"].as_array().unwrap();
    assert_eq!(kids.len(), 1);
    let k = &kids[0];
    assert_eq!(
        (k["key"].as_str(), k["call_id"].as_str(), k["step"].as_u64()),
        (Some("claude:agent-a1"), Some("toolu_task"), Some(2))
    );
    assert_eq!(u(&k["usage"], "steps"), 1);
    assert_eq!(k["status"], "idle");
    assert_eq!(k["title"], "dig deep", "no title: the first prompt names it");

    // Step 3 (turn 2): the call with no result.
    let run = &steps[2]["items"][0];
    assert_eq!((run["running"].as_bool(), run.get("ended_at")), (Some(true), None));
    assert!(run.get("is_error").is_none());
    assert_eq!(turns[1]["items"].as_array().unwrap().len(), 0);

    // Totals line up with the session list; the parent carries the family.
    let (_, s) = get(srv.addr, "/v1/sessions/claude:S").await;
    assert_eq!(u(&s["family"], "sessions"), 1);
    assert_eq!(s["parent_call"], Value::Null);
    let (_, a1) = get(srv.addr, "/v1/sessions/claude:agent-a1").await;
    assert_eq!((a1["parent"].as_str(), a1["parent_call"].as_str()), (Some("claude:S"), Some("toolu_task")));
    assert!(a1.get("family").is_none());

    assert_eq!(get(srv.addr, "/v1/sessions/claude:nope/trajectory").await.0, 404);
}
