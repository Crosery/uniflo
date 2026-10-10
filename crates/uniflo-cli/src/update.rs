//! `uniflo update`: check crates.io, then upgrade the way this executable was installed
//! (ADR-0009): `cargo install` for a cargo install, the release package for a binary install,
//! only instructions for anything else. A launchd daemon running this executable is restarted,
//! then the setup step follows.

use crate::setup;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;
use uniflo_core::UpdateInfo;
use uniflo_core::install::{self, InstallRecord, Method};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LAUNCHD_LABEL: &str = "com.crosery.uniflo";

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
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let record_file = InstallRecord::file();
    let record = InstallRecord::load(&record_file);
    let method = install::detect(&exe, record.as_ref(), &install::cargo_bin_dirs());
    let info = uniflo_core::update::check();
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
    let record = match (method, record) {
        (Method::Unknown, _) => {
            println!("{}", manual_ways(&exe, Some(version)));
            return Ok(());
        }
        (Method::Binary, Some(r)) => Some(r),
        _ => None,
    };
    if check {
        return Ok(());
    }
    match record {
        Some(rec) => binary_upgrade(&exe, rec, &record_file, version)?,
        None => cargo_install(version)?,
    }
    println!("已安装 uniflo {}{}。", version, if is_pre { "（预发布版）" } else { "" });
    restart_daemon(&exe, method);
    setup::after_update(&exe);
    Ok(())
}

fn cargo_install(version: &str) -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["install", "uniflo", "--force", "--version", version])
        .status()
        .with_context(|| format!("cargo 不可用；手动运行 `cargo install uniflo --force --version {version}`"))?;
    if !status.success() {
        bail!("cargo install 失败（exit {:?}）", status.code());
    }
    Ok(())
}

fn binary_upgrade(exe: &Path, mut rec: InstallRecord, record_file: &Path, version: &str) -> Result<()> {
    let target = match rec.target.as_str() {
        "" => install::host_target().context("这个平台没有预编译发布包")?.to_owned(),
        t => t.to_owned(),
    };
    install::upgrade_binary(exe, &target, version, &install::release_base())
        .with_context(|| format!("升级失败，{} 未改动", exe.display()))?;
    rec.version = version.to_owned();
    rec.target = target;
    if let Err(e) = rec.save(record_file) {
        eprintln!("uniflo: 新版本已就位，但 {} 未能更新：{e:#}", record_file.display());
    }
    Ok(())
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
    format!(
        "无法识别 {} 的安装方式（不在 cargo 的 bin 目录，也没有对应的 install.json），uniflo update 不会改动它。升级方式二选一：\n  {cargo}\n  {script}",
        exe.display()
    )
}

/// Replace the running daemon so the new executable actually serves requests.
fn restart_daemon(exe: &Path, method: Method) {
    #[cfg(target_os = "macos")]
    {
        let _ = method;
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        let msg = launchd(exe, &uid, &mut |args| {
            std::process::Command::new("launchctl")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        });
        println!("{msg}");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = exe;
        println!(
            "{}",
            if method == Method::Cargo {
                "守护进程若正在运行，请先停掉再启动新版（Windows 上运行中的二进制无法被覆盖，建议 update 前停 daemon）。"
            } else {
                "守护进程若正在运行，重启后生效：`uniflo daemon`（旧进程仍是旧版本）。"
            }
        );
    }
}

/// Restart the launchd service only when it runs `exe`: restarting one that runs another
/// executable would not pick up this upgrade. `launchctl` runs one command and returns its
/// stdout when it succeeded (tests inject it).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchd(exe: &Path, uid: &str, launchctl: &mut dyn FnMut(&[&str]) -> Option<String>) -> String {
    let service = format!("gui/{uid}/{LAUNCHD_LABEL}");
    let Some(print) = (!uid.is_empty()).then(|| launchctl(&["print", &service])).flatten() else {
        return "守护进程若正在运行，重启后生效：`uniflo daemon`（旧进程仍是旧版本）。".to_owned();
    };
    let program = print.lines().find_map(|l| l.trim().strip_prefix("program = ")).map(str::trim);
    match program {
        Some(p) if same_file(Path::new(p), exe) => {
            if launchctl(&["kickstart", "-k", &service]).is_some() {
                "launchd 服务已重启（KeepAlive），新守护进程在跑。".to_owned()
            } else {
                format!("launchd 服务在跑但自动重启失败：`launchctl kickstart -k gui/$(id -u)/{LAUNCHD_LABEL}`")
            }
        }
        other => format!(
            "launchd 服务 {LAUNCHD_LABEL} 运行的是 {}，不是刚升级的 {}，未重启。",
            other.unwrap_or("（未知程序）"),
            exe.display()
        ),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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
        let msg = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            None
        });
        assert_eq!(calls, [["print", "gui/501/com.crosery.uniflo"]]);
        assert!(msg.contains("重启后生效"), "{msg}");

        // loaded and running this executable: kickstart -k
        calls.clear();
        let msg = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some(print_of("/opt/uniflo-test/bin/uniflo"))
        });
        assert_eq!(
            calls,
            [vec!["print", "gui/501/com.crosery.uniflo"], vec!["kickstart", "-k", "gui/501/com.crosery.uniflo"]]
        );
        assert!(msg.contains("已重启"), "{msg}");

        // loaded but running another executable (e.g. ~/.cargo/bin/uniflo): left alone
        calls.clear();
        let msg = launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some(print_of("/Users/me/.cargo/bin/uniflo"))
        });
        assert_eq!(calls, [["print", "gui/501/com.crosery.uniflo"]]);
        assert!(msg.contains("未重启") && msg.contains("/Users/me/.cargo/bin/uniflo"), "{msg}");

        // unparseable output or no uid: never a restart
        calls.clear();
        launchd(exe, "501", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some("something else".into())
        });
        assert_eq!(calls.len(), 1);
        calls.clear();
        launchd(exe, "", &mut |a| {
            calls.push(a.iter().map(|s| s.to_string()).collect());
            Some(print_of("/opt/uniflo-test/bin/uniflo"))
        });
        assert!(calls.is_empty());
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
    }
}
