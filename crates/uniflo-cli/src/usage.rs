//! `uniflo usage` and `uniflo pricing`: token / cost tables over the daemon's `/v1/usage`,
//! `/v1/sessions/{key}/usage` and `/v1/pricing`, or the same library calls in-process.

use crate::client::Client;
use crate::{Cli, Source, enc};
use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::io::Write;
use uniflo_core::pricing::{Pricing, sync};
use uniflo_gateway::usage::UsageParams;
use uniflo_schema::{PricingStatus, SessionUsageDetail, UsageReport, UsageRow};

#[derive(Args)]
pub struct UsageArgs {
    /// Session filter in search syntax (`h:claude in:~/work since:7d …`), or one session key
    /// for its per-step usage.
    pub query: Vec<String>,
    /// harness, model, project, cwd, dir, day, hour, weekday, weekday_hour or session.
    #[arg(long = "by", default_value = "harness")]
    pub by: String,
    /// Event-time window start: `30m`, `2h`, `7d`, `YYYY-MM-DD` or epoch ms.
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    /// Only cwds at or below this directory (`--by dir` lists its children).
    #[arg(long)]
    pub under: Option<String>,
    /// Path components below the base for `--by dir`.
    #[arg(long)]
    pub depth: Option<usize>,
    /// Zone for day / hour / weekday buckets: local (default), UTC, +08:00, Asia/Shanghai.
    #[arg(long)]
    pub tz: Option<String>,
    /// Keep this many rows; the rest fold into `(other)`.
    #[arg(short = 'n', long)]
    pub limit: Option<usize>,
    /// cost, tokens, steps, sessions, prompts or key.
    #[arg(long)]
    pub sort: Option<String>,
    /// Only steps of this model (a `--by model` key).
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct PricingArgs {
    #[command(subcommand)]
    pub action: Option<PricingAction>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum PricingAction {
    /// Fetch the price catalog now (models.dev, LiteLLM fallback) into the data directory.
    Sync,
}

impl UsageArgs {
    fn params(&self) -> UsageParams {
        let q = self.query.join(" ");
        UsageParams {
            group_by: Some(self.by.clone()),
            q: (!q.is_empty()).then_some(q),
            since: self.since.clone(),
            until: self.until.clone(),
            tz: self.tz.clone(),
            under: self.under.clone(),
            depth: self.depth,
            limit: self.limit,
            sort: self.sort.clone(),
            model: self.model.clone(),
        }
    }
}

pub fn usage(src: &Source, a: &UsageArgs) -> Result<()> {
    if let [one] = a.query.as_slice()
        && let Ok(s) = src.session(one)
    {
        return session(src, &s.key, a.json);
    }
    let p = a.params();
    let json = match src {
        Source::Daemon(c) => fetch(c, &format!("/v1/usage?{}", query_string(&p)))?,
        Source::Local(e) => {
            e.index_usage();
            serde_json::to_vec_pretty(&p.report(e).map_err(|m| anyhow!(m))?)?
        }
    };
    if a.json {
        return emit(&json);
    }
    let r: UsageReport = serde_json::from_slice(&json)?;
    print_report(&r);
    Ok(())
}

fn session(src: &Source, key: &str, json: bool) -> Result<()> {
    let body = match src {
        Source::Daemon(c) => fetch(c, &format!("/v1/sessions/{}/usage", enc(key)))?,
        Source::Local(e) => serde_json::to_vec_pretty(&e.session_usage(key)?)?,
    };
    if json {
        return emit(&body);
    }
    let d: SessionUsageDetail = serde_json::from_slice(&body)?;
    print_detail(&d);
    Ok(())
}

/// Runs without indexing: status comes from the daemon when one answers, else from the
/// data directory; `sync` always runs here and a running daemon reloads the files.
pub fn pricing(cli: &Cli, a: &PricingArgs) -> Result<()> {
    let local = Pricing::new(Some(uniflo_core::paths::data_dir().join("pricing")));
    if let Some(PricingAction::Sync) = a.action {
        let r = local.sync_now(&sync::Sources::from_env());
        if a.json {
            println!("{}", serde_json::to_string_pretty(&r)?);
        } else {
            match (&r.fetched_at, &r.error) {
                (Some(_), err) => {
                    println!(
                        "synced {} models: {} price changes, {} added, {} waiting for a second read",
                        r.models,
                        r.changed.len(),
                        r.added,
                        r.pending.len()
                    );
                    if let Some(e) = err {
                        println!("partial failure: {e}");
                    }
                }
                (None, err) => println!("sync failed, previous catalog kept: {}", err.as_deref().unwrap_or("?")),
            }
        }
        if r.fetched_at.is_none() {
            std::process::exit(1);
        }
        return Ok(());
    }
    let st: PricingStatus = match (!cli.local).then(|| Client::new(&cli.url, cli.token.clone())) {
        Some(Ok(c)) if c.alive() => c.get_json("/v1/pricing")?,
        _ => local.status(uniflo_core::util::now_ms()),
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(());
    }
    let when = st.fetched_at.map_or_else(|| "never (embedded snapshot)".to_owned(), date_time);
    println!("source     {}", st.source);
    println!("fetched    {when}{}", if st.stale { "  (stale)" } else { "" });
    println!("models     {}  ·  overrides {}  ·  pending {}", st.models, st.overrides, st.pending);
    println!("sync       {}", if st.sync_enabled { "on" } else { "off" });
    if let Some(e) = &st.error {
        println!("error      {e}");
    }
    Ok(())
}

/// The daemon's JSON exactly as sent: decoding and re-encoding could move float digits.
fn fetch(c: &Client, path: &str) -> Result<Vec<u8>> {
    let mut r = c.get(path)?;
    let body = r.body()?;
    if r.status != 200 {
        bail!("HTTP {}: {}", r.status, String::from_utf8_lossy(&body));
    }
    Ok(body)
}

fn emit(json: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(json)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn query_string(p: &UsageParams) -> String {
    let mut out = Vec::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("{k}={}", enc(&v)));
        }
    };
    put("group_by", p.group_by.clone());
    put("q", p.q.clone());
    put("since", p.since.clone());
    put("until", p.until.clone());
    put("tz", p.tz.clone());
    put("under", p.under.clone());
    put("depth", p.depth.map(|d| d.to_string()));
    put("limit", p.limit.map(|d| d.to_string()));
    put("sort", p.sort.clone());
    put("model", p.model.clone());
    out.join("&")
}

fn print_report(r: &UsageReport) {
    let w = r.rows.iter().map(|x| x.label.chars().count()).max().unwrap_or(0).clamp(8, 48);
    println!(
        "{:<w$} {:>6} {:>7} {:>7} {:>8} {:>8} {:>9} {:>9} {:>11} {:>8}",
        r.group_by, "sess", "prompts", "steps", "input", "output", "cache_rd", "cache_wr", "cost_usd", "unpriced"
    );
    for x in &r.rows {
        println!("{}", row(&cut(&x.label, w), x, w));
    }
    println!("{}", row("total", &r.totals, w));
    let mut notes = Vec::new();
    if !r.indexing.ready {
        notes.push(format!("still indexing {}/{} sources: totals will grow", r.indexing.done, r.indexing.total));
    }
    if r.totals.unpriced_steps > 0 {
        notes.push(format!("{} steps have no price (unknown model): not counted as $0", r.totals.unpriced_steps));
    }
    let prices = r.pricing.fetched_at.map_or_else(|| "embedded snapshot".to_owned(), date_time);
    notes.push(format!(
        "prices: {prices}{} · API-equivalent cost, not a bill",
        if r.pricing.stale { " (stale)" } else { "" }
    ));
    for n in notes {
        eprintln!("{n}");
    }
}

fn row(label: &str, x: &UsageRow, w: usize) -> String {
    format!(
        "{label:<w$} {:>6} {:>7} {:>7} {:>8} {:>8} {:>9} {:>9} {:>11} {:>8}",
        x.sessions,
        x.prompts,
        x.steps,
        tokens(x.input),
        tokens(x.output),
        tokens(x.cache_read),
        tokens(x.cache_write),
        money(x.cost_usd),
        x.unpriced_steps
    )
}

fn print_detail(d: &SessionUsageDetail) {
    println!(
        "{:<19} {:>4} {:<28} {:>8} {:>8} {:>9} {:>9} {:>10} {:<8} {:>6}",
        "time", "turn", "model", "input", "output", "cache_rd", "cache_wr", "cost_usd", "source", "ctx%"
    );
    for s in &d.steps {
        let src = s.cost_source.map_or("-".to_owned(), |c| format!("{c:?}").to_lowercase());
        let pct = s.context_pct.map_or("-".to_owned(), |p| format!("{p:.1}"));
        println!(
            "{:<19} {:>4} {:<28} {:>8} {:>8} {:>9} {:>9} {:>10} {:<8} {:>6}",
            date_time(s.ts),
            s.turn,
            cut(s.model.as_deref().unwrap_or("?"), 28),
            tokens(s.input),
            tokens(s.output),
            tokens(s.cache_read),
            tokens(s.cache_write),
            money(s.cost_usd),
            src,
            pct
        );
    }
    println!();
    println!(
        "{:>4} {:>7} {:>6} {:>8} {:>8} {:>9} {:>9} {:>10} {:>8}",
        "turn", "prompts", "steps", "input", "output", "cache_rd", "cache_wr", "cost_usd", "unpriced"
    );
    for t in &d.turns {
        println!(
            "{:>4} {:>7} {:>6} {:>8} {:>8} {:>9} {:>9} {:>10} {:>8}",
            t.turn,
            t.prompts,
            t.steps,
            tokens(t.input),
            tokens(t.output),
            tokens(t.cache_read),
            tokens(t.cache_write),
            money(t.cost_usd),
            t.unpriced_steps
        );
    }
    let u = &d.totals;
    let limit = u.context_limit.map_or_else(|| "?".to_owned(), tokens);
    eprintln!(
        "{} steps · {} · {} unpriced · last context {} / {limit}",
        u.steps,
        money(u.cost_usd),
        u.unpriced_steps,
        tokens(u.last_context_tokens)
    );
}

fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

fn money(c: Option<f64>) -> String {
    match c {
        None => "-".into(),
        Some(c) if c != 0.0 && c.abs() < 0.01 => format!("${c:.4}"),
        Some(c) => format!("${c:.2}"),
    }
}

fn date_time(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(|| ms.to_string(), |d| d.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn cut(s: &str, w: usize) -> String {
    if s.chars().count() <= w { s.to_owned() } else { format!("{}…", s.chars().take(w - 1).collect::<String>()) }
}
