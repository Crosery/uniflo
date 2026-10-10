//! `uniflo setup`: connect the agent harnesses on this machine to Uniflo — register the MCP
//! server (`uniflo mcp`) and/or install the Skill — and undo it again.
//!
//! Only harness *configuration* is written (ADR-0007): the `uniflo` entry of an MCP config
//! (through the harness's own `mcp add` where it has one), a `uniflo` symlink in a skills
//! directory, and optionally one Claude Code SessionStart hook. Files are backed up before they
//! change and replaced atomically; entries Uniflo did not write are never touched. What was done
//! is recorded in `<config dir>/setup.json`, which `--uninstall` and `--reload` follow.
//!
//! Nothing is written without a confirmation: in a terminal the user confirms the plan, elsewhere
//! only `--yes` does; otherwise the plan is printed and nothing changes.

mod edit;
mod harness;

use anyhow::{Context, Result, bail};
use clap::Args;
use edit::Outcome;
use harness::{Detected, Env, Mcp, Probe, Skill, Spec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Args, Default)]
pub struct SetupArgs {
    /// Register the MCP server (the default).
    #[arg(long, group = "how")]
    pub mcp: bool,
    /// Install the Skill.
    #[arg(long, group = "how")]
    pub skill: bool,
    /// MCP server and Skill.
    #[arg(long, group = "how")]
    pub both: bool,
    /// Connect nothing (remembered: no automatic prompt afterwards).
    #[arg(long, group = "how")]
    pub none: bool,
    /// Only these harnesses, comma-separated ids (`claude,codex`); default: every one detected.
    #[arg(long, value_delimiter = ',')]
    pub agents: Vec<String>,
    /// Also add a Claude Code SessionStart hook that prints `uniflo context` into new sessions.
    #[arg(long)]
    pub hook: bool,
    /// Apply without asking (outside a terminal nothing is written without it).
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Only print the detected harnesses and the planned changes.
    #[arg(long)]
    pub dry_run: bool,
    /// Undo what setup recorded: remove `uniflo` entries and links, keep backups.
    #[arg(long, conflicts_with_all = ["mcp", "skill", "both", "none", "reload", "hook"])]
    pub uninstall: bool,
    /// Point recorded entries at this executable and connect newly detected harnesses.
    #[arg(long)]
    pub reload: bool,
    /// The step `uniflo update` runs after installing (TTY only).
    #[arg(long, hide = true)]
    pub after_update: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Mcp,
    Skill,
    Both,
    None,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mcp" => Some(Mode::Mcp),
            "skill" => Some(Mode::Skill),
            "both" => Some(Mode::Both),
            "none" | "skip" => Some(Mode::None),
            _ => None,
        }
    }

    fn mcp(self) -> bool {
        matches!(self, Mode::Mcp | Mode::Both)
    }

    fn skill(self) -> bool {
        matches!(self, Mode::Skill | Mode::Both)
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Mcp => "MCP",
            Mode::Skill => "Skill",
            Mode::Both => "MCP + Skill",
            Mode::None => "跳过",
        }
    }
}

/// `setup.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    /// The user answered a setup question (a refusal included): no automatic prompt again.
    #[serde(default)]
    pub asked: bool,
    /// Last applied mode; `None` = never configured.
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(default)]
    pub harnesses: BTreeMap<String, Rec>,
    /// The Skill file written (`~/.agents/skills/uniflo/SKILL.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<FileRec>,
    /// Every file setup has written, symlink it created and backup it made.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub backups: Vec<String>,
    #[serde(default)]
    pub updated_at: i64,
}

/// Outcome for one harness.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Rec {
    /// `connected`, `unchanged`, `skipped`, `occupied`, `failed` or `declined`.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpRec>,
    /// The `uniflo` symlink in its skills directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum McpRec {
    /// Registered through the harness's own CLI.
    Cli,
    /// Uniflo edited this JSON (`created`: containers / the file it added).
    Json {
        path: String,
        key: String,
        #[serde(default)]
        created: Vec<String>,
    },
    Toml {
        path: String,
        #[serde(default)]
        created: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRec {
    pub path: String,
    #[serde(default)]
    pub created: Vec<String>,
}

impl State {
    fn load(p: &Path) -> Option<State> {
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    fn save(&mut self, p: &Path) -> Result<()> {
        self.version = 1;
        self.updated_at = uniflo_core::util::now_ms();
        dedup(&mut self.files);
        dedup(&mut self.links);
        dedup(&mut self.backups);
        std::fs::create_dir_all(p.parent().context("setup.json has no parent")?)?;
        std::fs::write(p, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", p.display()))
    }

    fn connected(&self) -> impl Iterator<Item = &str> {
        self.harnesses
            .iter()
            .filter(|(_, r)| r.mcp.is_some() || r.link.is_some() || r.result == "connected")
            .map(|(k, _)| k.as_str())
    }
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

/// Everything the flow reads from the machine, so tests can point it at a temporary home.
pub struct Ctx {
    pub env: Env,
    /// The uniflo executable that MCP entries and links point at.
    pub bin: String,
    pub state_path: PathBuf,
    /// Suffix of backups made in this run.
    pub stamp: String,
    pub var: fn(&str) -> Option<String>,
}

impl Ctx {
    pub fn from_process() -> Result<Ctx> {
        let exe = std::env::current_exe().context("locate the uniflo executable")?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        Ok(Ctx {
            env: Env::from_process(),
            bin: exe.display().to_string(),
            state_path: uniflo_core::paths::config_dir().join("setup.json"),
            stamp: chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
            var: |k| std::env::var(k).ok().filter(|v| !v.is_empty()),
        })
    }

    fn skill_dir(&self) -> PathBuf {
        self.env.home.join(".agents/skills/uniflo")
    }
}

fn tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Automatic prompts are for people at a terminal only, never CI, never when switched off.
fn prompts_allowed(tty: bool, var: fn(&str) -> Option<String>) -> bool {
    tty && var("CI").is_none() && var("UNIFLO_NO_SETUP").is_none_or(|v| v == "0")
}

pub fn run(a: &SetupArgs) -> Result<()> {
    let ctx = Ctx::from_process()?;
    let mut input = std::io::stdin().lock();
    flow(&ctx, a, tty(), &mut input, &mut std::io::stdout())
}

/// Ask once on the first interactive run of any command (not `daemon`, `mcp`, `setup`…).
pub fn first_run() {
    let Ok(ctx) = Ctx::from_process() else { return };
    let state = State::load(&ctx.state_path);
    if !should_ask_first_run(tty(), ctx.var, state.as_ref()) {
        return;
    }
    let mut input = std::io::stdin().lock();
    let mut out = std::io::stdout();
    let _ = writeln!(
        out,
        "uniflo 可以让本机的 agent（Claude Code、Codex 等）直接查询所有会话：注册 MCP 服务器和/或安装 Skill。"
    );
    let yes = ask_yes(&mut input, &mut out, "现在设置吗？选 n 后不再询问，之后可随时运行 `uniflo setup`", true);
    let res = if yes {
        flow(&ctx, &SetupArgs::default(), true, &mut input, &mut out)
    } else {
        let mut s = state.unwrap_or_default();
        s.asked = true;
        s.save(&ctx.state_path)
    };
    if let Err(e) = res {
        eprintln!("uniflo setup: {e:#}");
    }
    let _ = writeln!(out);
}

pub fn should_ask_first_run(tty: bool, var: fn(&str) -> Option<String>, state: Option<&State>) -> bool {
    prompts_allowed(tty, var) && state.is_none_or(|s| !s.asked && s.mode.is_none())
}

/// End of a successful `uniflo update`: let the newly installed binary at `exe` run the setup
/// step (`current_exe()` of a replaced executable is no longer valid on Linux).
pub fn after_update(exe: &Path) {
    if !prompts_allowed(tty(), |k| std::env::var(k).ok().filter(|v| !v.is_empty())) {
        return;
    }
    let _ = Command::new(exe).args(["setup", "--after-update"]).status();
}

#[derive(Debug, PartialEq)]
pub enum AfterUpdate {
    /// Never configured: the full interactive setup, once.
    Ask,
    /// Configured: refresh silently; ask only about these newly detected harnesses.
    Reload { new: Vec<&'static str> },
    /// Declined before.
    Nothing,
}

pub fn after_update_decision(state: Option<&State>, detected: &[&'static str]) -> AfterUpdate {
    let Some(s) = state else { return AfterUpdate::Ask };
    match s.mode {
        None if !s.asked => AfterUpdate::Ask,
        None | Some(Mode::None) => AfterUpdate::Nothing,
        Some(_) => {
            AfterUpdate::Reload { new: detected.iter().copied().filter(|id| !s.harnesses.contains_key(*id)).collect() }
        }
    }
}

/// One harness's planned work.
struct Item {
    spec: &'static Spec,
    steps: Vec<Step>,
}

enum Step {
    /// Run a harness CLI; `first` runs before it (removing an entry that points elsewhere).
    Run {
        argv: Vec<String>,
        first: Option<Vec<String>>,
    },
    Json {
        path: PathBuf,
        key: &'static str,
        want: serde_json::Value,
    },
    Toml {
        path: PathBuf,
    },
    Link {
        link: PathBuf,
        target: PathBuf,
    },
    /// The harness reads `~/.agents/skills` itself.
    Shared,
    RunRemove {
        argv: Vec<String>,
    },
    JsonRemove {
        path: PathBuf,
        key: String,
        created: Vec<String>,
    },
    TomlRemove {
        path: PathBuf,
        created: Vec<String>,
    },
    Unlink {
        link: PathBuf,
    },
    Note(Note, String),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Note {
    Unchanged,
    Skipped,
    Occupied,
}

struct Plan {
    mode: Mode,
    items: Vec<Item>,
    /// Write SKILL.md here first (some harness gets the Skill).
    skill: Option<PathBuf>,
    hook: Option<(PathBuf, String)>,
    /// Uninstall: remove these.
    unskill: Option<PathBuf>,
    unhook: Option<FileRec>,
    undo: bool,
}

/// The whole command; `tty` decides whether questions are asked.
fn flow(ctx: &Ctx, a: &SetupArgs, tty: bool, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut state = State::load(&ctx.state_path);
    if a.uninstall {
        let Some(st) = state.as_mut().filter(|s| !s.harnesses.is_empty() || s.skill.is_some() || s.hook.is_some())
        else {
            writeln!(out, "uniflo setup：没有可撤销的记录（{}）。", ctx.state_path.display())?;
            return Ok(());
        };
        let plan = uninstall_plan(ctx, st);
        print_plan(ctx, &plan, out, "撤销")?;
        if !confirmed(a, tty, input, out)? {
            return Ok(());
        }
        apply(ctx, &plan, st, out)?;
        st.harnesses.clear();
        st.skill = None;
        st.hook = None;
        st.mode = None;
        st.asked = true;
        st.save(&ctx.state_path)?;
        writeln!(out, "已撤销；备份文件保留。记录：{}", ctx.state_path.display())?;
        return Ok(());
    }
    let detected = harness::detect(&ctx.env);
    let interactive = tty && !a.yes && !a.dry_run;

    if a.after_update {
        let ids: Vec<&'static str> = detected.iter().map(|d| d.spec.id).collect();
        match after_update_decision(state.as_ref(), &ids) {
            AfterUpdate::Nothing => return Ok(()),
            AfterUpdate::Ask => return flow(ctx, &SetupArgs::default(), tty, input, out),
            AfterUpdate::Reload { new } => {
                let st = state.as_mut().context("state")?;
                let mode = st.mode.unwrap_or(Mode::Mcp);
                let known: Vec<&Detected> =
                    detected.iter().filter(|d| st.connected().any(|id| id == d.spec.id)).collect();
                let hook = st.hook.is_some();
                let refresh = plan(ctx, mode, &known, hook);
                apply(ctx, &refresh, st, &mut std::io::sink())?;
                st.save(&ctx.state_path)?;
                if new.is_empty() || !tty {
                    return Ok(());
                }
                let names: Vec<&str> = new.iter().filter_map(|id| harness::lookup(id)).map(|s| s.name).collect();
                let q = format!("检测到新的 harness：{}。为它们接入 {}？", names.join("、"), mode.label());
                let fresh: Vec<&Detected> = detected.iter().filter(|d| new.contains(&d.spec.id)).collect();
                if ask_yes(input, out, &q, true) {
                    let add = plan(ctx, mode, &fresh, false);
                    print_plan(ctx, &add, out, "接入")?;
                    apply(ctx, &add, st, out)?;
                } else {
                    for d in fresh {
                        st.harnesses.insert(d.spec.id.into(), Rec { result: "declined".into(), ..Default::default() });
                    }
                }
                return st.save(&ctx.state_path);
            }
        }
    }

    let env_mode = (ctx.var)("UNIFLO_SETUP").and_then(|v| Mode::parse(&v));
    let flag_mode = [(a.mcp, Mode::Mcp), (a.skill, Mode::Skill), (a.both, Mode::Both), (a.none, Mode::None)]
        .into_iter()
        .find_map(|(on, m)| on.then_some(m))
        .or(env_mode);
    let recorded = state.as_ref().and_then(|s| s.mode);
    if a.reload && flag_mode.is_none() && recorded.is_none() {
        writeln!(out, "uniflo setup --reload：还没有配置过，先运行 `uniflo setup`。")?;
        return Ok(());
    }
    let mode = match flag_mode.or(if a.reload { recorded } else { None }) {
        Some(m) => m,
        None if interactive => ask_mode(input, out)?,
        None => Mode::Mcp,
    };
    if mode == Mode::None {
        writeln!(out, "uniflo setup：跳过，不接入任何 harness。之后可随时运行 `uniflo setup`。")?;
        if a.yes || interactive {
            let mut s = state.unwrap_or_default();
            s.asked = true;
            s.save(&ctx.state_path)?;
        }
        return Ok(());
    }

    let mut picked: Vec<&Detected> = if !a.agents.is_empty() {
        let mut v = Vec::new();
        for id in &a.agents {
            let Some(spec) = harness::lookup(id) else {
                bail!(
                    "unknown harness `{id}`; known: {}",
                    harness::SPECS.iter().map(|s| s.id).collect::<Vec<_>>().join(", ")
                )
            };
            match detected.iter().find(|d| d.spec.id == spec.id) {
                Some(d) => v.push(d),
                None => writeln!(
                    out,
                    "  {:<10} 未检测到（PATH 中没有 {}，也没有 ~/{}），跳过",
                    spec.id, spec.bins[0], spec.dirs[0]
                )?,
            }
        }
        v
    } else if a.reload {
        let st = state.as_ref();
        detected
            .iter()
            .filter(|d| st.and_then(|s| s.harnesses.get(d.spec.id)).is_none_or(|r| r.result != "declined"))
            .collect()
    } else if interactive {
        ask_agents(input, out, &detected)?
    } else {
        detected.iter().collect()
    };
    let mut seen = std::collections::HashSet::new();
    picked.retain(|d| seen.insert(d.spec.id));
    if picked.is_empty() {
        writeln!(out, "uniflo setup：没有可接入的 harness。")?;
        return Ok(());
    }
    let claude = picked.iter().any(|d| d.spec.id == "claude");
    let hook = claude
        && (a.hook
            || (a.reload && state.as_ref().is_some_and(|s| s.hook.is_some()))
            || (interactive
                && ask_yes(
                    input,
                    out,
                    "为 Claude Code 安装 SessionStart hook（新会话开头附上本项目最近的会话列表）？",
                    false,
                )));
    let todo = plan(ctx, mode, &picked, hook);
    print_plan(ctx, &todo, out, "接入")?;
    if !confirmed(a, tty, input, out)? {
        return Ok(());
    }
    let mut st = state.unwrap_or_default();
    apply(ctx, &todo, &mut st, out)?;
    st.asked = true;
    st.mode = Some(mode);
    st.binary = Some(ctx.bin.clone());
    st.save(&ctx.state_path)?;
    writeln!(out, "完成。记录：{}；撤销：`uniflo setup --uninstall`。", ctx.state_path.display())?;
    Ok(())
}

/// Whether to write: `--yes`, or a confirmed prompt in a terminal. Prints why not otherwise.
fn confirmed(a: &SetupArgs, tty: bool, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<bool> {
    if a.dry_run {
        writeln!(out, "（预演：未写入任何文件）")?;
        return Ok(false);
    }
    if a.yes {
        return Ok(true);
    }
    if !tty {
        writeln!(out, "（非交互环境：只打印计划，未写入任何文件；确认无误后加 --yes 执行）")?;
        return Ok(false);
    }
    let yes = ask_yes(input, out, "确认执行？", true);
    if !yes {
        writeln!(out, "已取消，未写入任何文件。")?;
    }
    Ok(yes)
}

fn read_answer(input: &mut dyn BufRead) -> Option<String> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_owned()),
    }
}

/// Yes/no with a default on Enter; end of input counts as "no".
fn ask_yes(input: &mut dyn BufRead, out: &mut dyn Write, q: &str, default: bool) -> bool {
    let _ = write!(out, "{q} {} ", if default { "[Y/n]" } else { "[y/N]" });
    let _ = out.flush();
    match read_answer(input) {
        None => false,
        Some(a) if a.is_empty() => default,
        Some(a) => matches!(a.to_ascii_lowercase().as_str(), "y" | "yes" | "是" | "好"),
    }
}

fn ask_mode(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<Mode> {
    writeln!(out, "选择 agent 接入方式：")?;
    writeln!(out, "  1) MCP（默认）：agent 直接调用 uniflo 工具查会话、检索、用量")?;
    writeln!(out, "  2) Skill：agent 按说明使用 uniflo 命令行 / REST")?;
    writeln!(out, "  3) 两者")?;
    writeln!(out, "  4) 跳过")?;
    loop {
        write!(out, "请选择 [1]: ")?;
        out.flush()?;
        let Some(a) = read_answer(input) else { return Ok(Mode::None) };
        match a.as_str() {
            "" | "1" => return Ok(Mode::Mcp),
            "2" => return Ok(Mode::Skill),
            "3" => return Ok(Mode::Both),
            "4" => return Ok(Mode::None),
            other => match Mode::parse(other) {
                Some(m) => return Ok(m),
                None => writeln!(out, "请输入 1–4。")?,
            },
        }
    }
}

fn ask_agents<'a>(input: &mut dyn BufRead, out: &mut dyn Write, detected: &'a [Detected]) -> Result<Vec<&'a Detected>> {
    if detected.is_empty() {
        return Ok(Vec::new());
    }
    writeln!(out, "检测到 {} 个 harness（默认全部接入）：", detected.len())?;
    for (i, d) in detected.iter().enumerate() {
        writeln!(out, "  {:>2}) {:<10} {}", i + 1, d.spec.id, d.spec.name)?;
    }
    loop {
        write!(out, "输入要接入的编号或 id（逗号分隔），回车为全部: ")?;
        out.flush()?;
        let Some(a) = read_answer(input) else { return Ok(Vec::new()) };
        if a.is_empty() {
            return Ok(detected.iter().collect());
        }
        let mut v = Vec::new();
        let mut bad = None;
        for w in a.split([',', ' ', '，']).filter(|w| !w.is_empty()) {
            let hit = w
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| detected.get(i))
                .or_else(|| harness::lookup(w).and_then(|s| detected.iter().find(|d| d.spec.id == s.id)));
            match hit {
                Some(d) => v.push(d),
                None => bad = Some(w.to_owned()),
            }
        }
        match bad {
            Some(w) => writeln!(out, "不认识 `{w}`，请重新输入。")?,
            None => return Ok(v),
        }
    }
}

/// What connecting `picked` in `mode` takes. Reads (never writes) the files involved.
fn plan(ctx: &Ctx, mode: Mode, picked: &[&Detected], hook: bool) -> Plan {
    let shared = ctx.skill_dir();
    let mut items = Vec::new();
    let mut needs_skill = false;
    for d in picked {
        let spec = d.spec;
        let mut steps = Vec::new();
        if mode.mcp() {
            steps.extend(mcp_steps(ctx, d));
        }
        if mode.skill() || (mode.mcp() && matches!(spec.mcp, Mcp::None)) {
            needs_skill = true;
            steps.push(match spec.skill {
                Skill::Shared => Step::Shared,
                Skill::Link(dir) => {
                    let link = ctx.env.file(dir).join("uniflo");
                    let target = relative(link.parent().unwrap_or(Path::new("/")), &shared);
                    match link_state(&link, &target, &shared) {
                        LinkState::Missing => Step::Link { link, target },
                        LinkState::Ours => {
                            Step::Note(Note::Unchanged, format!("Skill 软链已存在：{}", ctx.env.show(&link)))
                        }
                        LinkState::Other => Step::Note(
                            Note::Occupied,
                            format!("{} 已被占用（不是指向 Uniflo Skill 的软链），未改动", ctx.env.show(&link)),
                        ),
                    }
                }
            });
        }
        items.push(Item { spec, steps });
    }
    let skill_file = shared.join("SKILL.md");
    let skill = (needs_skill && std::fs::read_to_string(&skill_file).ok().as_deref() != Some(crate::agent::SKILL))
        .then_some(skill_file);
    let hook = hook.then(|| (ctx.env.file(".claude/settings.json"), edit::hook_command(&ctx.bin)));
    Plan { mode, items, skill, hook, unskill: None, unhook: None, undo: false }
}

fn mcp_steps(ctx: &Ctx, d: &Detected) -> Vec<Step> {
    let bin = ctx.bin.as_str();
    match &d.spec.mcp {
        Mcp::None => vec![Step::Note(Note::Skipped, format!("{} 没有可用的 MCP 客户端，改装 Skill", d.spec.name))],
        Mcp::File { file, key, shape } => {
            let path = ctx.env.file(file);
            let want = edit::entry(*shape, bin);
            let text = std::fs::read_to_string(&path).ok();
            match edit::json_set(text.as_deref(), key, &want) {
                Outcome::Write { .. } => vec![Step::Json { path, key, want }],
                o => vec![note_for(ctx, &path, &format!("{key}.uniflo"), o)],
            }
        }
        Mcp::Cli { add, remove, probe, toml_fallback } => {
            let (probe_path, found) = probe_cli(ctx, probe);
            let Some(cli) = &d.cli else {
                if *toml_fallback {
                    let text = std::fs::read_to_string(&probe_path).ok();
                    return match edit::toml_set(text.as_deref(), bin) {
                        Outcome::Write { .. } => vec![Step::Toml { path: probe_path }],
                        o => vec![note_for(ctx, &probe_path, "mcp_servers.uniflo", o)],
                    };
                }
                return vec![Step::Note(
                    Note::Skipped,
                    format!("PATH 中没有 {}，无法用它的 mcp add 注册", d.spec.bins[0]),
                )];
            };
            let argv = |tpl: &[&str]| -> Vec<String> {
                std::iter::once(cli.display().to_string())
                    .chain(tpl.iter().map(|w| if *w == "{bin}" { bin.to_owned() } else { (*w).to_owned() }))
                    .collect()
            };
            match found {
                Some(Ok(Some(p))) if p == bin => {
                    vec![Step::Note(Note::Unchanged, format!("{} 中已有 uniflo，未变化", ctx.env.show(&probe_path)))]
                }
                Some(Ok(Some(_))) => vec![Step::Run { argv: argv(add), first: Some(argv(remove)) }],
                Some(Err(())) => vec![Step::Note(
                    Note::Occupied,
                    format!("{} 中的 uniflo 条目不是 Uniflo 写入的，未覆盖", ctx.env.show(&probe_path)),
                )],
                Some(Ok(None)) | None => vec![Step::Run { argv: argv(add), first: None }],
            }
        }
    }
}

/// The file a harness's `mcp add` writes, and what its `uniflo` entry is: `None` unreadable,
/// `Ok(None)` absent, `Ok(Some(program))` Uniflo's, `Err(())` someone else's.
fn probe_cli(ctx: &Ctx, probe: &Probe) -> (PathBuf, Option<Result<Option<String>, ()>>) {
    let (p, json) = match probe {
        Probe::Json(f) => (ctx.env.file(f), true),
        Probe::Toml(f) => (ctx.env.file(f), false),
    };
    let text = std::fs::read_to_string(&p).ok();
    let found = text.and_then(|t| if json { edit::probe_json(&t) } else { edit::probe_toml(&t) });
    (p, found)
}

fn note_for(ctx: &Ctx, path: &Path, what: &str, o: Outcome) -> Step {
    match o {
        Outcome::Unchanged => Step::Note(Note::Unchanged, format!("{} 中的 {what} 已是最新", ctx.env.show(path))),
        Outcome::Occupied => Step::Note(
            Note::Occupied,
            format!("{} 中的 {what} 已被占用（不是 Uniflo 写入的），未覆盖", ctx.env.show(path)),
        ),
        Outcome::Invalid(e) => Step::Note(Note::Skipped, format!("{} 解析失败，跳过：{e}", ctx.env.show(path))),
        Outcome::Write { .. } => unreachable!("writes are steps"),
    }
}

fn uninstall_plan(ctx: &Ctx, st: &State) -> Plan {
    let detected = harness::detect(&ctx.env);
    let mut items = Vec::new();
    for (id, rec) in &st.harnesses {
        let Some(spec) = harness::lookup(id) else { continue };
        let mut steps = Vec::new();
        match &rec.mcp {
            Some(McpRec::Cli) => {
                let cli = detected.iter().find(|d| d.spec.id == spec.id).and_then(|d| d.cli.as_ref());
                match (&spec.mcp, cli) {
                    (Mcp::Cli { remove, probe, .. }, Some(cli)) => match probe_cli(ctx, probe) {
                        (_, Some(Ok(None))) => {}
                        (path, Some(Err(()))) => steps.push(Step::Note(
                            Note::Occupied,
                            format!("{} 中的 uniflo 已不是 Uniflo 写入的，保留", ctx.env.show(&path)),
                        )),
                        _ => steps.push(Step::RunRemove {
                            argv: std::iter::once(cli.display().to_string())
                                .chain(remove.iter().map(|w| (*w).to_owned()))
                                .collect(),
                        }),
                    },
                    _ => steps.push(Step::Note(
                        Note::Skipped,
                        format!("PATH 中没有 {}，请手动移除 uniflo 条目", spec.bins[0]),
                    )),
                }
            }
            Some(McpRec::Json { path, key, created }) => {
                steps.push(Step::JsonRemove { path: path.into(), key: key.clone(), created: created.clone() })
            }
            Some(McpRec::Toml { path, created }) => {
                steps.push(Step::TomlRemove { path: path.into(), created: created.clone() })
            }
            None => {}
        }
        if let Some(l) = &rec.link {
            steps.push(Step::Unlink { link: l.into() });
        }
        if !steps.is_empty() {
            items.push(Item { spec, steps });
        }
    }
    Plan {
        mode: Mode::None,
        items,
        skill: None,
        hook: None,
        unskill: st.skill.as_ref().map(PathBuf::from),
        unhook: st.hook.clone(),
        undo: true,
    }
}

fn print_plan(ctx: &Ctx, p: &Plan, out: &mut dyn Write, verb: &str) -> Result<()> {
    if verb == "接入" {
        writeln!(out, "uniflo setup：{verb}方式 {} · 可执行文件 {}", p.mode.label(), ctx.bin)?;
    } else {
        writeln!(out, "uniflo setup：{verb}")?;
    }
    if let Some(f) = &p.skill {
        writeln!(out, "  {:<10} 写入 Skill {}", "skill", ctx.env.show(f))?;
    }
    if let Some(f) = &p.unskill {
        writeln!(out, "  {:<10} 删除 Skill {}", "skill", ctx.env.show(Path::new(f)))?;
    }
    for it in &p.items {
        for (i, s) in it.steps.iter().enumerate() {
            let id = if i == 0 { it.spec.id } else { "" };
            writeln!(out, "  {id:<10} {}", describe(ctx, s))?;
        }
    }
    if let Some((f, cmd)) = &p.hook {
        writeln!(out, "  {:<10} 编辑 {}：hooks.SessionStart 追加 `{cmd}`（改前备份）", "hook", ctx.env.show(f))?;
    }
    if let Some(h) = &p.unhook {
        writeln!(
            out,
            "  {:<10} 编辑 {}：移除 uniflo 的 SessionStart hook（改前备份）",
            "hook",
            ctx.env.show(Path::new(&h.path))
        )?;
    }
    Ok(())
}

fn describe(ctx: &Ctx, s: &Step) -> String {
    let cmd = |argv: &[String]| {
        let mut w: Vec<String> = argv.iter().map(|a| uniflo_core::resume::sh_quote(a)).collect();
        if let Some(name) = Path::new(&argv[0]).file_name() {
            w[0] = name.to_string_lossy().into_owned();
        }
        w.join(" ")
    };
    match s {
        Step::Run { argv, first: None } => format!("运行 {}", cmd(argv)),
        Step::Run { argv, first: Some(f) } => format!("刷新路径：运行 {}，再运行 {}", cmd(f), cmd(argv)),
        Step::Json { path, key, .. } => format!("编辑 {}：写入 {key}.uniflo（改前备份）", ctx.env.show(path)),
        Step::Toml { path } => format!("编辑 {}：写入 [mcp_servers.uniflo]（改前备份）", ctx.env.show(path)),
        Step::Link { link, target } => format!("Skill 软链 {} → {}", ctx.env.show(link), target.display()),
        Step::Shared => "Skill：读取 ~/.agents/skills，无需软链".to_owned(),
        Step::RunRemove { argv } => format!("运行 {}", cmd(argv)),
        Step::JsonRemove { path, key, .. } => format!("编辑 {}：移除 {key}.uniflo（改前备份）", ctx.env.show(path)),
        Step::TomlRemove { path, .. } => format!("编辑 {}：移除 [mcp_servers.uniflo]（改前备份）", ctx.env.show(path)),
        Step::Unlink { link } => format!("删除软链 {}（仅当它指向 Uniflo Skill）", ctx.env.show(link)),
        Step::Note(n, text) => format!(
            "{}：{text}",
            match n {
                Note::Unchanged => "未变化",
                Note::Skipped => "跳过",
                Note::Occupied => "已占用",
            }
        ),
    }
}

/// Carry out a plan, recording into `st` and printing one line per harness.
fn apply(ctx: &Ctx, p: &Plan, st: &mut State, out: &mut dyn Write) -> Result<()> {
    if let Some(f) = &p.skill {
        std::fs::create_dir_all(f.parent().context("skill dir")?)?;
        std::fs::write(f, crate::agent::SKILL).with_context(|| format!("write {}", f.display()))?;
    }
    if p.items.iter().any(|it| it.steps.iter().any(|s| matches!(s, Step::Link { .. } | Step::Shared))) {
        let f = ctx.skill_dir().join("SKILL.md");
        st.skill = Some(f.display().to_string());
        st.files.push(f.display().to_string());
    }
    for it in &p.items {
        let rec = st.harnesses.entry(it.spec.id.to_owned()).or_default();
        let mut worst = 0u8;
        let mut msgs: Vec<String> = Vec::new();
        let mut bump = |rank: u8, msg: String| {
            worst = worst.max(rank);
            msgs.push(msg);
        };
        for s in &it.steps {
            match exec(ctx, s, rec, &mut st.files, &mut st.links, &mut st.backups) {
                Ok(Some(done)) => bump(1, done),
                Ok(None) => {}
                Err(StepErr::Note(Note::Unchanged, m)) => bump(0, m),
                Err(StepErr::Note(Note::Skipped, m)) => bump(2, m),
                Err(StepErr::Note(Note::Occupied, m)) => bump(3, m),
                Err(StepErr::Failed(m)) => bump(4, m),
            }
        }
        let (rank, msg) = (worst, msgs.join("；"));
        let done = if p.undo { "removed" } else { "connected" };
        rec.result = ["unchanged", done, "skipped", "occupied", "failed"][rank as usize].to_owned();
        rec.detail = (!msg.is_empty()).then(|| msg.clone());
        writeln!(out, "  {:<10} {:<9} {msg}", it.spec.id, rec.result)?;
    }
    if let Some((path, cmd)) = &p.hook {
        let text = std::fs::read_to_string(path).ok();
        match edit::hook_set(text.as_deref(), cmd) {
            Outcome::Write { text, created } => {
                let b = edit::write(path, &text, &ctx.stamp)?;
                st.backups.extend(b.map(|b| b.display().to_string()));
                st.files.push(path.display().to_string());
                st.hook = Some(FileRec { path: path.display().to_string(), created });
                writeln!(out, "  {:<10} {:<9} {}", "hook", "connected", ctx.env.show(path))?;
            }
            Outcome::Unchanged => {
                st.hook.get_or_insert_with(|| FileRec { path: path.display().to_string(), created: Vec::new() });
            }
            o => writeln!(out, "  {:<10} {:<9} {:?}", "hook", "skipped", o)?,
        }
    }
    if let Some(h) = &p.unhook {
        let path = PathBuf::from(&h.path);
        let text = std::fs::read_to_string(&path).ok();
        if let Outcome::Write { text, .. } = edit::hook_remove(text.as_deref(), &h.created) {
            let b =
                if text.is_empty() { edit::delete(&path, &ctx.stamp)? } else { edit::write(&path, &text, &ctx.stamp)? };
            st.backups.extend(b.map(|b| b.display().to_string()));
            writeln!(out, "  {:<10} {:<9} {}", "hook", "removed", ctx.env.show(&path))?;
        }
    }
    if let Some(f) = &p.unskill {
        if f.is_file() {
            std::fs::remove_file(f)?;
        }
        if let Some(d) = f.parent() {
            let _ = std::fs::remove_dir(d);
        }
    }
    Ok(())
}

enum StepErr {
    Note(Note, String),
    Failed(String),
}

/// One step; `Ok(Some(msg))` when it changed something.
fn exec(
    ctx: &Ctx,
    s: &Step,
    rec: &mut Rec,
    files: &mut Vec<String>,
    links: &mut Vec<String>,
    backups: &mut Vec<String>,
) -> Result<Option<String>, StepErr> {
    let failed = |e: anyhow::Error| StepErr::Failed(format!("{e:#}"));
    match s {
        Step::Note(n, m) => Err(StepErr::Note(*n, m.clone())),
        Step::Run { argv, first } => {
            if let Some(f) = first {
                run_cli(ctx, f).map_err(StepErr::Failed)?;
            }
            run_cli(ctx, argv).map_err(StepErr::Failed)?;
            rec.mcp = Some(McpRec::Cli);
            Ok(Some(format!(
                "已用 {} mcp add 注册",
                Path::new(&argv[0]).file_name().unwrap_or_default().to_string_lossy()
            )))
        }
        Step::RunRemove { argv } => {
            run_cli(ctx, argv).map_err(StepErr::Failed)?;
            rec.mcp = None;
            Ok(Some("已移除 uniflo".into()))
        }
        Step::Json { path, key, want } => {
            let text = std::fs::read_to_string(path).ok();
            match edit::json_set(text.as_deref(), key, want) {
                Outcome::Write { text, created } => {
                    let b = edit::write(path, &text, &ctx.stamp).map_err(failed)?;
                    backups.extend(b.map(|b| b.display().to_string()));
                    files.push(path.display().to_string());
                    let created = match &rec.mcp {
                        Some(McpRec::Json { created: old, .. }) if created.is_empty() => old.clone(),
                        _ => created,
                    };
                    rec.mcp = Some(McpRec::Json { path: path.display().to_string(), key: (*key).to_owned(), created });
                    Ok(Some(format!("已写入 {}", ctx.env.show(path))))
                }
                o => match note_for(ctx, path, &format!("{key}.uniflo"), o) {
                    Step::Note(n, m) => Err(StepErr::Note(n, m)),
                    _ => Ok(None),
                },
            }
        }
        Step::Toml { path } => {
            let text = std::fs::read_to_string(path).ok();
            match edit::toml_set(text.as_deref(), &ctx.bin) {
                Outcome::Write { text, created } => {
                    let b = edit::write(path, &text, &ctx.stamp).map_err(failed)?;
                    backups.extend(b.map(|b| b.display().to_string()));
                    files.push(path.display().to_string());
                    rec.mcp = Some(McpRec::Toml { path: path.display().to_string(), created });
                    Ok(Some(format!("已写入 {}", ctx.env.show(path))))
                }
                o => match note_for(ctx, path, "mcp_servers.uniflo", o) {
                    Step::Note(n, m) => Err(StepErr::Note(n, m)),
                    _ => Ok(None),
                },
            }
        }
        Step::JsonRemove { path, key, created } => {
            remove_file_entry(ctx, path, created, backups, rec, |t, c| edit::json_remove(t, key, c))
        }
        Step::TomlRemove { path, created } => remove_file_entry(ctx, path, created, backups, rec, edit::toml_remove),
        Step::Link { link, target } => {
            std::fs::create_dir_all(link.parent().unwrap_or(Path::new("."))).map_err(|e| failed(e.into()))?;
            symlink(target, link)
                .map_err(|e| failed(anyhow::Error::from(e).context(format!("link {}", link.display()))))?;
            links.push(link.display().to_string());
            rec.link = Some(link.display().to_string());
            Ok(Some(format!("Skill 软链 {}", ctx.env.show(link))))
        }
        Step::Shared => Ok(Some("Skill 在 ~/.agents/skills".into())),
        Step::Unlink { link } => {
            let shared = ctx.skill_dir();
            let target = relative(link.parent().unwrap_or(Path::new("/")), &shared);
            match link_state(link, &target, &shared) {
                LinkState::Ours => {
                    std::fs::remove_file(link).map_err(|e| failed(e.into()))?;
                    rec.link = None;
                    Ok(Some(format!("已删除软链 {}", ctx.env.show(link))))
                }
                LinkState::Missing => {
                    rec.link = None;
                    Ok(None)
                }
                LinkState::Other => {
                    Err(StepErr::Note(Note::Occupied, format!("{} 不再指向 Uniflo Skill，保留", ctx.env.show(link))))
                }
            }
        }
    }
}

fn remove_file_entry(
    ctx: &Ctx,
    path: &Path,
    created: &[String],
    backups: &mut Vec<String>,
    rec: &mut Rec,
    f: impl Fn(Option<&str>, &[String]) -> Outcome,
) -> Result<Option<String>, StepErr> {
    let text = std::fs::read_to_string(path).ok();
    match f(text.as_deref(), created) {
        Outcome::Write { text, .. } => {
            let b = if text.is_empty() { edit::delete(path, &ctx.stamp) } else { edit::write(path, &text, &ctx.stamp) };
            let b = b.map_err(|e| StepErr::Failed(format!("{e:#}")))?;
            backups.extend(b.map(|b| b.display().to_string()));
            rec.mcp = None;
            Ok(Some(format!("已从 {} 移除 uniflo", ctx.env.show(path))))
        }
        Outcome::Unchanged => {
            rec.mcp = None;
            Ok(None)
        }
        Outcome::Occupied => {
            Err(StepErr::Note(Note::Occupied, format!("{} 中的 uniflo 不是 Uniflo 写入的，保留", ctx.env.show(path))))
        }
        Outcome::Invalid(e) => {
            Err(StepErr::Note(Note::Skipped, format!("{} 解析失败，未改动：{e}", ctx.env.show(path))))
        }
    }
}

/// Run a harness CLI without a terminal, at most a minute. In a sandbox (`UNIFLO_HOME`) it sees
/// that directory as `HOME` and none of its own config-dir overrides.
fn run_cli(ctx: &Ctx, argv: &[String]) -> Result<(), String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if ctx.env.sandboxed {
        cmd.env("HOME", &ctx.env.home).env_remove("CLAUDE_CONFIG_DIR").env_remove("CODEX_HOME");
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", argv[0]))?;
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if t0.elapsed() > Duration::from_secs(60) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} timed out", argv[0]));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    };
    if status.success() {
        return Ok(());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    let last = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_owned();
    Err(format!("exit {}: {last}", status.code().unwrap_or(-1)))
}

enum LinkState {
    Missing,
    /// A symlink to the Uniflo Skill directory.
    Ours,
    Other,
}

fn link_state(link: &Path, target: &Path, shared: &Path) -> LinkState {
    match std::fs::symlink_metadata(link) {
        Err(_) => LinkState::Missing,
        Ok(md) if md.file_type().is_symlink() => {
            let same = std::fs::read_link(link).is_ok_and(|t| t == target)
                || std::fs::canonicalize(link)
                    .ok()
                    .zip(std::fs::canonicalize(shared).ok())
                    .is_some_and(|(a, b)| a == b);
            if same { LinkState::Ours } else { LinkState::Other }
        }
        Ok(_) => LinkState::Other,
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// `to` relative to the directory `from` (both absolute): `~/.claude/skills` → `../../.agents/skills/uniflo`.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let f: Vec<Component> = from.components().collect();
    let t: Vec<Component> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    out
}

#[cfg(test)]
mod tests;
