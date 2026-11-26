//! `uniflo update`: check crates.io, then upgrade the way this executable was installed
//! (ADR-0009): `cargo install` for a cargo install, the release package for a binary install,
//! only instructions for anything else. The newly installed executable then finishes the job
//! (`uniflo setup --after-update`): it restarts a daemon that still runs an older version and
//! refreshes the agent setup.

use crate::i18n::tr;
use crate::ui;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Instant;
use uniflo_core::UpdateInfo;
use uniflo_core::install::{self, InstallRecord, Method};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LAUNCHD_LABEL: &str = "com.crosery.uniflo";

/// Status marks of the step lines (legacy Windows console fonts lack the check mark).
#[cfg(not(windows))]
pub const MARKS: [&str; 4] = ["✓", "·", "!", "✗"];
#[cfg(windows)]
pub const MARKS: [&str; 4] = ["+", "-", "!", "x"];

#[derive(Clone, Copy)]
pub enum Mark {
    Ok,
    Info,
    Warn,
    Fail,
}

/// One indented step line: `  ✓ text`.
pub fn step(m: Mark, text: &str) -> String {
    let mark = MARKS[m as usize];
    let mark = match m {
        Mark::Ok => ui::good(mark),
        Mark::Info => ui::dim(mark),
        Mark::Warn => ui::warn(mark),
        Mark::Fail => ui::bad(mark),
    };
    format!("  {mark} {text}")
}

/// `uniflo update --check --json`: the check result plus how this executable was installed.
#[derive(Serialize)]
struct Report<'a> {
    #[serde(flatten)]
    info: &'a UpdateInfo,
    method: Method,
}

/// The default target is the newest **stable** release; a prerelease is only ever announced,
/// and installed solely via the explicit `--pre` opt-in.
pub fn run(check: bool, prerelease: bool, json: bool) -> Result<()> {
    let exe = std::env::current_exe().context("locate the uniflo executable")?;
    let exe = install::canon(&exe);
    let record_file = InstallRecord::file();
    let record = InstallRecord::load(&record_file);
    let method = install::detect(&exe, record.as_ref(), &install::cargo_bin_dirs());
    if !json && !check && std::io::stdout().is_terminal() {
        println!("{}", ui::banner(uniflo_core::update::current_version()));
    }
    let (info, hits) = if !check && !json {
        let smart = uniflo_core::update::check_smart();
        (smart.info, smart.hits)
    } else {
        (uniflo_core::update::check(), Vec::new())
    };
    let smart = uniflo_core::update::SmartCheck { info: info.clone(), hits };
    if json {
        println!("{}", serde_json::to_string_pretty(&Report { info: &info, method })?);
        if method == Method::Unknown {
            eprintln!("{}", manual_ways(&exe, None));
        }
        if info.available {
            std::process::exit(10);
        }
        return Ok(());
    }
    let cur = &info.current;
    if let Some(err) = &info.error {
        println!("{}", tr!("uniflo {cur}：无法检查更新 — {err}", "uniflo {cur}: cannot check for updates — {err}"));
        println!(
            "{}",
            step(
                Mark::Info,
                &tr!(
                    "可用 UNIFLO_MIRROR=cn（国内镜像优先）或 UNIFLO_MIRROR=official（官方优先）指定更新源后重试",
                    "retry with UNIFLO_MIRROR=cn (mainland China mirrors first) or UNIFLO_MIRROR=official (official sources first)"
                )
            )
        );
        return Ok(());
    }
    // Decide what (if anything) to install.
    let target: Option<(&str, bool)> = if prerelease {
        info.latest_prerelease.as_deref().map(|v| (v, true))
    } else {
        info.available.then(|| (info.latest.as_deref().unwrap_or("?"), false))
    };
    if prerelease && target.is_none() {
        println!(
            "{}",
            tr!("uniflo {cur}：crates.io 上没有比它更新的预发布版", "uniflo {cur}: no newer prerelease on crates.io")
        );
        return Ok(());
    }
    let head = match (&info.latest, target) {
        (_, Some((v, true))) => tr!(
            "uniflo {cur} → {v}（预发布版，按 --pre 选择，不保证稳定）",
            "uniflo {cur} → {v} (prerelease, chosen with --pre; not guaranteed stable)"
        ),
        (_, Some((v, _))) => format!("uniflo {cur} → {v}"),
        (Some(latest), None) if cur.contains('-') => {
            tr!(
                "uniflo {cur} 是预发布版；最新正式版 {latest}",
                "uniflo {cur} is a prerelease; latest stable is {latest}"
            )
        }
        (Some(_), None) => tr!("uniflo {cur} 已是最新版本", "uniflo {cur} is up to date"),
        (None, None) => tr!("uniflo {cur}：crates.io 上还没有发布版本", "uniflo {cur}: no release on crates.io yet"),
    };
    println!("{head}");
    if !prerelease && let Some(pre) = &info.latest_prerelease {
        println!(
            "{}",
            step(
                Mark::Info,
                &tr!(
                    "有预发布版 {pre}（不保证稳定）：`uniflo update --pre` 试用",
                    "prerelease {pre} available (not guaranteed stable): try it with `uniflo update --pre`"
                )
            )
        );
    }
    let Some((version, is_pre)) = target else { return Ok(()) };
    let record = match (method, record) {
        (Method::Unknown, _) => {
            println!("{}", manual_ways(&exe, Some(version)));
            return Ok(());
        }
        (Method::Binary, Some(r)) => Some(r),
        _ => None,
    };
    if check {
        let cmd = if is_pre { "uniflo update --pre" } else { "uniflo update" };
        println!("{}", step(Mark::Info, &tr!("安装：`{cmd}`", "install it with `{cmd}`")));
        return Ok(());
    }
    let t0 = Instant::now();
    let how = match record {
        Some(rec) => match binary_upgrade(&exe, rec, &record_file, version)? {
            Some(host) => tr!("发布包 · 经 {host}", "release package via {host}"),
            None => tr!("发布包", "release package"),
        },
        None => {
            let probes = smart.index_probes_for(version);
            cargo_install(&exe, version, &probes)?
        }
    };
    let secs = t0.elapsed().as_secs();
    println!(
        "{}",
        step(Mark::Ok, &tr!("已安装 {version}（{how}，{secs} 秒）", "installed {version} ({how}, {secs}s)"))
    );
    // `current_exe()` of a replaced executable is no longer valid on Linux: run the new one by path.
    let mut after = std::process::Command::new(&exe);
    after.args(["setup", "--after-update"]);
    if std::env::current_dir().is_err() {
        after.current_dir(safe_workdir());
    }
    let _ = after.status();
    Ok(())
}

/// One status line rewritten in place while a long step runs (only on a terminal).
struct Live {
    on: bool,
    width: usize,
}

impl Live {
    fn new() -> Live {
        Live { on: std::io::stderr().is_terminal(), width: 0 }
    }

    fn set(&mut self, text: &str) {
        if !self.on {
            return;
        }
        let line = format!("  {} {text}", MARKS[1]);
        let w = width(&line);
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{line}{}", " ".repeat(self.width.saturating_sub(w)));
        let _ = err.flush();
        self.width = w;
    }

    fn clear(&mut self) {
        if self.on && self.width > 0 {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{}\r", " ".repeat(self.width));
            let _ = err.flush();
            self.width = 0;
        }
    }
}

/// Terminal columns of `s` (CJK and full-width forms take two).
pub fn width(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F => 2,
            0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 => 2,
            _ => 1,
        })
        .sum()
}

/// Guaranteed-existing working directory for `cargo install` (and post-update recovery when the
/// caller's cwd was deleted or unmounted, e.g. a disconnected NAS mount). Running `cargo install`
/// here also avoids picking up an unrelated local project's `.cargo/config.toml`.
fn safe_workdir() -> std::path::PathBuf {
    let tmp = std::env::temp_dir();
    if tmp.is_dir() {
        return tmp;
    }
    uniflo_core::util::home()
}

fn cargo_install(
    #[cfg_attr(not(windows), allow(unused_variables))] exe: &Path,
    version: &str,
    probes: &[uniflo_core::mirror::IndexProbe],
) -> Result<String> {
    #[cfg(windows)]
    let backup = if exe.is_file() {
        install::remove_aside(exe);
        install::move_aside(exe).ok()
    } else {
        None
    };

    let res = run_cargo(version, probes);

    #[cfg(windows)]
    if let Some(backup) = backup {
        if res.is_ok() {
            install::remove_aside(exe);
        } else {
            if exe.is_file() {
                let _ = std::fs::remove_file(exe);
            }
            let _ = std::fs::rename(&backup, exe);
        }
    }
    res
}

fn source_label(s: &uniflo_core::mirror::CargoSource) -> String {
    if s.official { "crates.io".to_owned() } else { uniflo_core::mirror::host(&s.index_root).to_owned() }
}

fn source_hint(s: &uniflo_core::mirror::CargoSource, reason: uniflo_core::mirror::PickReason) -> Option<String> {
    use uniflo_core::mirror::PickReason;
    let host = source_label(s);
    match (reason, s.official, s.latency_ms) {
        (PickReason::Probed, true, Some(ms)) => Some(tr!(
            "测速选择官方源 crates.io（{ms} ms，UNIFLO_MIRROR=cn 强制国内镜像）",
            "selected official crates.io by probe ({ms} ms; UNIFLO_MIRROR=cn for China mirrors)"
        )),
        (PickReason::Probed, false, Some(ms)) => Some(tr!(
            "测速选择镜像 {host}（{ms} ms，UNIFLO_MIRROR=official 改回官方源）",
            "selected mirror {host} by probe ({ms} ms; UNIFLO_MIRROR=official for crates.io)"
        )),
        (PickReason::Override, false, Some(ms)) => Some(tr!(
            "按 UNIFLO_MIRROR=cn 使用镜像 {host}（{ms} ms）",
            "using mirror {host} via UNIFLO_MIRROR=cn ({ms} ms)"
        )),
        (PickReason::Override, false, None) => {
            Some(tr!("按 UNIFLO_MIRROR=cn 使用镜像 {host}", "using mirror {host} via UNIFLO_MIRROR=cn"))
        }
        (PickReason::Override, true, Some(ms)) => Some(tr!(
            "按 UNIFLO_MIRROR=official 使用官方源 crates.io（{ms} ms）",
            "using official crates.io via UNIFLO_MIRROR=official ({ms} ms)"
        )),
        (PickReason::Override, true, None) => Some(tr!(
            "按 UNIFLO_MIRROR=official 使用官方源 crates.io",
            "using official crates.io via UNIFLO_MIRROR=official"
        )),
        (PickReason::Timezone, false, _) => Some(tr!(
            "按时区使用国内镜像 {host}（UNIFLO_MIRROR=official 改回官方源）",
            "fetching crates through {host} for your time zone (UNIFLO_MIRROR=official to use crates.io)"
        )),
        _ => None,
    }
}

/// True when cargo's failure tail indicates a registry/network/sync failure (safe to retry via
/// another candidate registry) rather than a local Rust compilation failure.
fn is_network_cargo_error(tail: &std::collections::VecDeque<String>) -> bool {
    let joined = tail.iter().map(String::as_str).collect::<Vec<_>>().join("\n").to_ascii_lowercase();
    [
        "failed to download",
        "failed to get ",
        "failed to query replaced source",
        "failed to select a version",
        "spurious network error",
        "could not connect",
        "couldn't connect",
        "could not resolve host",
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "status code",
        "http2",
        "ssl",
        "tls",
        "failed to fetch",
    ]
    .iter()
    .any(|needle| joined.contains(needle))
}

struct CargoFail {
    code: String,
    tail: std::collections::VecDeque<String>,
}

/// `cargo install` with smart network-probed registry selection, safe working directory, and
/// automatic fallback to the next candidate registry when a mirror/network download fails.
fn run_cargo(version: &str, probes: &[uniflo_core::mirror::IndexProbe]) -> Result<String> {
    let cargo_home = uniflo_core::mirror::cargo_home();
    if uniflo_core::mirror::has_user_cargo_replacement(&cargo_home) {
        return match run_cargo_once(version, &[])? {
            Ok(()) => Ok("cargo install".to_owned()),
            Err(fail) => {
                for l in &fail.tail {
                    eprintln!("    {l}");
                }
                let code = fail.code;
                bail!(tr!("cargo install 失败（退出码 {code}）", "cargo install failed (exit code {code})"));
            }
        };
    }

    let override_r = uniflo_core::mirror::env_override();
    let (sources, reason) =
        uniflo_core::mirror::rank_cargo_sources(probes, override_r, uniflo_core::mirror::timezone_region());
    // When user explicitly pinned UNIFLO_MIRROR, do not silently jump to the opposite group.
    let attempts: Vec<&uniflo_core::mirror::CargoSource> = match override_r {
        Some(uniflo_core::mirror::Region::Global) => sources.iter().take(1).collect(),
        Some(uniflo_core::mirror::Region::China) => sources.iter().filter(|s| !s.official).collect(),
        None => sources.iter().collect(),
    };

    for (i, source) in attempts.iter().enumerate() {
        if i == 0
            && let Some(hint) = source_hint(source, reason)
        {
            println!("{}", step(Mark::Info, &hint));
        }
        let args = uniflo_core::mirror::cargo_args_for(source);
        match run_cargo_once(version, &args)? {
            Ok(()) => {
                return Ok(if source.official {
                    "cargo install".to_owned()
                } else {
                    format!("cargo install · {}", source_label(source))
                });
            }
            Err(fail) => {
                if let Some(next) = attempts.get(i + 1)
                    && is_network_cargo_error(&fail.tail)
                {
                    let cur_host = source_label(source);
                    let next_host = source_label(next);
                    println!(
                        "{}",
                        step(
                            Mark::Warn,
                            &tr!(
                                "{cur_host} 下载失败，自动切换至 {next_host} 重试…",
                                "{cur_host} download failed, retrying via {next_host}…"
                            )
                        )
                    );
                    continue;
                }
                for l in &fail.tail {
                    eprintln!("    {l}");
                }
                let code = fail.code;
                bail!(tr!("cargo install 失败（退出码 {code}）", "cargo install failed (exit code {code})"));
            }
        }
    }
    bail!(tr!("cargo install 失败：没有可用的下载源", "cargo install failed: no registry available"))
}

/// One `cargo install` invocation with its build log folded into one live line; the log's tail is
/// returned only when the command exits non-zero.
fn run_cargo_once(version: &str, extra_args: &[String]) -> Result<std::result::Result<(), CargoFail>> {
    let mut child = std::process::Command::new("cargo")
        .args(["install", "uniflo", "--force", "--color", "never", "--version", version])
        .args(extra_args)
        .current_dir(safe_workdir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            tr!(
                "cargo 不可用；手动运行 `cargo install uniflo --force --version {version}`",
                "cargo is not available; run `cargo install uniflo --force --version {version}` yourself"
            )
        })?;
    let mut live = Live::new();
    let start = tr!("cargo 编译 {version}…", "building {version} with cargo…");
    if live.on {
        live.set(&start);
    } else {
        println!("{}", step(Mark::Info, &start));
    }
    let mut tail = std::collections::VecDeque::new();
    let mut n = 0;
    let stderr = child.stderr.take().context("cargo stderr")?;
    for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
        if let Some(name) = line.trim_start().strip_prefix("Compiling ").and_then(|r| r.split(' ').next()) {
            n += 1;
            live.set(&tr!(
                "cargo 编译 {version} · 第 {n} 个 crate · {name}",
                "building {version} with cargo · crate {n} · {name}"
            ));
        }
        if tail.len() == 40 {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    let status = child.wait()?;
    live.clear();
    if !status.success() {
        let code = status.code().map_or_else(|| "signal".to_owned(), |c| c.to_string());
        return Ok(Err(CargoFail { code, tail }));
    }
    Ok(Ok(()))
}

/// Returns the host the package came from when it was not GitHub itself.
fn binary_upgrade(exe: &Path, mut rec: InstallRecord, record_file: &Path, version: &str) -> Result<Option<String>> {
    let target = match rec.target.as_str() {
        "" => install::host_target()
            .context(tr!("这个平台没有预编译发布包", "no prebuilt package for this platform"))?
            .to_owned(),
        t => t.to_owned(),
    };
    let mut live = Live::new();
    live.set(&tr!("下载 {version}（{target}）…", "downloading {version} ({target})…"));
    let bases = install::release_bases();
    let res = install::upgrade_binary(exe, &target, version, &bases);
    live.clear();
    let from = res.with_context(|| tr!("升级失败，{} 未改动", "upgrade failed, {} left unchanged", exe.display()))?;
    let via = (from != bases[0]).then(|| uniflo_core::mirror::host(from).to_owned());
    rec.version = version.to_owned();
    rec.target = target;
    if let Err(e) = rec.save(record_file) {
        eprintln!(
            "uniflo: {}",
            tr!(
                "新版本已就位，但 {} 未能更新：{e:#}",
                "the new version is in place, but {} could not be updated: {e:#}",
                record_file.display()
            )
        );
    }
    Ok(via)
}

/// The two ways to upgrade an install `uniflo update` does not manage.
fn manual_ways(exe: &Path, version: Option<&str>) -> String {
    let cargo =
        format!("cargo install uniflo --force{}", version.map(|v| format!(" --version {v}")).unwrap_or_default());
    let script = if cfg!(windows) {
        let pin = version.map(|v| format!("$env:UNIFLO_VERSION='{v}'; ")).unwrap_or_default();
        format!("{pin}irm {} | iex", install::INSTALL_PS1_URL)
    } else {
        let pin = version.map(|v| format!("UNIFLO_VERSION={v} ")).unwrap_or_default();
        format!("curl -fsSL {} | {pin}sh", install::INSTALL_SH_URL)
    };
    let exe = exe.display();
    tr!(
        "无法识别 {exe} 的安装方式（不在 cargo 的 bin 目录，也没有对应的 install.json），uniflo update 不会改动它。升级方式二选一：\n  {cargo}\n  {script}",
        "cannot tell how {exe} was installed (not in a cargo bin directory, no matching install.json), so uniflo update leaves it alone. Upgrade it either way:\n  {cargo}\n  {script}"
    )
}

/// After an upgrade, run by the new executable: make the daemon serve this version and say how.
pub fn refresh_daemon() {
    let Ok(exe) = std::env::current_exe() else { return };
    println!("{}", daemon_step(&install::canon(&exe)));
}

fn daemon_step(exe: &Path) -> String {
    let mine = env!("CARGO_PKG_VERSION");
    let pid = crate::read_daemon_pid();
    let launch = pid.and_then(crate::Launch::find);
    let bind = launch.as_ref().map(|l| l.bind()).unwrap_or_else(|| {
        std::env::var("UNIFLO_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| crate::DEFAULT_URL.to_owned())[7..]
            .trim_end_matches('/')
            .to_owned()
    });
    let url = format!("http://{bind}");
    let token = std::env::var("UNIFLO_TOKEN").ok().filter(|t| !t.is_empty());
    let running = running_version(&url, token.clone());
    if running.as_deref() == Some(mine) {
        return step(Mark::Ok, &tr!("守护进程已是 {mine}（{url}）", "daemon already runs {mine} ({url})"));
    }
    if let Some(pid) = pid {
        let Some(l) = launch else {
            return step(
                Mark::Warn,
                &tr!(
                    "后台守护进程（pid {pid}）仍是旧版本：运行 `uniflo daemon --restart`",
                    "background daemon (pid {pid}) still runs the old version: run `uniflo daemon --restart`"
                ),
            );
        };
        if !same_file(Path::new(&l.exe), exe) {
            let other = &l.exe;
            let exe = exe.display();
            return step(
                Mark::Warn,
                &tr!(
                    "后台守护进程运行的是 {other}，不是刚升级的 {exe}，未重启",
                    "background daemon runs {other}, not the upgraded {exe}; not restarted"
                ),
            );
        }
        if l.token && token.is_none() {
            return step(
                Mark::Warn,
                &tr!(
                    "后台守护进程启用了 token：设置 UNIFLO_TOKEN 后运行 `uniflo daemon --restart`",
                    "background daemon uses a token: set UNIFLO_TOKEN, then run `uniflo daemon --restart`"
                ),
            );
        }
        let _ = crate::stop_daemon(true);
        let args: Vec<std::ffi::OsString> = l.args.iter().map(Into::into).collect();
        return match crate::start_background(exe, &args, &bind, token) {
            Ok(crate::Started::Up { pid, .. } | crate::Started::Slow { pid }) => step(
                Mark::Ok,
                &tr!("守护进程已重启：{url}（pid {pid}，{mine}）", "daemon restarted: {url} (pid {pid}, {mine})"),
            ),
            Ok(crate::Started::Already(_)) => step(
                Mark::Warn,
                &tr!(
                    "旧守护进程停止后 {url} 仍被占用：运行 `uniflo daemon --restart`",
                    "{url} is still taken after stopping the old daemon: run `uniflo daemon --restart`"
                ),
            ),
            Err(e) => step(
                Mark::Fail,
                &tr!(
                    "守护进程重启失败：{e:#}；运行 `uniflo daemon`",
                    "daemon restart failed: {e:#}; run `uniflo daemon`"
                ),
            ),
        };
    }
    #[cfg(target_os = "macos")]
    {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let res = launchd(exe, &uid, &mut |args| {
            std::process::Command::new("launchctl")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        });
        if let Some(line) = launchd_step(res, exe) {
            return line;
        }
    }
    match running {
        Some(v) => step(
            Mark::Warn,
            &tr!(
                "{url} 上的守护进程仍是 {v}（前台运行）：重启它后生效",
                "the daemon on {url} still runs {v} (in the foreground): restart it to use {mine}"
            ),
        ),
        None => step(
            Mark::Info,
            &tr!("守护进程未在运行；启动：`uniflo daemon`", "daemon is not running; start it with `uniflo daemon`"),
        ),
    }
}

/// `version` from `/v1/health`; `?` when something answers without one (e.g. it wants a token).
fn running_version(url: &str, token: Option<String>) -> Option<String> {
    let c = crate::client::Client::new(url, token).ok()?;
    let mut r = c.get("/v1/health").ok()?;
    let body: serde_json::Value = serde_json::from_slice(&r.body().unwrap_or_default()).unwrap_or_default();
    Some(body["version"].as_str().unwrap_or("?").to_owned())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum Launchd {
    /// No such service (or no uid): nothing done.
    NotLoaded,
    Restarted,
    Failed,
    /// The service runs another executable (or one we cannot tell).
    Other(Option<String>),
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd_step(res: Launchd, exe: &Path) -> Option<String> {
    Some(match res {
        Launchd::NotLoaded => return None,
        Launchd::Restarted => {
            step(Mark::Ok, &tr!("launchd 服务已重启，新版本在运行", "launchd service restarted on the new version"))
        }
        Launchd::Failed => step(
            Mark::Fail,
            &tr!(
                "launchd 服务重启失败：运行 `launchctl kickstart -k gui/$(id -u)/{LAUNCHD_LABEL}`",
                "launchd service restart failed: run `launchctl kickstart -k gui/$(id -u)/{LAUNCHD_LABEL}`"
            ),
        ),
        Launchd::Other(p) => {
            let other = p.unwrap_or_else(|| tr!("（未知程序）", "(unknown program)"));
            let exe = exe.display();
            step(
                Mark::Warn,
                &tr!(
                    "launchd 服务 {LAUNCHD_LABEL} 运行的是 {other}，不是刚升级的 {exe}，未重启",
                    "launchd service {LAUNCHD_LABEL} runs {other}, not the upgraded {exe}; not restarted"
                ),
            )
        }
    })
}

/// Restart the launchd service only when it runs `exe`: restarting one that runs another
/// executable would not pick up this upgrade. `launchctl` runs one command and returns its
/// stdout when it succeeded (tests inject it).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd(exe: &Path, uid: &str, launchctl: &mut dyn FnMut(&[&str]) -> Option<String>) -> Launchd {
    let service = format!("gui/{uid}/{LAUNCHD_LABEL}");
    let Some(print) = (!uid.is_empty()).then(|| launchctl(&["print", &service])).flatten() else {
        return Launchd::NotLoaded;
    };
    let program = print.lines().find_map(|l| l.trim().strip_prefix("program = ")).map(str::trim);
    match program {
        Some(p) if same_file(Path::new(p), exe) => {
            if launchctl(&["kickstart", "-k", &service]).is_some() {
                Launchd::Restarted
            } else {
                Launchd::Failed
            }
        }
        other => Launchd::Other(other.map(str::to_owned)),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let c = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    c(a) == c(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn print_of(program: &str) -> String {
        format!("gui/501/{LAUNCHD_LABEL} = {{\n\tactive count = 1\n\tstate = running\n\tprogram = {program}\n}}\n")
    }

    #[test]
    fn launchd_kickstart_only_when_the_service_runs_this_executable() {
        let exe = Path::new("/opt/uniflo-test/bin/uniflo");
        let mut calls: Vec<Vec<String>> = Vec::new();

        // not loaded: one read-only probe, nothing restarted
        let res = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            None
        });
        assert_eq!(calls, [["print", "gui/501/com.crosery.uniflo"]]);
        assert_eq!(res, Launchd::NotLoaded);

        // loaded and running this executable: kickstart -k
        calls.clear();
        let res = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some(print_of("/opt/uniflo-test/bin/uniflo"))
        });
        assert_eq!(
            calls,
            [vec!["print", "gui/501/com.crosery.uniflo"], vec!["kickstart", "-k", "gui/501/com.crosery.uniflo"]]
        );
        assert_eq!(res, Launchd::Restarted);
        assert!(launchd_step(res, exe).unwrap().contains("已重启"));

        // loaded but running another executable (e.g. ~/.cargo/bin/uniflo): left alone
        calls.clear();
        let res = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some(print_of("/Users/me/.cargo/bin/uniflo"))
        });
        assert_eq!(calls, [["print", "gui/501/com.crosery.uniflo"]]);
        let msg = launchd_step(res, exe).unwrap();
        assert!(msg.contains("未重启") && msg.contains("/Users/me/.cargo/bin/uniflo"), "{msg}");

        // unparseable output or no uid: never a restart
        calls.clear();
        let res = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some("something else".into())
        });
        assert_eq!((calls.len(), res), (1, Launchd::Other(None)));
        calls.clear();
        let res = launchd(exe, "", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some(print_of("/opt/uniflo-test/bin/uniflo"))
        });
        assert!(calls.is_empty());
        assert_eq!(res, Launchd::NotLoaded);
        assert!(launchd_step(Launchd::NotLoaded, exe).is_none());
    }

    #[test]
    fn manual_ways_name_both_installers() {
        let msg = manual_ways(Path::new("/src/target/release/uniflo"), Some("0.1.5"));
        assert!(msg.contains("cargo install uniflo --force --version 0.1.5"), "{msg}");
        if cfg!(windows) {
            assert!(msg.contains("UNIFLO_VERSION='0.1.5'; irm ") && msg.contains("install.ps1 | iex"), "{msg}");
        } else {
            assert!(msg.contains("install.sh | UNIFLO_VERSION=0.1.5 sh"), "{msg}");
        }
        let msg = manual_ways(Path::new("/x/uniflo"), None);
        assert!(msg.contains("cargo install uniflo --force\n"), "{msg}");
        crate::i18n::set_zh(false);
        let msg = manual_ways(Path::new("/x/uniflo"), None);
        assert!(msg.starts_with("cannot tell how /x/uniflo was installed"), "{msg}");
        crate::i18n::set_zh(true);
    }

    #[test]
    fn step_lines_and_widths() {
        assert_eq!(step(Mark::Ok, "done"), format!("  {} done", MARKS[0]));
        assert_eq!(width("ab"), 2);
        assert_eq!(width("编译 a"), 6);
    }

    #[test]
    fn safe_workdir_exists_and_network_error_detection() {
        assert!(safe_workdir().is_dir());

        let tail = |lines: &[&str]| lines.iter().map(|s| (*s).to_owned()).collect::<std::collections::VecDeque<_>>();
        assert!(is_network_cargo_error(&tail(&["error: failed to download `uniflo v0.2.9`"])));
        assert!(is_network_cargo_error(&tail(&["error: failed to select a version for `uniflo`."])));
        assert!(is_network_cargo_error(&tail(&["Caused by: [28] Timeout was reached"])));
        assert!(!is_network_cargo_error(&tail(&[
            "error[E0425]: cannot find value `x` in this scope",
            "error: could not compile `uniflo`"
        ])));

        let ustc = uniflo_core::mirror::CargoSource {
            index_root: uniflo_core::mirror::CN_INDEXES[1].to_owned(),
            official: false,
            latency_ms: Some(95),
        };
        let hint = source_hint(&ustc, uniflo_core::mirror::PickReason::Probed).unwrap();
        assert!(hint.contains("mirrors.ustc.edu.cn") && hint.contains("95 ms"), "{hint}");
    }
}
