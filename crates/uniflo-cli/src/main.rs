//! `uniflo` — daemon and command line for the unified agent-harness session gateway.

mod agent;
mod client;
mod grep;
mod mcp;
mod render;
mod setup;
mod usage;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::Client;
use render::Style;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, EngineOptions, HistoryQuery, PriceSync};
use uniflo_gateway::GuardOptions;
use uniflo_schema::{Envelope, Event, Harness, Session, Status};
use uniflo_search::fts::{Fts, FtsOptions};
use uniflo_search::{Query, search};

const DEFAULT_URL: &str = "http://127.0.0.1:7311";

#[derive(Parser)]
#[command(name = "uniflo", version, about = "Unified live view of every local agent harness session")]
struct Cli {
    /// Daemon URL used by query commands.
    #[arg(long, global = true, env = "UNIFLO_URL", default_value = DEFAULT_URL)]
    url: String,
    /// Bearer token for the daemon (or for `daemon`: required token).
    #[arg(long, global = true, env = "UNIFLO_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Do not talk to a daemon; index in-process.
    #[arg(long, global = true)]
    local: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the background index + HTTP gateway.
    Daemon {
        #[arg(long, default_value = "127.0.0.1:7311")]
        bind: String,
        /// Extra browser origins allowed (loopback origins always are). `*` = any.
        #[arg(long = "cors-origin")]
        cors_origins: Vec<String>,
        /// Extra Host header values accepted (e.g. a LAN name). Use with --token.
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
        /// Seconds of silence after which a `work` session without a live process is `idle`.
        #[arg(long, default_value_t = 600)]
        stale_after: u64,
        /// Ignore and do not write the on-disk index cache.
        #[arg(long)]
        no_cache: bool,
        /// Skip the periodic crates.io update check (checks are opt-out; one HTTPS GET per interval).
        #[arg(long)]
        no_update_check: bool,
        /// Never fetch the price catalog (embedded snapshot + local files only).
        #[arg(long)]
        no_price_sync: bool,
        /// Seconds after startup before the first price sync (tests).
        #[arg(long, default_value_t = 60, hide = true)]
        price_sync_delay: u64,
        /// Do not build or serve the full-text index (`/v1/search` answers 503, no index file is created).
        #[arg(long)]
        no_fts: bool,
    },
    /// Check crates.io for a newer stable release, or install it (`cargo install uniflo --force`).
    Update {
        /// Only report; do not install.
        #[arg(long)]
        check: bool,
        /// Install the newest prerelease instead of the stable release (explicit opt-in;
        /// prereleases do not guarantee stability).
        #[arg(long = "pre")]
        prerelease: bool,
        #[arg(long)]
        json: bool,
    },
    /// One-shot in-process index; prints coverage per harness.
    Scan {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_cache: bool,
    },
    /// List / search sessions (fd-style filters + fzf-style text, see `uniflo help ls`).
    #[command(alias = "find")]
    Ls {
        /// e.g. `s:work h:claude in:uniflo since:2h 'gateway`
        query: Vec<String>,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
        /// Tab-separated (key, harness, status, age, title, cwd) for piping into fzf.
        #[arg(long)]
        tsv: bool,
    },
    /// Sessions currently working.
    Ps {
        #[arg(long)]
        json: bool,
    },
    /// Session metadata plus its latest events.
    Show {
        key: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        events: usize,
    },
    /// Print a session transcript; `-f` keeps following live.
    Tail {
        key: String,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        #[arg(short, long)]
        follow: bool,
        /// Raw NDJSON events instead of rendered lines.
        #[arg(long)]
        json: bool,
    },
    /// Stream live envelopes as NDJSON (all sessions unless filtered).
    Watch {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        kinds: Option<String>,
        #[arg(long)]
        since: Option<u64>,
    },
    /// Supported harnesses and their session counts on this machine.
    Harnesses {
        #[arg(long)]
        json: bool,
    },
    /// Token usage and API-equivalent cost by harness / model / project / day …, or per step
    /// of one session (`uniflo usage <key>`).
    Usage(usage::UsageArgs),
    /// Price catalog status; `uniflo pricing sync` fetches it now.
    Pricing(usage::PricingArgs),
    /// Full-text search over message, reasoning and tool text (Chinese and code substrings).
    Grep {
        /// Terms are ANDed; `"two words"` is a phrase, `-term` excludes (quote the whole query,
        /// e.g. `'deploy -rollback'`, or put terms after `--`).
        #[arg(required = true)]
        terms: Vec<String>,
        /// Session filter in the `uniflo ls` syntax, e.g. `h:claude in:work since:7d`.
        #[arg(long)]
        filter: Option<String>,
        /// Sessions to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// stdio MCP server for agents (what `uniflo setup --mcp` registers).
    Mcp,
    /// The agent Skill shipped with this binary.
    Skill {
        #[command(subcommand)]
        action: agent::SkillAction,
    },
    /// Connect the agent harnesses on this machine: MCP server and/or Skill (asks first).
    Setup(setup::SetupArgs),
    /// Continue a session in its own harness from its cwd; `--print` only prints the command.
    Resume {
        key: String,
        #[arg(long)]
        print: bool,
    },
    /// This project's recent sessions as short Markdown for an agent (empty without a daemon).
    Context(agent::ContextArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if !matches!(
        cli.cmd,
        Cmd::Daemon { .. } | Cmd::Mcp | Cmd::Setup(_) | Cmd::Skill { .. } | Cmd::Context(_) | Cmd::Update { .. }
    ) {
        setup::first_run();
    }
    match cli.cmd {
        Cmd::Daemon {
            ref bind,
            ref cors_origins,
            ref allow_hosts,
            stale_after,
            no_cache,
            no_update_check,
            no_price_sync,
            price_sync_delay,
            no_fts,
        } => {
            let guard = GuardOptions {
                token: cli.token.clone(),
                cors_origins: cors_origins.clone(),
                allowed_hosts: allow_hosts.clone(),
                ..Default::default()
            };
            if !allow_hosts.is_empty() && guard.token.is_none() {
                bail!("--allow-host exposes transcripts beyond loopback; set --token as well");
            }
            let price_sync = (!no_price_sync)
                .then(|| PriceSync { delay: Duration::from_secs(price_sync_delay), ..Default::default() });
            daemon(bind, guard, stale_after, no_cache, !no_update_check, price_sync, !no_fts)
        }
        Cmd::Scan { json, no_cache } => scan(json, no_cache),
        Cmd::Update { check, prerelease, json } => update(check, prerelease, json),
        Cmd::Pricing(ref a) => usage::pricing(&cli, a),
        Cmd::Mcp => mcp::run(&cli),
        Cmd::Skill { ref action } => {
            agent::skill(action);
            Ok(())
        }
        Cmd::Setup(ref a) => setup::run(a),
        Cmd::Context(ref a) => agent::context(&cli, a),
        ref cmd => Source::open(&cli)?.run(cmd),
    }
}

fn engine_opts(no_cache: bool, stale_after: u64) -> EngineOptions {
    EngineOptions {
        stale_after: Duration::from_secs(stale_after),
        cache_path: if no_cache { None } else { EngineOptions::default().cache_path },
        ..Default::default()
    }
}

fn daemon(
    bind: &str,
    guard: GuardOptions,
    stale_after: u64,
    no_cache: bool,
    update_check: bool,
    price_sync: Option<PriceSync>,
    fts: bool,
) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "uniflo=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut opts = engine_opts(no_cache, stale_after);
    // Background crates.io check: hourly; the first one fires right after startup.
    opts.update_check = update_check.then(|| Duration::from_secs(3600));
    opts.price_sync = price_sync;
    let engine = Engine::new(uniflo_adapters::all(), opts);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("bind {bind}"))?;
        let report = {
            let e = engine.clone();
            tokio::task::spawn_blocking(move || e.index()).await?
        };
        eprintln!(
            "uniflo: indexed {} sessions from {} sources in {} ms ({} read, {} cached) · listening on http://{bind}",
            report.sessions, report.files, report.ms, report.read, report.restored
        );
        let _ = engine.save_cache();
        let fts = fts.then(|| start_fts(&engine)).flatten();
        let runner = tokio::spawn(engine.clone().run());
        let router = uniflo_gateway::router_with_fts(engine.clone(), guard, fts);
        uniflo_gateway::serve(listener, router, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
        runner.abort();
        engine.save_cache()?;
        engine.save_usage_cache()?;
        eprintln!("uniflo: index cache saved, bye");
        Ok(())
    })
}

/// The index builds in its own thread; failing to open it only disables `/v1/search`.
fn start_fts(engine: &Arc<Engine>) -> Option<Arc<Fts>> {
    match Fts::start(engine.clone(), FtsOptions::default()) {
        Ok(f) => {
            let st = f.status();
            eprintln!(
                "uniflo: full-text index {} · {} of {} sessions to index in the background",
                st.path,
                st.progress.total - st.progress.done,
                st.progress.total
            );
            Some(Arc::new(f))
        }
        Err(err) => {
            tracing::warn!("full-text index unavailable, /v1/search disabled: {err:#}");
            None
        }
    }
}

fn scan(json: bool, no_cache: bool) -> Result<()> {
    let t0 = Instant::now();
    let engine = Engine::new(uniflo_adapters::all(), engine_opts(no_cache, 600));
    let report = engine.index();
    if !no_cache {
        engine.save_cache()?;
    }
    let stats = engine.stats();
    if json {
        let out = serde_json::json!({ "report": report, "harnesses": engine.harnesses(), "stats": stats });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("{:<14} {:>8} {:>8}  roots", "harness", "sessions", "working");
    for h in engine.harnesses() {
        println!("{:<14} {:>8} {:>8}  {}", h.id, h.sessions, h.working, h.roots.join(", "));
    }
    println!(
        "\n{} sessions · {} sources ({} read, {} from cache) · {} MB parsed · {} bad lines · {} read errors · {} ms",
        report.sessions,
        report.files,
        report.read,
        report.restored,
        stats.bytes / 1_000_000,
        stats.bad_lines,
        stats.read_errors,
        t0.elapsed().as_millis()
    );
    if !stats.unknown.is_empty() {
        println!("\nunmapped record kinds (harness:kind → count):");
        let mut u: Vec<_> = stats.unknown.iter().collect();
        u.sort_by(|a, b| b.1.cmp(a.1));
        for (k, n) in u.iter().take(40) {
            println!("  {k} {n}");
        }
    }
    if let Some(e) = stats.last_error {
        println!("\nlast read error: {e}");
    }
    Ok(())
}

/// Check crates.io, and unless `--check`, install the new version with cargo and restart the
/// daemon. The default target is the newest **stable** release; a prerelease is only ever
/// announced, and installed solely via the explicit `--pre` opt-in.
fn update(check: bool, prerelease: bool, json: bool) -> Result<()> {
    let info = uniflo_core::update::check();
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        if info.available {
            std::process::exit(10);
        }
        return Ok(());
    }
    if let Some(err) = &info.error {
        println!("uniflo {}: 无法检查更新 — {err}", info.current);
        return Ok(());
    }
    // Decide what (if anything) to install.
    let target: Option<(&str, bool)> = if prerelease {
        info.latest_prerelease.as_deref().map(|v| (v, true))
    } else {
        info.available.then(|| (info.latest.as_deref().unwrap_or("?"), false))
    };
    if prerelease && target.is_none() {
        println!("uniflo {}：crates.io 上没有比你更新的预发布版。", info.current);
        return Ok(());
    }
    match (&info.latest, target) {
        (_, Some((v, true))) => println!("uniflo {} → 预发布版 {}（不保证稳定，已按 --pre 显式选择）", info.current, v),
        (_, Some((v, _))) => println!("uniflo {} → 新版本 {} 可用（正式版）", info.current, v),
        (Some(latest), None) if info.current.contains('-') => {
            println!("uniflo {} 为预发布版；最新正式版 {}。", info.current, latest)
        }
        (Some(latest), None) => println!("uniflo {} 已是最新（crates.io 最新正式版 {}）", info.current, latest),
        (None, None) => println!("uniflo {}: crates.io 没有已发布版本", info.current),
    }
    if !prerelease && let Some(pre) = &info.latest_prerelease {
        println!("检测到预发布 {pre}（不保证稳定）；如需试用：`uniflo update --pre`。");
    }
    let Some((version, is_pre)) = target else { return Ok(()) };
    if check {
        return Ok(());
    }
    let status = std::process::Command::new("cargo")
        .args(["install", "uniflo", "--force", "--version", version])
        .status()
        .with_context(|| format!("cargo 不可用；手动运行 `cargo install uniflo --force --version {version}`"))?;
    if !status.success() {
        bail!("cargo install 失败（exit {:?}）", status.code());
    }
    println!("已安装 uniflo {}{}。", version, if is_pre { "（预发布版）" } else { "" });
    // Replace the running daemon so the new binary actually serves requests.
    #[cfg(target_os = "macos")]
    {
        let loaded = std::process::Command::new("launchctl")
            .args(["list"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("com.crosery.uniflo"))
            .unwrap_or(false);
        if loaded {
            let uid = std::process::Command::new("id")
                .arg("-u")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned());
            let ok = uid.is_some_and(|uid| {
                std::process::Command::new("launchctl")
                    .args(["kickstart", "-k", &format!("gui/{uid}/com.crosery.uniflo")])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            });
            println!(
                "{}",
                if ok {
                    "launchd 服务已重启（KeepAlive），新守护进程在跑。"
                } else {
                    "launchd 服务在跑但自动重启失败：`launchctl kickstart -k gui/$(id -u)/com.crosery.uniflo`"
                }
            );
        } else {
            println!("守护进程若正在运行，重启后生效：`uniflo daemon`（旧进程仍是旧版本）。");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!(
            "守护进程若正在运行，请先停掉再启动新版（Windows 上运行中的二进制无法被覆盖，建议 update 前停 daemon）。"
        );
    }
    setup::after_update();
    Ok(())
}

/// Query commands run against the daemon, or an in-process engine when none answers.
pub(crate) enum Source {
    Daemon(Client),
    Local(Arc<Engine>),
}

impl Source {
    fn open(cli: &Cli) -> Result<Source> {
        if !cli.local {
            let c = Client::new(&cli.url, cli.token.clone())?;
            if c.alive() {
                return Ok(Source::Daemon(c));
            }
            if matches!(cli.cmd, Cmd::Watch { .. }) || matches!(cli.cmd, Cmd::Tail { follow: true, .. }) {
                bail!("no daemon at {} (start one with `uniflo daemon`)", cli.url);
            }
            eprintln!("uniflo: no daemon at {}, indexing in-process", cli.url);
        }
        let engine = Engine::new(uniflo_adapters::all(), EngineOptions::default());
        engine.index();
        let _ = engine.save_cache();
        Ok(Source::Local(engine))
    }

    fn sessions(&self, q: &str, limit: usize) -> Result<Vec<Session>> {
        match self {
            Source::Daemon(c) => c.get_json(&format!("/v1/sessions?limit={limit}&q={}", enc(q))),
            Source::Local(e) => {
                let all = e.sessions();
                Ok(search(&all, &Query::parse(q, now_ms()), limit).into_iter().map(|h| h.session.clone()).collect())
            }
        }
    }

    fn session(&self, key: &str) -> Result<Session> {
        match self {
            Source::Daemon(c) => c.get_json(&format!("/v1/sessions/{}", enc(key))),
            Source::Local(e) => e.session(key).with_context(|| format!("unknown session {key}")),
        }
    }

    /// Resolve a full key, a bare id, or a unique key/id prefix.
    fn resolve(&self, key: &str) -> Result<Session> {
        if let Ok(s) = self.session(key) {
            return Ok(s);
        }
        let hits = self.sessions(&format!("id:{key}"), 2)?;
        match hits.as_slice() {
            [one] => Ok(one.clone()),
            [] => bail!("no session matches {key}"),
            _ => bail!("{key} is ambiguous; use the full key"),
        }
    }

    fn events(&self, key: &str, limit: usize) -> Result<Vec<Event>> {
        match self {
            Source::Daemon(c) => {
                let v: serde_json::Value = c.get_json(&format!("/v1/sessions/{}/events?limit={limit}", enc(key)))?;
                Ok(serde_json::from_value(v["events"].clone())?)
            }
            Source::Local(e) => e.history(key, &HistoryQuery { before: None, limit }),
        }
    }

    fn harnesses(&self) -> Result<Vec<Harness>> {
        match self {
            Source::Daemon(c) => c.get_json("/v1/harnesses"),
            Source::Local(e) => Ok(e.harnesses()),
        }
    }

    fn run(&self, cmd: &Cmd) -> Result<()> {
        let st = Style::detect();
        match cmd {
            Cmd::Ls { query, limit, json, tsv } => {
                let list = self.sessions(&query.join(" "), *limit)?;
                print_sessions(&st, &list, *json, *tsv)
            }
            Cmd::Ps { json } => {
                let list = self.sessions("s:work", 1000)?;
                print_sessions(&st, &list, *json, false)
            }
            Cmd::Show { key, events } => {
                let s = self.resolve(key)?;
                println!("{}", serde_json::to_string_pretty(&s)?);
                println!();
                for e in self.events(&s.key, *events)? {
                    println!("{}", render::event_line(&st, &e, 160));
                }
                Ok(())
            }
            Cmd::Tail { key, lines, follow, json } => {
                let s = self.resolve(key)?;
                for e in self.events(&s.key, *lines)? {
                    print_event(&st, &e, *json)?;
                }
                if *follow {
                    let Source::Daemon(c) = self else { bail!("--follow needs a running daemon") };
                    let since = c.get(&format!("/v1/sessions/{}", enc(&s.key)))?;
                    drop(since);
                    stream(c, &format!("/v1/stream.ndjson?types=event&session={}", enc(&s.key)), |env| {
                        if let Envelope::Event { event, .. } = env {
                            print_event(&st, &event, *json)?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            }
            Cmd::Watch { session, harness, kinds, since } => {
                let Source::Daemon(c) = self else { bail!("watch needs a running daemon") };
                let mut path = "/v1/stream.ndjson?max_text=0".to_owned();
                for (k, v) in [("session", session), ("harness", harness), ("kinds", kinds)] {
                    if let Some(v) = v {
                        path.push_str(&format!("&{k}={}", enc(v)));
                    }
                }
                if let Some(s) = since {
                    path.push_str(&format!("&since={s}"));
                }
                let mut r = c.get(&path)?;
                use std::io::Write;
                let out = std::io::stdout();
                r.lines(|l| {
                    let mut o = out.lock();
                    o.write_all(l).and_then(|_| o.write_all(b"\n")).and_then(|_| o.flush()).is_ok()
                })
            }
            Cmd::Harnesses { json } => {
                let hs = self.harnesses()?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&hs)?);
                } else {
                    for h in hs {
                        let roots = if h.roots.is_empty() { "(not installed)".to_owned() } else { h.roots.join(", ") };
                        println!(
                            "{:<12} {:<16} {:>6} sessions {:>4} working  {roots}",
                            h.id, h.name, h.sessions, h.working
                        );
                    }
                }
                Ok(())
            }
            Cmd::Usage(a) => usage::usage(self, a),
            Cmd::Grep { terms, filter, limit, json } => grep::run(self, terms, filter.as_deref(), *limit, *json),
            Cmd::Resume { key, print } => agent::resume(self, key, *print),
            Cmd::Daemon { .. }
            | Cmd::Scan { .. }
            | Cmd::Update { .. }
            | Cmd::Pricing(_)
            | Cmd::Mcp
            | Cmd::Skill { .. }
            | Cmd::Setup(_)
            | Cmd::Context(_) => unreachable!(),
        }
    }
}

fn stream(c: &Client, path: &str, mut f: impl FnMut(Envelope) -> Result<()>) -> Result<()> {
    let mut r = c.get(path)?;
    let mut err = None;
    r.lines(|l| match serde_json::from_slice::<Envelope>(l) {
        Ok(env) => match f(env) {
            Ok(()) => true,
            Err(e) => {
                err = Some(e);
                false
            }
        },
        Err(_) => true,
    })?;
    err.map_or(Ok(()), Err)
}

fn print_event(st: &Style, e: &Event, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(e)?);
    } else {
        println!("{}", render::event_line(st, e, 200));
    }
    Ok(())
}

fn print_sessions(st: &Style, list: &[Session], json: bool, tsv: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
    } else if tsv {
        for s in list {
            println!("{}", render::session_tsv(s));
        }
    } else {
        for s in list {
            println!("{}", render::session_line(st, s));
        }
        let working = list.iter().filter(|s| s.status == Status::Work).count();
        eprintln!("{} sessions, {working} working", list.len());
    }
    Ok(())
}

/// Percent-encode a query/path component.
pub(crate) fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
