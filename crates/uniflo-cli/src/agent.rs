//! Agent-facing commands: `uniflo resume`, `uniflo context` and `uniflo skill`.

use crate::client::Client;
use crate::{Cli, Source, enc};
use anyhow::{Context as _, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::path::{Path, PathBuf};
use uniflo_core::usage::report::{parse_time, project_of};
use uniflo_core::usage::tz::Tz;
use uniflo_core::util::now_ms;
use uniflo_schema::{ContextReport, ResumeInfo, Session};

/// The agent Skill shipped with the binary (`uniflo skill print`, installed by `uniflo setup`).
pub const SKILL: &str = include_str!("../skill/SKILL.md");

#[derive(Subcommand)]
pub enum SkillAction {
    /// Print SKILL.md.
    Print,
}

#[derive(Args)]
pub struct ContextArgs {
    /// Project directory; sessions are matched by its git root.
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
    #[arg(short = 'n', long, default_value_t = 5)]
    pub limit: usize,
    /// `30m`, `2h`, `14d`, `YYYY-MM-DD` or epoch ms.
    #[arg(long, default_value = "14d")]
    pub since: String,
    #[arg(long)]
    pub json: bool,
}

pub fn skill(a: &SkillAction) {
    match a {
        SkillAction::Print => print!("{SKILL}"),
    }
}

/// Continue a session: print its command, or `exec` it from the session's cwd. Exit code 2 when
/// the session cannot be resumed.
pub fn resume(src: &Source, key: &str, print: bool) -> Result<()> {
    let s = src.resolve(key)?;
    let info: ResumeInfo = match src {
        Source::Daemon(c) => c.get_json(&format!("/v1/sessions/{}/resume", enc(&s.key)))?,
        Source::Local(_) => uniflo_core::resume::resume(&s),
    };
    if !info.supported {
        eprintln!("uniflo: cannot resume {}: {}", s.key, info.reason.as_deref().unwrap_or("unsupported"));
        std::process::exit(2);
    }
    if print {
        println!("{}", info.command.as_deref().unwrap_or_default());
        return Ok(());
    }
    run(&s, &info)
}

fn run(s: &Session, info: &ResumeInfo) -> Result<()> {
    let needs_cwd = uniflo_core::resume::argv(&s.harness, &s.id).is_some_and(|(_, c)| c);
    let dir = info.cwd.as_deref().filter(|c| Path::new(c).is_dir());
    if needs_cwd && dir.is_none() {
        bail!("{} resumes per directory, and {} no longer exists", s.harness, info.cwd.as_deref().unwrap_or("?"));
    }
    let (program, args) = info.argv.split_first().ok_or_else(|| anyhow!("empty resume command"))?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err).with_context(|| format!("run {program}"))
    }
    #[cfg(not(unix))]
    {
        let st = cmd.status().with_context(|| format!("run {program}"))?;
        std::process::exit(st.code().unwrap_or(1));
    }
}

/// Recent sessions of `--cwd`'s project from the daemon. Never indexes in-process and never
/// fails: without a daemon (or on any error) it prints nothing, so a SessionStart hook can run it.
pub fn context(cli: &Cli, a: &ContextArgs) -> Result<()> {
    let Ok(c) = Client::new(&cli.url, cli.token.clone()) else { return Ok(()) };
    let cwd = absolute(&a.cwd);
    let report = match build_context(&mut |p| c.get_json(p), &cwd, a.limit, &a.since) {
        Ok(r) => r,
        Err(e) => {
            if e.downcast_ref::<std::io::Error>().is_none() {
                eprintln!("uniflo context: {e:#}");
            }
            return Ok(());
        }
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", uniflo_core::context::markdown(&report));
    }
    Ok(())
}

/// Sessions of `cwd`'s git root through any `GET` returning JSON (daemon or in-process routes).
pub fn build_context(
    get: &mut dyn FnMut(&str) -> Result<Value>,
    cwd: &Path,
    limit: usize,
    since: &str,
) -> Result<ContextReport> {
    let now = now_ms();
    let since = parse_time(since, now, &Tz::parse("").map_err(|e| anyhow!(e))?).map_err(|e| anyhow!(e))?;
    let project = project_of(&cwd.display().to_string());
    // A superset in the session search syntax keeps the response small; exact matching follows.
    let days = (now - since).max(0) / 86_400_000 + 1;
    let mut q = format!("is:root since:{days}d");
    if let Some(base) = Path::new(&project).file_name().and_then(|b| b.to_str())
        && !base.is_empty()
        && base.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c))
    {
        q.push_str(&format!(" in:{base}"));
    }
    let list: Vec<Session> = serde_json::from_value(get(&format!("/v1/sessions?limit=100000&q={}", enc(&q)))?)?;
    Ok(uniflo_core::context::report(&list, &project, since, limit.max(1)))
}

/// `p` made absolute without resolving symlinks (harnesses record the logical path, e.g.
/// `/tmp/x`, not `/private/tmp/x`): relative paths start from `$PWD` when it is the current
/// directory.
pub fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    }
    let pwd = std::env::var_os("PWD").map(PathBuf::from).filter(|d| {
        d.is_absolute()
            && std::fs::canonicalize(d).ok() == std::env::current_dir().ok().and_then(|c| c.canonicalize().ok())
    });
    match pwd {
        Some(d) => std::path::absolute(d.join(p)).unwrap_or_else(|_| d.join(p)),
        None => std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()),
    }
}
