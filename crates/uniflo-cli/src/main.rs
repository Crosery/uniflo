//! `uniflo` — daemon and command line for the unified agent-harness session gateway.

mod client;
mod render;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use client::Client;
use render::Style;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, EngineOptions, HistoryQuery};
use uniflo_gateway::GuardOptions;
use uniflo_schema::{Envelope, Event, Harness, Session, Status};
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { ref bind, ref cors_origins, ref allow_hosts, stale_after, no_cache } => {
            let guard = GuardOptions {
                token: cli.token.clone(),
                cors_origins: cors_origins.clone(),
                allowed_hosts: allow_hosts.clone(),
            };
            if !allow_hosts.is_empty() && guard.token.is_none() {
                bail!("--allow-host exposes transcripts beyond loopback; set --token as well");
            }
            daemon(bind, guard, stale_after, no_cache)
        }
        Cmd::Scan { json, no_cache } => scan(json, no_cache),
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

fn daemon(bind: &str, guard: GuardOptions, stale_after: u64, no_cache: bool) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "uniflo=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let engine = Engine::new(uniflo_adapters::all(), engine_opts(no_cache, stale_after));
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
        let runner = tokio::spawn(engine.clone().run());
        let router = uniflo_gateway::router(engine.clone(), guard);
        uniflo_gateway::serve(listener, router, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
        runner.abort();
        engine.save_cache()?;
        eprintln!("uniflo: index cache saved, bye");
        Ok(())
    })
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

/// Query commands run against the daemon, or an in-process engine when none answers.
enum Source {
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
            Cmd::Daemon { .. } | Cmd::Scan { .. } => unreachable!(),
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
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
