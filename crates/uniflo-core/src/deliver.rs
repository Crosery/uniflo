//! Zero-hook active delivery and wake engine for cross-agent messaging (ADR-0012).
//!
//! Delivers messages without requiring harness hooks:
//! 1. Live `herdr-core` terminal injection (matched by process ancestry or unique cwd+harness,
//!    with terminal screen safety check so permission prompts are never clobbered).
//! 2. Live `tmux` pane injection (matched by process ancestry, with pane capture safety check).
//! 3. Official headless context resume (`agy -r=<id> -p=...`, `claude -p --resume <id>`,
//!    `codex exec resume <id>`, `opencode run -s <id>`, `omp -p -r <id>`, `pi -p --session <id>`,
//!    `gemini -r <id> -p ...`, etc.) — wakes idle or offline sessions in their original context
//!    and records the reply back onto the message bus.
//!
//! Enforces a server-side auto-wake hop limit ([`MAX_AUTO_WAKE_HOPS`]) to prevent AI-to-AI loops.

use crate::engine::Engine;
use crate::message::{MessageBus, dir_matches};
use crate::resume;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uniflo_schema::{AgentMessage, Body, Envelope, PeerInfo, PeersResponse, Session, Status};

/// Maximum automatic wake hops before falling back to inbox-only (prevents AI ping-pong loops).
pub const MAX_AUTO_WAKE_HOPS: u8 = 2;
/// Maximum headless resume execution duration.
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(75);
/// Cache TTL for external `herdr-core ls` / `tmux list-panes` probes.
const PROBE_TTL: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Inbox,
    Auto,
    Wake,
}

impl DeliveryMode {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or("") {
            "auto" => Self::Auto,
            "wake" => Self::Wake,
            _ => Self::Inbox,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeliveryOutcome {
    pub channel: String,
    pub detail: String,
    pub reply: Option<AgentMessage>,
}

/// Verified headless non-interactive context-resume command (`argv`) for a harness session.
///
/// Always validates `id` via [`resume::valid_id`] so untrusted ids can never become CLI flags.
pub fn headless_resume_argv(harness: &str, id: &str, prompt: &str) -> Option<Vec<String>> {
    if !resume::valid_id(id) {
        return None;
    }
    let h = harness.to_ascii_lowercase();
    let argv: Vec<String> = match h.as_str() {
        "antigravity" | "agy" => vec!["agy".into(), format!("-r={id}"), format!("-p={prompt}")],
        "claude" => vec!["claude".into(), "-p".into(), "--resume".into(), id.into(), "--".into(), prompt.into()],
        "codex" => vec![
            "codex".into(),
            "exec".into(),
            "resume".into(),
            "--skip-git-repo-check".into(),
            id.into(),
            prompt.into(),
        ],
        "opencode" => vec!["opencode".into(), "run".into(), "--pure".into(), "-s".into(), id.into(), prompt.into()],
        "omp" => {
            vec!["omp".into(), "-p".into(), "-r".into(), id.into(), "--max-time=60".into(), prompt.into()]
        }
        "pi" => vec!["pi".into(), "-p".into(), "--session".into(), id.into(), "--".into(), prompt.into()],
        "gemini" => vec!["gemini".into(), "--skip-trust".into(), "-r".into(), id.into(), "-p".into(), prompt.into()],
        "cursor" => {
            vec!["cursor-agent".into(), "-p".into(), "--trust".into(), "--resume".into(), id.into(), prompt.into()]
        }
        "codebuddy" => {
            vec!["codebuddy".into(), "-p".into(), "--resume".into(), id.into(), "--".into(), prompt.into()]
        }
        "qoder" => vec!["qodercli".into(), "-p".into(), "--resume".into(), id.into(), prompt.into()],
        "workbuddy" => {
            vec!["workbuddy".into(), "-p".into(), "--resume".into(), id.into(), "--".into(), prompt.into()]
        }
        "factory" => vec!["droid".into(), "exec".into(), "-s".into(), id.into(), prompt.into()],
        _ => return None,
    };
    Some(argv)
}

/// Build headless resume `argv` for a concrete `Session`, preferring `--session-file` for Gemini
/// whose Uniflo `id` is the chat filename stem rather than the internal UUID.
pub fn headless_resume_argv_for_session(target: &Session, prompt: &str) -> Option<Vec<String>> {
    if target.harness.eq_ignore_ascii_case("gemini")
        && !target.source.is_empty()
        && std::path::Path::new(&target.source).is_file()
    {
        return Some(vec![
            "gemini".into(),
            "--skip-trust".into(),
            "--session-file".into(),
            target.source.clone(),
            "-p".into(),
            prompt.into(),
        ]);
    }
    headless_resume_argv(&target.harness, &target.id, prompt)
}

/// Whether `harness` has a supported headless resume recipe (for peer reachability hints).
pub fn supports_headless_resume(harness: &str) -> bool {
    headless_resume_argv(harness, "a1", "ping").is_some()
}

/// Format the prompt injected into a target session.
///
/// Human chat messages from `/demo` (`from == "web-demo"`) are forwarded cleanly as plain text
/// so chatting feels native; cross-agent messages carry compact routing metadata and hop count.
pub fn format_wake_prompt(msg: &AgentMessage, next_hop: u8) -> String {
    if msg.from == "web-demo" {
        return msg.text.clone();
    }
    let body = msg.text.replace("\r\n", " ").replace('\n', " ");
    format!(
        "[Uniflo 跨会话消息 hop={next_hop} id={} from={}] {} （请直接简洁回复正文，Uniflo 会自动转回给对方）",
        msg.id, msg.from, body
    )
}

/// Guard against injecting keystrokes into a terminal waiting on a permission confirmation prompt.
pub fn screen_looks_like_permission_prompt(screen: &str) -> bool {
    let tail = if screen.len() > 1400 {
        let mut idx = screen.len() - 1400;
        while !screen.is_char_boundary(idx) {
            idx += 1;
        }
        &screen[idx..]
    } else {
        screen
    };
    let lower = tail.to_ascii_lowercase();
    [
        "allow once",
        "allow always",
        "do you want to proceed",
        "do you want to run",
        "do you want to allow",
        "do you want to make this edit",
        "[y/n]",
        "press enter to confirm",
        "approve?",
        "permission required",
    ]
    .iter()
    .any(|pat| lower.contains(pat))
}

#[derive(Debug, Clone, Deserialize)]
struct HerdrLs {
    #[serde(default)]
    terminals: Vec<HerdrTerminal>,
}

#[derive(Debug, Clone, Deserialize)]
struct HerdrTerminal {
    address: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HerdrDetail {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    foreground: Option<HerdrFg>,
}

#[derive(Debug, Clone, Deserialize)]
struct HerdrFg {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Clone)]
struct HerdrEntry {
    address: String,
    agent: String,
    cwd: String,
    status: String,
    shell_pid: Option<u32>,
    fg_pid: Option<u32>,
    fg_cwd: Option<String>,
}

#[derive(Debug, Clone)]
struct TmuxPane {
    id: String,
    pid: u32,
}

struct ProbeCache<T> {
    at: Instant,
    data: T,
}

fn herdr_cache() -> &'static Mutex<Option<ProbeCache<Vec<HerdrEntry>>>> {
    static CACHE: OnceLock<Mutex<Option<ProbeCache<Vec<HerdrEntry>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn tmux_cache() -> &'static Mutex<Option<ProbeCache<Vec<TmuxPane>>>> {
    static CACHE: OnceLock<Mutex<Option<ProbeCache<Vec<TmuxPane>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Build an augmented `PATH` so background daemons started under minimal launchd/systemd
/// environments still locate user-installed CLIs (`herdr-core`, `agy`, `claude`, `codex`, `tmux`, `node`, `bun`).
fn enriched_path() -> std::ffi::OsString {
    let mut dirs: Vec<std::path::PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let mut push_unique = |p: std::path::PathBuf| {
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    };
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(std::path::PathBuf::from) {
        for rel in [
            ".local/bin",
            ".bun/bin",
            ".cargo/bin",
            ".opencode/bin",
            ".npm-global/bin",
            ".volta/bin",
            ".asdf/shims",
            ".codebuddy/bin",
            "bin",
        ] {
            push_unique(home.join(rel));
        }
        // Include newest installed nvm node bin if present (for `#!/usr/bin/env node` CLIs under launchd).
        if let Ok(rd) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut versions: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            if let Some(latest) = versions.pop() {
                push_unique(latest);
            }
        }
    }
    for sys in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/bin", "/bin"] {
        push_unique(std::path::PathBuf::from(sys));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

fn resolve_binary(bin: &str, search_path: &std::ffi::OsStr) -> std::path::PathBuf {
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 {
        return p.to_path_buf();
    }
    for dir in std::env::split_paths(search_path) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return cand;
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let c = dir.join(format!("{bin}.{ext}"));
                if c.is_file() {
                    return c;
                }
            }
        }
    }
    p.to_path_buf()
}

struct CmdResult {
    ok: bool,
    code: Option<i32>,
    timed_out: bool,
    stdout: String,
}

fn run_cmd_timeout(
    bin: &str,
    args: &[&str],
    cwd: Option<&str>,
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Option<CmdResult> {
    let path_env = enriched_path();
    let exe = resolve_binary(bin, &path_env);
    let mut cmd = Command::new(exe);
    cmd.args(args).env("PATH", &path_env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(d) = cwd
        && std::path::Path::new(d).is_dir()
    {
        cmd.current_dir(d);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    let mut out_pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = out_pipe {
            let _ = p.take(512 * 1024).read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break None,
        }
    };
    let stdout = reader.join().unwrap_or_default();
    let ok = status.is_some_and(|s| s.success());
    let code = status.and_then(|s| s.code());
    Some(CmdResult { ok, code, timed_out, stdout })
}

fn probe_herdr(force: bool) -> Vec<HerdrEntry> {
    if std::env::var_os("UNIFLO_NO_TERM_PROBE").is_some() {
        return Vec::new();
    }
    if !force
        && let Ok(g) = herdr_cache().lock()
        && let Some(c) = g.as_ref()
        && c.at.elapsed() < PROBE_TTL
    {
        return c.data.clone();
    }
    let mut entries = Vec::new();
    if let Some(res) = run_cmd_timeout("herdr-core", &["ls", "--json"], None, &[], Duration::from_secs(2))
        && res.ok
        && let Ok(ls) = serde_json::from_str::<HerdrLs>(&res.stdout)
    {
        let live: Vec<HerdrTerminal> = ls.terminals.into_iter().filter(|t| t.state == "live").collect();
        // Inspect live agent terminals in parallel chunks (at most 8 concurrent `herdr-core get` calls)
        // so even 40+ terminals finish in ~100 ms instead of seconds.
        let mut details: HashMap<String, HerdrDetail> = HashMap::new();
        let agent_addrs: Vec<String> =
            live.iter().filter(|t| is_agent_herdr(&t.agent)).map(|t| t.address.clone()).collect();
        for chunk in agent_addrs.chunks(8) {
            let chunk_res: Vec<(String, Option<HerdrDetail>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|addr| {
                        scope.spawn(move || {
                            let det = run_cmd_timeout(
                                "herdr-core",
                                &["get", addr, "--json"],
                                None,
                                &[],
                                Duration::from_millis(800),
                            )
                            .and_then(|r| r.ok.then_some(r.stdout))
                            .and_then(|d_out| serde_json::from_str::<HerdrDetail>(&d_out).ok());
                            (addr.clone(), det)
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            for (addr, det) in chunk_res {
                if let Some(d) = det {
                    details.insert(addr, d);
                }
            }
        }
        for t in live {
            let det = details.remove(&t.address);
            let (shell_pid, fg_pid, fg_cwd) = match det {
                Some(d) => (d.pid, d.foreground.as_ref().and_then(|f| f.pid), d.foreground.and_then(|f| f.cwd)),
                None => (None, None, None),
            };
            entries.push(HerdrEntry {
                address: t.address,
                agent: t.agent,
                cwd: t.cwd,
                status: t.status,
                shell_pid,
                fg_pid,
                fg_cwd,
            });
        }
    }
    if let Ok(mut g) = herdr_cache().lock() {
        *g = Some(ProbeCache { at: Instant::now(), data: entries.clone() });
    }
    entries
}

fn probe_tmux(force: bool) -> Vec<TmuxPane> {
    if std::env::var_os("UNIFLO_NO_TERM_PROBE").is_some() {
        return Vec::new();
    }
    if !force
        && let Ok(g) = tmux_cache().lock()
        && let Some(c) = g.as_ref()
        && c.at.elapsed() < PROBE_TTL
    {
        return c.data.clone();
    }
    let mut panes = Vec::new();
    if let Some(res) = run_cmd_timeout(
        "tmux",
        &["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"],
        None,
        &[],
        Duration::from_secs(2),
    ) && res.ok
    {
        for line in res.stdout.lines() {
            let mut parts = line.split_whitespace();
            let (Some(id), Some(pid_s)) = (parts.next(), parts.next()) else { continue };
            if let Ok(pid) = pid_s.parse::<u32>() {
                panes.push(TmuxPane { id: id.to_owned(), pid });
            }
        }
    }
    if let Ok(mut g) = tmux_cache().lock() {
        *g = Some(ProbeCache { at: Instant::now(), data: panes.clone() });
    }
    panes
}

/// Snapshot of `pid -> ppid` on Unix (empty on Windows).
pub fn parent_pid_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    #[cfg(unix)]
    {
        if let Some(res) = run_cmd_timeout("ps", &["-axo", "pid=,ppid="], None, &[], Duration::from_secs(2))
            && res.ok
        {
            for line in res.stdout.lines() {
                let mut it = line.split_whitespace();
                if let (Some(p), Some(pp)) = (it.next(), it.next())
                    && let (Ok(pid), Ok(ppid)) = (p.parse::<u32>(), pp.parse::<u32>())
                {
                    map.insert(pid, ppid);
                }
            }
        }
    }
    map
}

/// Walk `pid` and its ancestors (up to `max_depth` hops).
pub fn pid_ancestors(pid: u32, ppids: &HashMap<u32, u32>, max_depth: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(max_depth + 1);
    let mut seen = HashSet::new();
    let mut cur = pid;
    for _ in 0..=max_depth {
        if cur == 0 || !seen.insert(cur) {
            break;
        }
        out.push(cur);
        match ppids.get(&cur).copied() {
            Some(next) if next > 1 => cur = next,
            _ => break,
        }
    }
    out
}

/// Resolve which live Uniflo session owns `caller_pid` by walking its ancestor chain against
/// every live `session.pid`.
pub fn resolve_session_for_pid(engine: &Engine, caller_pid: u32, ppids: &HashMap<u32, u32>) -> Option<String> {
    let chain = pid_ancestors(caller_pid, ppids, 8);
    if chain.is_empty() {
        return None;
    }
    let snap = engine.sessions_snapshot();
    // Prefer closest ancestor match; tie-break by Work status then newest updated_at (snap.sorted order).
    for anc in chain {
        if let Some(sess) = snap
            .sorted
            .iter()
            .find(|s| s.pid == Some(anc) && s.status == Status::Work)
            .or_else(|| snap.sorted.iter().find(|s| s.pid == Some(anc)))
        {
            return Some(sess.key.clone());
        }
    }
    None
}

fn is_agent_herdr(agent: &str) -> bool {
    let a = agent.trim().to_ascii_lowercase();
    !a.is_empty() && !matches!(a.as_str(), "sh" | "zsh" | "bash" | "fish")
}

fn harness_matches_herdr(harness: &str, herdr_agent: &str) -> bool {
    let h = harness.to_ascii_lowercase();
    let a = herdr_agent.to_ascii_lowercase();
    if !is_agent_herdr(&a) {
        return false;
    }
    if h == a {
        return true;
    }
    match h.as_str() {
        "claude" => a.contains("claude"),
        "codex" => a.contains("codex"),
        "opencode" => a.contains("opencode"),
        "omp" => a.contains("omp"),
        "pi" => a == "pi" || a.contains("pi-"),
        "antigravity" | "agy" => a.contains("antigravity") || a.contains("agy"),
        "gemini" => a.contains("gemini"),
        "cursor" => a.contains("cursor"),
        "codebuddy" => a.contains("codebuddy"),
        "qoder" => a.contains("qoder"),
        _ => false,
    }
}

fn match_herdr<'a>(sess: &Session, entries: &'a [HerdrEntry], ppids: &HashMap<u32, u32>) -> Option<&'a HerdrEntry> {
    // 1. Exact process ancestry match via session.pid (100% accurate even when 10 sessions share `~`!).
    if let Some(spid) = sess.pid {
        let ancs = pid_ancestors(spid, ppids, 8);
        if let Some(hit) = entries.iter().find(|e| e.fg_pid.is_some_and(|p| p == spid || ancs.contains(&p))) {
            return Some(hit);
        }
        if let Some(hit) = entries.iter().find(|e| e.shell_pid.is_some_and(|p| ancs.contains(&p))) {
            return Some(hit);
        }
    }
    // 2. Fallback: unique live Herdr terminal sharing exact cwd + harness (only when unambiguous).
    let cwd = sess.cwd.as_deref()?;
    let cands: Vec<&HerdrEntry> = entries
        .iter()
        .filter(|e| {
            harness_matches_herdr(&sess.harness, &e.agent)
                && (e.fg_cwd.as_deref().is_some_and(|c| dir_matches(c, cwd) && dir_matches(cwd, c))
                    || (!e.cwd.is_empty() && dir_matches(&e.cwd, cwd) && dir_matches(cwd, &e.cwd)))
        })
        .collect();
    if cands.len() == 1 { Some(cands[0]) } else { None }
}

fn match_tmux<'a>(sess: &Session, panes: &'a [TmuxPane], ppids: &HashMap<u32, u32>) -> Option<&'a TmuxPane> {
    let spid = sess.pid?;
    let ancs = pid_ancestors(spid, ppids, 8);
    panes.iter().find(|p| ancs.contains(&p.pid))
}

fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() {
            if b[i + 1] == b'[' {
                i += 2;
                while i < b.len() && !b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                continue;
            } else if b[i + 1] == b']' {
                i += 2;
                while i < b.len() && b[i] != 0x07 {
                    if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < b.len() && b[i] == 0x07 {
                    i += 1;
                }
                continue;
            }
        }
        let ch = raw[i..].chars().next().unwrap_or(' ');
        if ch != '\r' {
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    out
}

/// Build the live reachable peers roster for `GET /v1/messages/peers`.
pub fn list_peers(
    engine: &Engine,
    bus: &MessageBus,
    caller_pid: Option<u32>,
    explicit_self: Option<&str>,
    limit: usize,
) -> PeersResponse {
    let limit = if limit == 0 { 25 } else { limit.min(100) };
    let ppids = parent_pid_map();
    let self_key = explicit_self
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| caller_pid.and_then(|pid| resolve_session_for_pid(engine, pid, &ppids)));
    let herdr = probe_herdr(false);
    let tmux = probe_tmux(false);
    let unread = bus.unread_counts();

    let snap = engine.sessions_snapshot();
    // Order: live working -> live idle -> recent idle; skip archived & sub-agents by default unless unread.
    let mut rows: Vec<&Arc<Session>> = snap
        .sorted
        .iter()
        .filter(|s| {
            if self_key.as_deref() == Some(s.key.as_str()) {
                return false;
            }
            if s.archived {
                return false;
            }
            s.parent.is_none() || unread.contains_key(&s.key)
        })
        .collect();
    rows.sort_by(|a, b| {
        let rank = |s: &Session| match (s.status, s.pid.is_some()) {
            (Status::Work, _) => 0u8,
            (Status::Idle, true) => 1u8,
            (Status::Idle, false) => 2u8,
        };
        rank(a).cmp(&rank(b)).then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    rows.truncate(limit);

    let peers = rows
        .into_iter()
        .map(|s| {
            let (channel, terminal) = if let Some(h) = match_herdr(s, &herdr, &ppids) {
                ("herdr".to_owned(), Some(h.address.clone()))
            } else if let Some(t) = match_tmux(s, &tmux, &ppids) {
                ("tmux".to_owned(), Some(t.id.clone()))
            } else if supports_headless_resume(&s.harness) {
                ("resume_wake".to_owned(), None)
            } else {
                ("inbox".to_owned(), None)
            };
            PeerInfo {
                key: s.key.clone(),
                harness: s.harness.clone(),
                title: s.title.clone(),
                cwd: s.cwd.clone(),
                status: s.status,
                pid: s.pid,
                updated_at: s.updated_at,
                channel,
                terminal,
                unread: unread.get(&s.key).copied().unwrap_or(0),
            }
        })
        .collect();
    PeersResponse { self_key, peers }
}

/// Actively deliver `msg` to `target` without requiring hooks, and optionally await a reply.
pub async fn deliver_message(
    engine: Arc<Engine>,
    bus: Arc<MessageBus>,
    target: Arc<Session>,
    msg: AgentMessage,
    mode: DeliveryMode,
    wait_secs: Option<u64>,
    hop: u8,
) -> DeliveryOutcome {
    if mode == DeliveryMode::Inbox {
        return DeliveryOutcome {
            channel: "inbox".into(), detail: "已写入消息总线与 SSE 实时流".into(), reply: None
        };
    }

    if hop >= MAX_AUTO_WAKE_HOPS {
        return DeliveryOutcome {
            channel: "loop_guard".into(),
            detail: format!("已达自动唤醒跳数上限（{MAX_AUTO_WAKE_HOPS}），消息保留在收件箱防止循环"),
            reply: None,
        };
    }

    let next_hop = hop.saturating_add(1);
    let wake_prompt = format_wake_prompt(&msg, next_hop);
    let wait_dur = wait_secs.map(|s| Duration::from_secs(s.clamp(1, 90)));

    // Subscribe to both MessageBus and Engine events BEFORE dispatching so we never miss a fast reply!
    let bus_rx = wait_dur.map(|_| bus.subscribe());
    let eng_rx = wait_dur.map(|_| engine.subscribe(None).1);

    // Step 1: Probe live Herdr / tmux terminals off the async runtime.
    let target_for_probe = target.clone();
    let prompt_for_term = wake_prompt.clone();
    let term_res = tokio::task::spawn_blocking(move || try_terminal_inject(&target_for_probe, &prompt_for_term, mode))
        .await
        .ok()
        .flatten();

    if let Some((channel, detail, injected)) = term_res {
        if !injected {
            return DeliveryOutcome { channel, detail, reply: None };
        }
        // Mark delivered once injected into live terminal.
        let _ = bus.ack(
            &engine,
            uniflo_schema::AckMessagesRequest { ids: vec![msg.id.clone()], to: None, state: Some("delivered".into()) },
        );
        let reply = match (wait_dur, bus_rx, eng_rx) {
            (Some(dur), Some(brx), Some(erx)) => wait_for_live_reply(&bus, &target.key, &msg, brx, erx, dur).await,
            _ => None,
        };
        return DeliveryOutcome { channel, detail, reply };
    }

    // Step 2: If target is currently working (non-Herdr/non-tmux) and mode is Auto, leave in inbox/watchdog
    // rather than spawning a competing headless process mid-turn.
    if mode == DeliveryMode::Auto && target.status == Status::Work {
        let reply = match (wait_dur, bus_rx, eng_rx) {
            (Some(dur), Some(brx), Some(erx)) => wait_for_live_reply(&bus, &target.key, &msg, brx, erx, dur).await,
            _ => None,
        };
        return DeliveryOutcome {
            channel: "inbox_busy".into(),
            detail: "对方会话正在工作中，消息已推送到实时看门狗与收件箱".into(),
            reply,
        };
    }

    // Step 3: Official headless context resume (zero hooks, works for idle/offline/plain-terminal sessions!).
    if let Some(argv) = headless_resume_argv_for_session(&target, &wake_prompt) {
        let display = format!("{} {}", argv[0], argv[1..argv.len().saturating_sub(1)].join(" "));
        let cwd = target.cwd.clone();
        let bus_bg = bus.clone();
        let engine_bg = engine.clone();
        let target_key = target.key.clone();
        let orig_from = msg.from.clone();
        let orig_id = msg.id.clone();

        if let Some(dur) = wait_dur {
            let outcome = tokio::task::spawn_blocking(move || {
                run_headless_and_record(
                    &argv,
                    cwd.as_deref(),
                    next_hop,
                    &orig_from,
                    &orig_id,
                    &target_key,
                    &bus_bg,
                    &engine_bg,
                    dur,
                )
            })
            .await
            .ok();
            return match outcome {
                Some(HeadlessOutcome::Replied(reply)) => DeliveryOutcome {
                    channel: "resume_wake".into(),
                    detail: format!("已通过 `{display}` 原上下文唤醒并收到回复"),
                    reply: Some(reply),
                },
                Some(HeadlessOutcome::OkNoText) => DeliveryOutcome {
                    channel: "resume_wake".into(),
                    detail: format!("已通过 `{display}` 原上下文唤醒完成（无标准输出文本）"),
                    reply: None,
                },
                Some(HeadlessOutcome::TimedOut) => DeliveryOutcome {
                    channel: "resume_wake".into(),
                    detail: format!("已触发 `{display}` 原上下文唤醒（等待超时，回复完成后将写入会话）"),
                    reply: None,
                },
                Some(HeadlessOutcome::Failed(code)) => DeliveryOutcome {
                    channel: "resume_failed".into(),
                    detail: match code {
                        Some(c) => format!("`{display}` 原上下文唤醒失败（退出码 {c}），消息已保留在收件箱"),
                        None => format!("`{display}` 原上下文唤醒异常中止，消息已保留在收件箱"),
                    },
                    reply: None,
                },
                Some(HeadlessOutcome::NotFound(bin)) => DeliveryOutcome {
                    channel: "inbox".into(),
                    detail: format!("未找到 `{bin}` 命令，消息已保留在收件箱与实时 SSE 流"),
                    reply: None,
                },
                None => DeliveryOutcome {
                    channel: "resume_failed".into(),
                    detail: format!("`{display}` 原上下文唤醒执行失败，消息已保留在收件箱"),
                    reply: None,
                },
            };
        }

        // Fire-and-forget async background wake: returns immediately (<1 ms) while reply streams into transcript & bus!
        tokio::task::spawn_blocking(move || {
            let _ = run_headless_and_record(
                &argv,
                cwd.as_deref(),
                next_hop,
                &orig_from,
                &orig_id,
                &target_key,
                &bus_bg,
                &engine_bg,
                HEADLESS_TIMEOUT,
            );
        });
        return DeliveryOutcome {
            channel: "resume_wake".into(),
            detail: format!("已在后台通过 `{display}` 原上下文唤醒对方，回复将实时写入对话流"),
            reply: None,
        };
    }

    DeliveryOutcome {
        channel: "inbox".into(),
        detail: "该 harness 不支持无头续跑，消息已写入收件箱与实时 SSE 流".into(),
        reply: None,
    }
}

/// Returns `Some((channel, detail, was_injected))` if a live Herdr or tmux terminal owns `target`.
fn try_terminal_inject(target: &Session, prompt: &str, mode: DeliveryMode) -> Option<(String, String, bool)> {
    let ppids = parent_pid_map();

    // Layer 1: Herdr live terminal
    let herdr = probe_herdr(false);
    if let Some(h) = match_herdr(target, &herdr, &ppids) {
        if h.status == "working" && mode == DeliveryMode::Auto {
            return Some((
                "inbox_busy".into(),
                format!("Herdr 终端 {} 正在输出中，已安全写入收件箱与看门狗", h.address),
                false,
            ));
        }
        if let Some(res) = run_cmd_timeout("herdr-core", &["read", &h.address], None, &[], Duration::from_secs(2))
            && res.ok
            && screen_looks_like_permission_prompt(&res.stdout)
        {
            return Some((
                "inbox_busy".into(),
                format!("Herdr 终端 {} 正在等待权限确认，已改为安全写入收件箱", h.address),
                false,
            ));
        }
        let safe_prompt = prompt.replace("\r\n", " ").replace('\n', " ");
        if let Some(res) =
            run_cmd_timeout("herdr-core", &["send", "--", &h.address, &safe_prompt], None, &[], Duration::from_secs(4))
            && res.ok
        {
            return Some(("herdr".into(), format!("已通过 Herdr 终端 {} 实时直推（零 Hook）", h.address), true));
        }
    }

    // Layer 2: tmux live pane
    let tmux = probe_tmux(false);
    if let Some(pane) = match_tmux(target, &tmux, &ppids) {
        if target.status == Status::Work && mode == DeliveryMode::Auto {
            return Some((
                "inbox_busy".into(),
                format!("tmux 窗格 {} 正在输出中，已安全写入收件箱与看门狗", pane.id),
                false,
            ));
        }
        if let Some(res) = run_cmd_timeout(
            "tmux",
            &["capture-pane", "-p", "-t", &pane.id, "-S", "-20"],
            None,
            &[],
            Duration::from_secs(2),
        ) && res.ok
            && screen_looks_like_permission_prompt(&res.stdout)
        {
            return Some((
                "inbox_busy".into(),
                format!("tmux 窗格 {} 正在等待权限确认，已改为安全写入收件箱", pane.id),
                false,
            ));
        }
        let safe_prompt = prompt.replace("\r\n", " ").replace('\n', " ");
        let sent_text = run_cmd_timeout(
            "tmux",
            &["send-keys", "-t", &pane.id, "-l", "--", &safe_prompt],
            None,
            &[],
            Duration::from_secs(3),
        );
        let sent_enter =
            run_cmd_timeout("tmux", &["send-keys", "-t", &pane.id, "Enter"], None, &[], Duration::from_secs(2));
        if sent_text.is_some_and(|r| r.ok) && sent_enter.is_some_and(|r| r.ok) {
            return Some(("tmux".into(), format!("已通过 tmux 窗格 {} 实时直推（零 Hook）", pane.id), true));
        }
    }

    None
}

enum HeadlessOutcome {
    Replied(AgentMessage),
    OkNoText,
    TimedOut,
    Failed(Option<i32>),
    NotFound(String),
}

#[allow(clippy::too_many_arguments)]
fn run_headless_and_record(
    argv: &[String],
    cwd: Option<&str>,
    next_hop: u8,
    orig_from: &str,
    orig_id: &str,
    target_key: &str,
    bus: &MessageBus,
    engine: &Engine,
    timeout: Duration,
) -> HeadlessOutcome {
    let Some(bin) = argv.first() else { return HeadlessOutcome::Failed(None) };
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let hop_str = next_hop.to_string();
    let envs = [
        ("NO_COLOR", "1"),
        ("UNIFLO_WAKE_HOP", hop_str.as_str()),
        ("UNIFLO_WAKE_FROM", orig_from),
        ("UNIFLO_WAKE_MSG_ID", orig_id),
    ];
    let Some(res) = run_cmd_timeout(bin, &args, cwd, &envs, timeout) else {
        return HeadlessOutcome::NotFound(bin.clone());
    };
    if res.timed_out {
        return HeadlessOutcome::TimedOut;
    }
    if !res.ok {
        return HeadlessOutcome::Failed(res.code);
    }
    let cleaned = strip_ansi(&res.stdout).trim().to_owned();
    if cleaned.is_empty() {
        return HeadlessOutcome::OkNoText;
    }
    let _ = bus.ack(
        engine,
        uniflo_schema::AckMessagesRequest { ids: vec![orig_id.to_owned()], to: None, state: Some("acked".into()) },
    );
    match bus.record_reply(target_key, orig_from, orig_id, cleaned) {
        Ok(reply) => HeadlessOutcome::Replied(reply),
        Err(_) => HeadlessOutcome::OkNoText,
    }
}

/// Wait for either an explicit MessageBus reply from `target_key` OR the target session finishing
/// a fresh assistant turn in Uniflo's live Event stream (zero-hook reply capture!).
async fn wait_for_live_reply(
    bus: &MessageBus,
    target_key: &str,
    orig: &AgentMessage,
    mut bus_rx: tokio::sync::broadcast::Receiver<Arc<AgentMessage>>,
    mut eng_rx: tokio::sync::broadcast::Receiver<Arc<Envelope>>,
    timeout: Duration,
) -> Option<AgentMessage> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_assistant_text = String::new();
    let mut saw_new_activity = false;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            if !last_assistant_text.is_empty() {
                return bus.record_reply(target_key, &orig.from, &orig.id, last_assistant_text).ok();
            }
            return None;
        }
        let rem = deadline - now;
        tokio::select! {
            _ = tokio::time::sleep(rem) => {
                if !last_assistant_text.is_empty() {
                    return bus.record_reply(target_key, &orig.from, &orig.id, last_assistant_text).ok();
                }
                return None;
            }
            res = bus_rx.recv() => {
                match res {
                    Ok(m) if m.from == target_key && m.id != orig.id => {
                        return Some((*m).clone());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    _ => {}
                }
            }
            res = eng_rx.recv() => {
                if let Ok(env) = res
                    && let Envelope::Event { event, .. } = &*env
                    && event.session == target_key
                {
                    match &event.body {
                        Body::UserMessage { .. } | Body::TurnStart {} => {
                            saw_new_activity = true;
                        }
                        Body::AssistantMessage { text, .. } if !event.partial => {
                            if !text.trim().is_empty() {
                                saw_new_activity = true;
                                last_assistant_text = text.clone();
                            }
                        }
                        Body::TurnEnd { .. } if saw_new_activity && !last_assistant_text.is_empty() => {
                            return bus.record_reply(target_key, &orig.from, &orig.id, last_assistant_text).ok();
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_argv_validates_id_and_builds_verified_shapes() {
        assert_eq!(
            headless_resume_argv("antigravity", "abc-123", "hello"),
            Some(vec!["agy".into(), "-r=abc-123".into(), "-p=hello".into()])
        );
        assert_eq!(
            headless_resume_argv("claude", "abc-123", "hello"),
            Some(vec!["claude".into(), "-p".into(), "--resume".into(), "abc-123".into(), "--".into(), "hello".into()])
        );
        assert_eq!(
            headless_resume_argv("codex", "abc-123", "hello"),
            Some(vec![
                "codex".into(),
                "exec".into(),
                "resume".into(),
                "--skip-git-repo-check".into(),
                "abc-123".into(),
                "hello".into()
            ])
        );
        assert_eq!(
            headless_resume_argv("opencode", "abc-123", "hello"),
            Some(vec!["opencode".into(), "run".into(), "--pure".into(), "-s".into(), "abc-123".into(), "hello".into()])
        );
        assert_eq!(
            headless_resume_argv("factory", "abc-123", "hello"),
            Some(vec!["droid".into(), "exec".into(), "-s".into(), "abc-123".into(), "hello".into()])
        );
        assert_eq!(
            headless_resume_argv("cursor", "abc-123", "hello"),
            Some(vec![
                "cursor-agent".into(),
                "-p".into(),
                "--trust".into(),
                "--resume".into(),
                "abc-123".into(),
                "hello".into()
            ])
        );
        assert_eq!(
            headless_resume_argv("qoder", "abc-123", "hello"),
            Some(vec!["qodercli".into(), "-p".into(), "--resume".into(), "abc-123".into(), "hello".into()])
        );
        // Option-like session IDs are rejected.
        assert!(headless_resume_argv("claude", "--dangerously-skip-permissions", "hi").is_none());
        assert!(headless_resume_argv("unknown-harness", "abc-123", "hi").is_none());
    }

    #[test]
    fn permission_prompt_detection_and_prompt_formatting() {
        assert!(screen_looks_like_permission_prompt("Do you want to proceed? [y/N]"));
        assert!(screen_looks_like_permission_prompt("Allow once / Allow always"));
        assert!(!screen_looks_like_permission_prompt("Crosery: 你好，有什么需要处理？\n> "));

        let web = AgentMessage {
            id: "m-1".into(),
            from: "web-demo".into(),
            to: "antigravity:abc".into(),
            text: "查一下测试状态".into(),
            kind: None,
            reply_to: None,
            created_at: 0,
            delivered_at: None,
            acked_at: None,
        };
        assert_eq!(format_wake_prompt(&web, 1), "查一下测试状态");

        let peer = AgentMessage { from: "claude:111".into(), ..web };
        let formatted = format_wake_prompt(&peer, 1);
        assert!(
            formatted.contains("hop=1")
                && formatted.contains("from=claude:111")
                && formatted.contains("查一下测试状态")
        );
    }

    #[test]
    fn pid_ancestry_walks_parent_chain_without_cycles() {
        let mut map = HashMap::new();
        map.insert(300, 200);
        map.insert(200, 100);
        map.insert(100, 1);
        assert_eq!(pid_ancestors(300, &map, 6), vec![300, 200, 100]);

        // Cycle safety
        let mut cyc = HashMap::new();
        cyc.insert(10, 20);
        cyc.insert(20, 10);
        assert_eq!(pid_ancestors(10, &cyc, 6), vec![10, 20]);
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        assert_eq!(strip_ansi("\x1b[32mOK\x1b[0m\r\n"), "OK\n");
    }

    #[test]
    fn resolve_binary_finds_executable_on_search_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("my-custom-agent-bin");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        let search = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_binary("my-custom-agent-bin", &search), fake);
        assert_eq!(resolve_binary("/abs/path/bin", &search), std::path::PathBuf::from("/abs/path/bin"));
        let enriched = enriched_path();
        let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&enriched).collect();
        assert!(
            dirs.iter()
                .any(|p| p == std::path::Path::new("/usr/bin") || p == std::path::Path::new("/opt/homebrew/bin"))
        );
    }
}
