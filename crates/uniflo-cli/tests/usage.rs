//! `uniflo usage` / `uniflo pricing` against a real `uniflo daemon` on a free loopback
//! port, with synthetic Claude sessions under a temporary home and a local price source.
//! Nothing outside the temp directory is read or written, and nothing goes to the network.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Minimal HTTP server standing in for models.dev: a settable status + body, counted hits.
struct Mock {
    url: String,
    reply: Arc<Mutex<(u16, String)>>,
    hits: Arc<AtomicUsize>,
}

impl Mock {
    fn start() -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api.json", l.local_addr().unwrap());
        let reply = Arc::new(Mutex::new((200, "{}".to_owned())));
        let hits = Arc::new(AtomicUsize::new(0));
        let (r, h) = (reply.clone(), hits.clone());
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut rd = BufReader::new(&s);
                let mut line = String::new();
                while rd.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                h.fetch_add(1, Ordering::SeqCst);
                let (status, body) = r.lock().unwrap().clone();
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = (&s).write_all(head.as_bytes()).and_then(|_| (&s).write_all(body.as_bytes()));
            }
        });
        Mock { url, reply, hits }
    }

    fn set(&self, status: u16, body: Value) {
        *self.reply.lock().unwrap() = (status, body.to_string());
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// models.dev shape with Anthropic models `claude-test-m` and `claude-test-o`.
fn catalog(m_output: f64) -> Value {
    json!({"anthropic": {"models": {
        "claude-test-m": {"cost": {"input": 3, "output": m_output, "cache_read": 0.3}, "limit": {"context": 200000, "output": 8000}},
        "claude-test-o": {"cost": {"input": 1, "output": 1, "cache_read": 0.1}, "limit": {"context": 100000, "output": 8000}}
    }}})
}

struct Home {
    dir: tempfile::TempDir,
    mock: Mock,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/projects/-w")).unwrap();
        Home { dir, mock: Mock::start() }
    }

    fn session(&self) -> PathBuf {
        self.dir.path().join(".claude/projects/-w/s1.jsonl")
    }

    fn pricing_dir(&self) -> PathBuf {
        self.dir.path().join("Library/Application Support/uniflo/pricing")
    }

    /// Everything a child sees: only the temp home, the mock source, no fallback source.
    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("UNIFLO_HOME", self.dir.path())
            .env("UNIFLO_DATA_DIR", self.pricing_dir().parent().unwrap())
            .env("UNIFLO_PRICING_URL", &self.mock.url)
            .env("UNIFLO_PRICING_FALLBACK_URL", "")
            .env("NO_COLOR", "1");
        c
    }

    fn daemon(&self, extra: &[&str]) -> Daemon {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bind = format!("127.0.0.1:{port}");
        let child = self
            .cmd()
            .args(["daemon", "--bind", &bind, "--no-cache", "--no-update-check"])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon { child, url: format!("http://{bind}"), port };
        wait("daemon health", Duration::from_secs(20), || d.get("/v1/health").0 == 200);
        wait("usage index", Duration::from_secs(20), || d.get("/v1/usage").1["indexing"]["ready"] == true);
        d
    }

    fn run(&self, d: Option<&Daemon>, args: &[&str]) -> Output {
        let mut c = self.cmd();
        if let Some(d) = d {
            c.args(["--url", &d.url]);
        }
        c.args(args).output().unwrap()
    }

    fn json(&self, d: Option<&Daemon>, args: &[&str]) -> Value {
        let out = self.run(d, args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

struct Daemon {
    child: Child,
    url: String,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn get(&self, path: &str) -> (u16, Value) {
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", self.port)) else { return (0, Value::Null) };
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", self.port);
        s.write_all(req.as_bytes()).unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { return (0, Value::Null) };
        let status = String::from_utf8_lossy(&raw[..split]).split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut body = raw[split + 4..].to_vec();
        if String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase().contains("transfer-encoding: chunked") {
            body = dechunk(&body);
        }
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
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

fn wait(what: &str, within: Duration, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < within, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn user(uuid: &str, ts: i64) -> String {
    line(json!({"type":"user","uuid":uuid,"timestamp":ts,"cwd":"/w","message":{"role":"user","content":"go"}}))
}

fn step(id: &str, ts: i64, model: &str, input: u64, output: u64) -> String {
    line(json!({"type":"assistant","uuid":format!("{id}-l"),"timestamp":ts,"message":{"id":id,"model":model,
        "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":input,"output_tokens":output,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}))
}

fn append(p: &Path, s: &str) {
    std::fs::OpenOptions::new().append(true).open(p).unwrap().write_all(s.as_bytes()).unwrap();
}

fn entry<'a>(cat: &'a Value, id: &str) -> &'a Value {
    cat["models"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("{id} not in catalog"))
}

fn read_catalog(h: &Home) -> Value {
    serde_json::from_slice(&std::fs::read(h.pricing_dir().join("catalog.json")).unwrap()).unwrap()
}

fn step_costs(d: &Daemon) -> Vec<Option<f64>> {
    d.get("/v1/sessions/claude:s1/usage").1["steps"]
        .as_array()
        .map(|a| a.iter().map(|s| s["cost_usd"].as_f64()).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
fn inode(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().ino()
}

#[test]
fn cli_and_rest_agree_and_table_has_cost_columns() {
    let h = Home::new();
    let t = now_ms() - 3_600_000;
    let mut s = user("u1", t) + &step("m1", t + 1000, "claude-sonnet-4-5", 1200, 300);
    s += &step("m2", t + 2000, "mystery-model-x", 50, 5);
    s += &user("u2", t + 3000);
    s += &step("m3", t + 4000, "gpt-5", 100, 10);
    std::fs::write(h.session(), s).unwrap();
    let d = h.daemon(&["--no-price-sync"]);

    let rest = d.get("/v1/usage?group_by=model").1;
    let cli = h.json(Some(&d), &["usage", "--by", "model", "--json"]);
    assert_eq!(cli, rest);
    assert_eq!(rest["totals"]["steps"], 3);
    assert_eq!(rest["totals"]["unpriced_steps"], 1);
    let rest = d.get("/v1/sessions/claude:s1/usage").1;
    assert_eq!(h.json(Some(&d), &["usage", "claude:s1", "--json"]), rest);
    assert_eq!(rest["steps"].as_array().unwrap().len(), 3);

    // In-process (no daemon) answers from the same library code.
    let local = h.json(None, &["--local", "usage", "--by", "model", "--json"]);
    let rest = d.get("/v1/usage?group_by=model").1;
    assert_eq!((&local["rows"], &local["totals"]), (&rest["rows"], &rest["totals"]));

    let out = h.run(Some(&d), &["usage", "--by", "model"]);
    let table = String::from_utf8_lossy(&out.stdout);
    let head = table.lines().next().unwrap();
    assert!(head.contains("cost_usd") && head.contains("unpriced"), "{table}");
    assert!(table.contains("mystery-model-x") && table.lines().last().unwrap().starts_with("total"), "{table}");
    let out = h.run(Some(&d), &["usage", "claude:s1"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("claude-sonnet-4-5"));
}

#[test]
fn price_sync_opens_a_time_segment_and_reprices_only_new_steps() {
    let h = Home::new();
    let t0 = now_ms() - 3_600_000;
    // The trailing prompt keeps the session working, so the daemon polls it while hot.
    let s = user("u1", t0) + &step("m1", t0 + 1000, "claude-test-m", 1000, 1000) + &user("u2", now_ms());
    std::fs::write(h.session(), s).unwrap();
    h.mock.set(200, catalog(15.0));
    let d = h.daemon(&["--price-sync-delay", "3600"]);
    assert_eq!(step_costs(&d), [None], "unknown before the first sync");

    let out = h.run(Some(&d), &["pricing", "sync"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let ino = inode(&h.pricing_dir().join("catalog.json"));
    let old = (1000.0 * 3.0 + 1000.0 * 15.0) / 1e6;
    wait("first catalog in the daemon", Duration::from_secs(5), || step_costs(&d) == [Some(old)]);

    h.mock.set(200, catalog(16.5));
    let before = now_ms();
    let r = h.json(Some(&d), &["pricing", "sync", "--json"]);
    let after = now_ms();
    assert_eq!(r["changed"], json!(["claude-test-m"]));
    let cat_path = h.pricing_dir().join("catalog.json");
    assert_ne!(inode(&cat_path), ino, "catalog.json replaced by rename");
    let leftovers: Vec<_> = std::fs::read_dir(h.pricing_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
    let cat = read_catalog(&h);
    let segs = entry(&cat, "claude-test-m")["prices"].as_array().unwrap().clone();
    assert_eq!(segs.len(), 2);
    let from = segs[1]["from"].as_i64().unwrap();
    assert!((before..=after).contains(&from), "segment starts at the observation time");
    assert_eq!(segs[0]["until"].as_i64(), Some(from));

    let t1 = now_ms();
    append(&h.session(), &step("m2", t1, "claude-test-m", 1000, 1000));
    let new = (1000.0 * 3.0 + 1000.0 * 16.5) / 1e6;
    wait("T1 step priced", Duration::from_secs(5), || step_costs(&d) == [Some(old), Some(new)]);

    let p = d.get("/v1/pricing").1;
    assert_eq!((p["fetched_at"].as_i64(), p["stale"].as_bool(), p["error"].is_null()), (Some(from), Some(false), true));
    assert_eq!(p["source"], "models.dev");
}

#[test]
fn sync_failures_and_safeguards_keep_prices_and_overrides() {
    let h = Home::new();
    let t = now_ms() - 3_600_000;
    let s = user("u1", t)
        + &step("m1", t + 1000, "claude-test-m", 1000, 1000)
        + &step("m2", t + 2000, "claude-test-o", 1000, 0);
    std::fs::write(h.session(), s).unwrap();
    std::fs::create_dir_all(h.pricing_dir()).unwrap();
    let overrides =
        json!([{"id":"claude-test-o","prices":[{"from":0,"input":7,"output":7,"cache_read":0.7}]}]).to_string();
    std::fs::write(h.pricing_dir().join("overrides.json"), &overrides).unwrap();
    let d = h.daemon(&["--price-sync-delay", "3600"]);
    let sync = || h.run(Some(&d), &["pricing", "sync", "--json"]);
    let m_output =
        || entry(&read_catalog(&h), "claude-test-m")["prices"].as_array().unwrap().last().unwrap()["output"].as_f64();
    let o_cost = || {
        let models = d.get("/v1/models").1;
        models.as_array().unwrap().iter().find(|m| m["model"] == "claude-test-o").unwrap()["cost_usd"].as_f64()
    };

    h.mock.set(200, catalog(15.0));
    assert!(sync().status.success());
    let good = std::fs::read(h.pricing_dir().join("catalog.json")).unwrap();

    h.mock.set(500, json!({"error":"boom"}));
    assert!(!sync().status.success(), "failed sync exits non-zero");
    assert_eq!(std::fs::read(h.pricing_dir().join("catalog.json")).unwrap(), good, "previous catalog kept");
    wait("error surfaced", Duration::from_secs(5), || d.get("/v1/pricing").1["error"].is_string());

    // 80 % jump seen once, then gone: never applied.
    h.mock.set(200, catalog(27.0));
    let r: Value = serde_json::from_slice(&sync().stdout).unwrap();
    assert_eq!(r["pending"], json!(["claude-test-m"]));
    assert_eq!(m_output(), Some(15.0));
    h.mock.set(200, catalog(15.0));
    assert!(sync().status.success());
    assert_eq!(m_output(), Some(15.0));

    // The same 80 % jump twice in a row: applied on the second read.
    h.mock.set(200, catalog(27.0));
    assert!(sync().status.success());
    assert_eq!(m_output(), Some(15.0));
    let r: Value = serde_json::from_slice(&sync().stdout).unwrap();
    assert_eq!(r["changed"], json!(["claude-test-m"]));
    assert_eq!(m_output(), Some(27.0));

    assert_eq!(std::fs::read_to_string(h.pricing_dir().join("overrides.json")).unwrap(), overrides);
    wait("override priced", Duration::from_secs(5), || o_cost() == Some(1000.0 * 7.0 / 1e6));
}

#[test]
fn no_price_sync_sends_no_request() {
    let h = Home::new();
    std::fs::write(h.session(), user("u1", now_ms())).unwrap();
    h.mock.set(200, catalog(15.0));
    {
        let d = h.daemon(&["--no-price-sync", "--price-sync-delay", "0"]);
        assert!(h.json(Some(&d), &["pricing", "--json"])["sync_enabled"] == false);
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(h.mock.hits(), 0, "--no-price-sync must not fetch");
    }
    // Control: the same daemon without the flag fetches right away.
    let _d = h.daemon(&["--price-sync-delay", "0"]);
    wait("first automatic sync", Duration::from_secs(10), || h.mock.hits() > 0);
    wait("catalog written", Duration::from_secs(10), || h.pricing_dir().join("catalog.json").exists());
}
