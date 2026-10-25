//! `uniflo clean` (plan → confirm → archive + trash) and `uniflo archive [ls|rm <key>]`, against
//! the daemon's write endpoints or, without a daemon, in-process.

use crate::i18n::tr;
use crate::render::date_time;
use crate::{Source, enc};
use anyhow::{Result, bail};
use std::io::{BufRead, IsTerminal, Write};
use uniflo_core::Engine;
use uniflo_core::cleanup::{Cleanup, CleanupOptions, ExecError};
use uniflo_schema::cleanup::{ArchiveList, ArchiveRemoved, CleanupPlan, CleanupReport, CleanupRequest, CleanupStatus};

#[derive(clap::Args)]
pub struct CleanArgs {
    /// Session keys (or unique key / id prefixes).
    keys: Vec<String>,
    /// Pick sessions with the `uniflo ls` syntax instead, e.g. `h:claude before:30d`.
    #[arg(long)]
    query: Option<String>,
    /// Only print the plan; change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Execute without asking (required when stdin is not a terminal).
    #[arg(short, long)]
    yes: bool,
    /// Print the plan (with --dry-run) or the result as JSON, as the REST API returns them.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub struct ArchiveArgs {
    #[command(subcommand)]
    cmd: Option<ArchiveCmd>,
    /// Print JSON, as the REST API returns it.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(clap::Subcommand)]
enum ArchiveCmd {
    /// List archived sessions and their sizes (default).
    Ls,
    /// Permanently delete an archive (a session's sub-agent archives go with it).
    Rm { key: String },
}

fn local(engine: &std::sync::Arc<Engine>) -> Result<Cleanup> {
    Cleanup::new(engine.clone(), CleanupOptions::from_env())
}

pub fn clean(src: &Source, a: &CleanArgs) -> Result<()> {
    if a.keys.is_empty() && a.query.is_none() {
        bail!(tr!(
            "请指定会话（`uniflo clean <key>…`）或用 --query 选择",
            "name sessions (`uniflo clean <key>…`) or select them with --query"
        ));
    }
    // Unresolvable keys go through as given: the plan reports them as unknown.
    let sessions = a.keys.iter().map(|k| src.resolve(k).map_or_else(|_| k.clone(), |s| s.key)).collect();
    let req = CleanupRequest { sessions, q: a.query.clone() };
    let (plan, svc): (CleanupPlan, Option<Cleanup>) = match src {
        Source::Daemon(c) => (c.write_json("POST", "/v1/cleanup/plan", Some(&serde_json::to_value(&req)?))?, None),
        Source::Local(engine) => {
            let svc = local(engine)?;
            (svc.plan(&uniflo_gateway::cleanup::select(engine, &req)), Some(svc))
        }
    };
    if a.json && a.dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    print_plan(&plan, a.json);
    let eligible = plan.sessions.iter().filter(|c| c.eligible).count();
    if a.dry_run || eligible == 0 {
        return Ok(());
    }
    if !a.yes {
        if !std::io::stdin().is_terminal() {
            bail!(tr!(
                "当前不是终端：加 `--yes` 执行这个计划（或加 `--dry-run` 只查看）",
                "not a terminal: add --yes to execute this plan (or --dry-run to only look)"
            ));
        }
        eprint!(
            "{}",
            tr!(
                "归档后把 {eligible} 个会话移入回收站？[y/N] ",
                "move {eligible} session(s) to the trash after archiving them? [y/N] "
            )
        );
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            bail!(tr!("已取消，没有改动", "cancelled; nothing changed"));
        }
    }
    let report: CleanupReport = match (src, svc) {
        (Source::Daemon(c), _) => {
            c.write_json("POST", &format!("/v1/cleanup/plans/{}/execute", enc(&plan.plan_id)), None)?
        }
        (_, Some(svc)) => match svc.execute(&plan.plan_id) {
            Ok(r) => r,
            Err(ExecError::Expired) => bail!(tr!("计划已过期，请重新运行", "plan expired; run again")),
            Err(ExecError::Unknown) => bail!(tr!("未知的计划", "unknown plan")),
        },
        (Source::Local(_), None) => unreachable!(),
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    if report.results.iter().any(|r| r.status == CleanupStatus::Failed) {
        std::process::exit(1);
    }
    Ok(())
}

pub fn archive(src: &Source, a: &ArchiveArgs) -> Result<()> {
    match &a.cmd {
        None | Some(ArchiveCmd::Ls) => {
            let list: ArchiveList = match src {
                Source::Daemon(c) => c.get_json("/v1/archive")?,
                Source::Local(engine) => local(engine)?.archives(),
            };
            if a.json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            for e in &list.archives {
                let untitled = tr!("（无标题）", "(untitled)");
                let title = e.title.as_deref().unwrap_or(&untitled);
                let restored = if e.restored { tr!("  [已还原]", "  [restored]") } else { String::new() };
                println!(
                    "{}",
                    tr!(
                        "{:<48} {:>9}  (原 {:>9})  {}  {title}{restored}",
                        "{:<48} {:>9}  (was {:>9})  {}  {title}{restored}",
                        e.key,
                        bytes(e.bytes),
                        bytes(e.source_bytes),
                        date_time(e.archived_at)
                    )
                );
            }
            eprintln!("{}", tr!("{} 个归档，共 {}", "{} archive(s), {}", list.archives.len(), bytes(list.bytes)));
        }
        Some(ArchiveCmd::Rm { key }) => {
            let r: ArchiveRemoved = match src {
                Source::Daemon(c) => c.write_json("DELETE", &format!("/v1/archive/{}", enc(key)), None)?,
                Source::Local(engine) => match local(engine)?.remove_archive(key)? {
                    Some(r) => r,
                    None => bail!(tr!("没有 {key} 的归档", "no archive for {key}")),
                },
            };
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!(
                    "{}",
                    tr!(
                        "删除了 {} 个归档，共 {}：{}",
                        "deleted {} archive(s), {}: {}",
                        r.removed.len(),
                        bytes(r.bytes),
                        r.removed.join(" ")
                    )
                );
            }
        }
    }
    Ok(())
}

fn print_plan(p: &CleanupPlan, to_stderr: bool) {
    let mut lines = vec![tr!("计划 {}（过期时间 {}）", "plan {} (expires {})", p.plan_id, date_time(p.expires_at))];
    for c in &p.sessions {
        let title = c.title.as_deref().unwrap_or("");
        if c.eligible {
            let kids = if c.children.is_empty() {
                String::new()
            } else {
                tr!(" +{} 个子代理", " +{} sub-agent(s)", c.children.len())
            };
            lines.push(tr!(
                "  可清理  {}  {} → 归档 ~{}{kids}  {title}",
                "  ok    {}  {} → archive ~{}{kids}  {title}",
                c.key,
                bytes(c.bytes),
                bytes(c.archive_bytes)
            ));
        } else {
            let why = c.message.as_deref().or(c.reason.as_deref()).unwrap_or("");
            lines.push(tr!("  跳过  {}  {why}", "  skip  {}  {why}", c.key));
        }
    }
    let n = p.sessions.iter().filter(|c| c.eligible).count();
    lines.push(tr!(
        "{} 个会话中有 {n} 个可清理：释放 {}，归档 ~{}",
        "{n} of {} session(s) can be cleaned: frees {}, archive ~{}",
        p.sessions.len(),
        bytes(p.freed_bytes),
        bytes(p.archive_bytes)
    ));
    for l in lines {
        if to_stderr {
            eprintln!("{l}");
        } else {
            println!("{l}");
        }
    }
}

fn print_report(r: &CleanupReport) {
    for x in &r.results {
        let status = match x.status {
            CleanupStatus::Archived => tr!("已归档", "archived"),
            CleanupStatus::Failed => tr!("失败", "FAILED"),
            CleanupStatus::Skipped => tr!("已跳过", "skipped"),
        };
        let why = x.message.as_deref().or(x.reason.as_deref()).map(|m| format!("  {m}")).unwrap_or_default();
        println!(
            "{}",
            tr!(
                "  {status:<8} {}  释放 {}  归档 {}{why}",
                "  {status:<8} {}  freed {}  archive {}{why}",
                x.key,
                bytes(x.freed_bytes),
                bytes(x.archive_bytes)
            )
        );
    }
    println!(
        "{}",
        tr!(
            "释放 {} 到回收站，归档 {}",
            "freed {} into the trash, archives {}",
            bytes(r.freed_bytes),
            bytes(r.archive_bytes)
        )
    );
}

fn bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", n as f64 / 1_073_741_824.0),
    }
}
