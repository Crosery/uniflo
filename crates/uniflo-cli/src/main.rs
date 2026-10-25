//! `uniflo` — daemon and command line for the unified agent-harness session gateway.

mod agent;
mod clean;
mod client;
mod grep;
mod i18n;
mod mcp;
mod notice;
mod render;
mod setup;
mod ui;
mod update;
mod usage;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::Client;
use i18n::tr;
use render::Style;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uniflo_core::cleanup::{Cleanup, CleanupOptions};
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, EngineOptions, HistoryQuery, PriceSync};
use uniflo_gateway::GuardOptions;
use uniflo_schema::{Envelope, Event, Harness, Session, Status};
use uniflo_search::fts::{Fts, FtsOptions};
use uniflo_search::{Query, search};

pub(crate) const DEFAULT_URL: &str = "http://127.0.0.1:7311";

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
        /// Sessions active within this many days stay in the full-text index's hot (position-aware)
        /// tier; older ones compact into the cold (block-level) tier (ADR-0011).
        #[arg(long, default_value_t = uniflo_search::fts::DEFAULT_HOT_DAYS)]
        fts_hot_days: u32,
        /// Disable every write endpoint (cleanup, archive deletion): they answer 403.
        #[arg(long)]
        read_only: bool,
        /// Run in the foreground instead of detaching into the background.
        #[arg(short = 'f', long)]
        foreground: bool,
        /// Stop the running background daemon.
        #[arg(long)]
        stop: bool,
        /// Restart the running background daemon.
        #[arg(long)]
        restart: bool,
    },
    /// Check crates.io for a newer stable release, or install it the way this executable was
    /// installed (`cargo install`, or the prebuilt release package for an install-script install).
    Update {
        /// Only report; do not install.
        #[arg(long)]
        check: bool,
        /// Install the newest prerelease instead of the stable release (explicit opt-in;
        /// prereleases do not guarantee stability).
        #[arg(long = "pre")]
        prerelease: bool,
        /// The check result plus `method` (cargo|binary|unknown); exit code 10 = update available.
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
    /// Archive sessions, then move their files to the trash (plan first; asks unless --yes).
    Clean(clean::CleanArgs),
    /// Archived (cleaned-up) sessions: `ls` (default) or `rm <key>`.
    Archive(clean::ArchiveArgs),
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

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("uniflo: {}{e:#}", tr!("错误：", "error: "));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        uniflo_core::install::remove_aside(&exe);
    }
    let cli = Cli::parse();
    // Commands that print for people get the "newer version" box afterwards; long-running and
    // agent-facing ones (and `update` itself) never do.
    let notice = notice::begin(!matches!(
        cli.cmd,
        Cmd::Daemon { .. } | Cmd::Mcp | Cmd::Setup(_) | Cmd::Skill { .. } | Cmd::Context(_) | Cmd::Update { .. }
    ));
    let res = dispatch(cli);
    notice.show();
    res
}

fn dispatch(cli: Cli) -> Result<()> {
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
            fts_hot_days,
            read_only,
            foreground,
            stop,
            restart,
        } => {
            let guard = GuardOptions {
                token: cli.token.clone(),
                cors_origins: cors_origins.clone(),
                allowed_hosts: allow_hosts.clone(),
                read_only,
            };
            if !allow_hosts.is_empty() && guard.token.is_none() {
                bail!(tr!(
                    "--allow-host 会把会话内容暴露到回环地址以外，必须同时设置 --token",
                    "--allow-host exposes transcripts beyond loopback; set --token as well"
                ));
            }
            if stop {
                stop_daemon(false)?;
                return Ok(());
            }
            if restart {
                let _ = stop_daemon(true);
            }
            use std::io::IsTerminal;
            let detach =
                !foreground && (restart || (std::io::stdin().is_terminal() && std::io::stderr().is_terminal()));
            if detach {
                return spawn_detached(bind, cli.token.clone());
            }
            let price_sync = (!no_price_sync)
                .then(|| PriceSync { delay: Duration::from_secs(price_sync_delay), ..Default::default() });
            daemon(bind, guard, stale_after, no_cache, !no_update_check, price_sync, !no_fts, fts_hot_days)
        }
        Cmd::Scan { json, no_cache } => scan(json, no_cache),
        Cmd::Update { check, prerelease, json } => update::run(check, prerelease, json),
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

fn pid_file() -> std::path::PathBuf {
    uniflo_core::paths::cache_dir().join("daemon.pid")
}

fn log_file() -> std::path::PathBuf {
    uniflo_core::paths::cache_dir().join("daemon.log")
}

pub(crate) fn read_daemon_pid() -> Option<u32> {
    let raw = std::fs::read_to_string(pid_file()).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    uniflo_core::util::pid_alive(pid).then_some(pid)
}

fn launch_file() -> std::path::PathBuf {
    uniflo_core::paths::cache_dir().join("daemon.json")
}

/// How a background daemon was started, kept next to its pid file so `uniflo update` can start
/// the new version the same way. Never holds a token value.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Launch {
    pub pid: u32,
    pub exe: String,
    /// Arguments after the executable, without `--foreground` and `--token`.
    pub args: Vec<String>,
    /// A token was in effect (`--token` or `UNIFLO_TOKEN`).
    #[serde(default)]
    pub token: bool,
}

impl Launch {
    fn new(pid: u32, exe: String, argv: impl IntoIterator<Item = String>, token: bool) -> Launch {
        let mut args = Vec::new();
        let mut token = token;
        let mut it = argv.into_iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--token" => {
                    it.next();
                    token = true;
                }
                t if t.starts_with("--token=") => token = true,
                "--foreground" | "-f" => {}
                _ => args.push(a),
            }
        }
        Launch { pid, exe, args, token }
    }

    /// The recorded launch of the daemon `pid`; for daemons that predate the record, what `ps`
    /// reports (an executable path with spaces then fails to match and nothing is restarted).
    pub fn find(pid: u32) -> Option<Launch> {
        let rec: Option<Launch> = std::fs::read(launch_file()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        if let Some(l) = rec.filter(|l| l.pid == pid) {
            return Some(l);
        }
        #[cfg(unix)]
        {
            let out =
                std::process::Command::new("ps").args(["-ww", "-o", "args=", "-p", &pid.to_string()]).output().ok()?;
            let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            let mut words = line.split_whitespace().map(str::to_owned);
            let exe = words.next()?;
            Some(Launch::new(pid, exe, words, false))
        }
        #[cfg(not(unix))]
        None
    }

    /// `--bind` of the recorded arguments, else the default.
    pub fn bind(&self) -> String {
        let mut it = self.args.iter();
        while let Some(a) = it.next() {
            if a == "--bind" {
                return it.next().cloned().unwrap_or_default();
            }
            if let Some(b) = a.strip_prefix("--bind=") {
                return b.to_owned();
            }
        }
        DEFAULT_URL.trim_start_matches("http://").to_owned()
    }
}

pub(crate) fn stop_daemon(quiet: bool) -> Result<bool> {
    let Some(pid) = read_daemon_pid() else {
        let _ = std::fs::remove_file(pid_file());
        if !quiet {
            eprintln!("uniflo: {}", tr!("没有在运行的后台守护进程", "no background daemon is running"));
        }
        return Ok(false);
    };
    let _ = std::fs::remove_file(pid_file());
    let t0 = Instant::now();
    while uniflo_core::util::pid_alive(pid) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !quiet {
        eprintln!("uniflo: {}", tr!("已停止后台守护进程（pid {pid}）", "stopped background daemon (pid {pid})"));
    }
    Ok(true)
}

/// `uniflo daemon` in a terminal: start this executable in the background with this command's
/// arguments.
pub(crate) fn spawn_detached(bind: &str, token: Option<String>) -> Result<()> {
    let url = format!("http://{bind}");
    let exe = std::env::current_exe().context("locate current executable")?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args: Vec<std::ffi::OsString> = if args.iter().any(|a| a == "daemon") {
        args.into_iter().filter(|a| a != "--restart").collect()
    } else {
        vec!["daemon".into()]
    };
    let log_path = log_file();
    match start_background(&exe, &args, bind, token)? {
        Started::Already(pid) => {
            let pid_note = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            eprintln!(
                "uniflo: {}",
                tr!(
                    "守护进程已在运行{pid_note}：{url}（重启：`uniflo daemon --restart`，停止：`uniflo daemon --stop`）",
                    "daemon is already running{pid_note} on {url} (use `uniflo daemon --restart` or `uniflo daemon --stop`)"
                )
            );
        }
        Started::Up { pid, sessions } => {
            eprintln!(
                "uniflo: {}",
                tr!(
                    "守护进程已在后台启动（pid {pid}）· {sessions} 个会话 · 监听 {url}",
                    "daemon started in background (pid {pid}) · {sessions} sessions · listening on {url}"
                )
            );
            eprintln!(
                "uniflo: {}",
                tr!(
                    "日志 {}（停止：`uniflo daemon --stop`，前台运行：`uniflo daemon -f`）",
                    "log at {} (stop: `uniflo daemon --stop`, foreground: `uniflo daemon -f`)",
                    log_path.display()
                )
            );
        }
        Started::Slow { pid } => {
            eprintln!(
                "uniflo: {}",
                tr!(
                    "守护进程正在后台启动（pid {pid}）· 监听 {url}",
                    "daemon starting in background (pid {pid}) · listening on {url}"
                )
            );
            eprintln!(
                "uniflo: {}",
                tr!(
                    "日志 {}（停止：`uniflo daemon --stop`）",
                    "log at {} (stop: `uniflo daemon --stop`)",
                    log_path.display()
                )
            );
        }
    }
    Ok(())
}

pub(crate) enum Started {
    /// Something already answers on the address (the pid when it is our background daemon).
    Already(Option<u32>),
    Up {
        pid: u32,
        sessions: u64,
    },
    /// Still indexing after 15 s.
    Slow {
        pid: u32,
    },
}

/// Start `exe <args> --foreground` detached from this terminal, logging to the daemon log, and
/// wait until it answers on `bind`.
pub(crate) fn start_background(
    exe: &std::path::Path,
    args: &[std::ffi::OsString],
    bind: &str,
    token: Option<String>,
) -> Result<Started> {
    let url = format!("http://{bind}");
    let client = Client::new(&url, token)?;
    if client.alive() {
        return Ok(Started::Already(read_daemon_pid()));
    }
    let log_path = log_file();
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let err = out.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .arg("--foreground")
        .env("UNIFLO_DETACHED", "1")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        uniflo_core::util::stdio_not_inheritable();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().context(tr!("启动后台守护进程", "spawn background daemon"))?;
    let pid = child.id();

    let t0 = Instant::now();
    loop {
        if let Ok(mut r) = client.get("/v1/health")
            && r.status == 200
        {
            let v: serde_json::Value = serde_json::from_slice(&r.body().unwrap_or_default()).unwrap_or_default();
            return Ok(Started::Up { pid, sessions: v["sessions"].as_u64().unwrap_or(0) });
        }
        if let Ok(Some(status)) = child.try_wait() {
            let _ = std::fs::remove_file(pid_file());
            bail!(tr!(
                "守护进程提前退出（{status}）；见 {}",
                "daemon exited early ({status}); see {}",
                log_path.display()
            ));
        }
        if t0.elapsed() > Duration::from_secs(15) {
            return Ok(Started::Slow { pid });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[allow(clippy::too_many_arguments)]
fn daemon(
    bind: &str,
    guard: GuardOptions,
    stale_after: u64,
    no_cache: bool,
    update_check: bool,
    price_sync: Option<PriceSync>,
    fts: bool,
    fts_hot_days: u32,
) -> Result<()> {
    let token = guard.token.is_some();
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
        let fts = fts.then(|| start_fts(&engine, fts_hot_days)).flatten();
        let cleanup = Cleanup::new(engine.clone(), CleanupOptions::from_env()).ok().map(Arc::new);
        let runner = tokio::spawn(engine.clone().run());
        let router = uniflo_gateway::router_with(engine.clone(), guard, uniflo_gateway::Services { fts, cleanup });
        let my_pid = std::process::id();
        let tracked = std::env::var_os("UNIFLO_DETACHED").is_some();
        if tracked {
            if let Some(dir) = pid_file().parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(pid_file(), format!("{my_pid}\n"));
            let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default();
            let argv = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned());
            let launch = Launch::new(my_pid, exe, argv, token);
            let _ = std::fs::write(launch_file(), serde_json::to_vec(&launch).unwrap_or_default());
        }
        uniflo_gateway::serve(listener, router, async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    if !tracked {
                        std::future::pending::<()>().await;
                    }
                    loop {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        if read_daemon_pid() != Some(my_pid) {
                            break;
                        }
                    }
                } => {}
            }
        })
        .await?;
        runner.abort();
        engine.save_cache()?;
        engine.save_usage_cache()?;
        if read_daemon_pid() == Some(std::process::id()) {
            let _ = std::fs::remove_file(pid_file());
        }
        let ours = std::fs::read(launch_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<Launch>(&b).ok())
            .is_some_and(|l| l.pid == my_pid);
        if ours {
            let _ = std::fs::remove_file(launch_file());
        }
        eprintln!("uniflo: index cache saved, bye");
        Ok(())
    })
}

/// The index builds in its own thread; failing to open it only disables `/v1/search`.
fn start_fts(engine: &Arc<Engine>, hot_days: u32) -> Option<Arc<Fts>> {
    match Fts::start(engine.clone(), FtsOptions { hot_days, ..FtsOptions::default() }) {
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
    let (mb, ms) = (stats.bytes / 1_000_000, t0.elapsed().as_millis());
    println!(
        "\n{}",
        tr!(
            "{} 个会话 · {} 个数据源（读取 {}，缓存 {}）· 解析 {mb} MB · 坏行 {} · 读错 {} · {ms} ms",
            "{} sessions · {} sources ({} read, {} from cache) · {mb} MB parsed · {} bad lines · {} read errors · {ms} ms",
            report.sessions,
            report.files,
            report.read,
            report.restored,
            stats.bad_lines,
            stats.read_errors
        )
    );
    if !stats.unknown.is_empty() {
        println!(
            "\n{}",
            tr!("未映射的记录类型（harness:kind → 次数）：", "unmapped record kinds (harness:kind → count):")
        );
        let mut u: Vec<_> = stats.unknown.iter().collect();
        u.sort_by(|a, b| b.1.cmp(a.1));
        for (k, n) in u.iter().take(40) {
            println!("  {k} {n}");
        }
    }
    if let Some(e) = stats.last_error {
        println!("\n{}", tr!("最近一次读取错误：{e}", "last read error: {e}"));
    }
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
                bail!(tr!(
                    "{} 上没有守护进程（启动：`uniflo daemon`）",
                    "no daemon at {} (start one with `uniflo daemon`)",
                    cli.url
                ));
            }
            eprintln!(
                "uniflo: {}",
                tr!("{} 上没有守护进程，改为在进程内建索引", "no daemon at {}, indexing in-process", cli.url)
            );
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
            Source::Local(e) => e.session(key).with_context(|| tr!("没有会话 {key}", "unknown session {key}")),
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
            [] => bail!(tr!("没有匹配 {key} 的会话", "no session matches {key}")),
            _ => bail!(tr!("{key} 匹配多个会话，请用完整的 key", "{key} is ambiguous; use the full key")),
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
                    let Source::Daemon(c) = self else {
                        bail!(tr!("--follow 需要在运行的守护进程", "--follow needs a running daemon"))
                    };
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
                let Source::Daemon(c) = self else {
                    bail!(tr!("watch 需要在运行的守护进程", "watch needs a running daemon"))
                };
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
                        let roots = if h.roots.is_empty() {
                            tr!("（未安装）", "(not installed)")
                        } else {
                            h.roots.join(", ")
                        };
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
            Cmd::Clean(a) => clean::clean(self, a),
            Cmd::Archive(a) => clean::archive(self, a),
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
        eprintln!("{}", tr!("{} 个会话，{working} 个工作中", "{} sessions, {working} working", list.len()));
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

#[cfg(test)]
mod tests {
    use super::Launch;

    #[test]
    fn launch_record_drops_token_values_and_foreground() {
        let argv = ["daemon", "--token", "s3cret", "--bind=127.0.0.1:7400", "-f", "--read-only", "--foreground"];
        let l = Launch::new(7, "/x/uniflo".into(), argv.map(String::from), false);
        assert_eq!(l.args, ["daemon", "--bind=127.0.0.1:7400", "--read-only"]);
        assert!(l.token && !serde_json::to_string(&l).unwrap().contains("s3cret"));
        assert_eq!(l.bind(), "127.0.0.1:7400");
        let l = Launch::new(
            7,
            "/x/uniflo".into(),
            ["daemon", "--token=abc", "--bind", "127.0.0.1:7401"].map(String::from),
            false,
        );
        assert_eq!((l.args.len(), l.token, l.bind().as_str()), (3, true, "127.0.0.1:7401"));
        let l = Launch::new(7, "/x/uniflo".into(), ["daemon".to_owned()], true);
        assert_eq!((l.token, l.bind().as_str()), (true, "127.0.0.1:7311"));
    }
}
