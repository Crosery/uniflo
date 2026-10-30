//! Other processes that currently have the index file open for writing.
//!
//! The write lease (`store::try_writer`) only binds Uniflo builds that know it: an older `uniflo
//! mcp` that outlived an upgrade, or a stray SQL shell, can still write the index. This module only
//! reports them (best effort, for `/v1/stats`); it never touches them. Per platform: `lsof` on
//! macOS, `/proc/<pid>/fd` plus `fdinfo` flags on Linux, nothing elsewhere.

use std::path::Path;
use uniflo_schema::search::FtsWriter;

/// Processes other than this one holding `path` open for writing, ordered by pid.
pub fn writers_of(path: &Path) -> Vec<FtsWriter> {
    let mut found = imp::writers_of(path, std::process::id());
    found.sort_by_key(|w| w.pid);
    found.dedup_by_key(|w| w.pid);
    found
}

/// The executable and its subcommand (arguments up to the first flag, at most two): enough to tell
/// `uniflo mcp` from `uniflo daemon`, and nothing that could carry a token.
pub fn short_command(args: &[&str]) -> String {
    let mut out = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 && (i > 2 || a.starts_with('-')) {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(a);
    }
    out
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    pub fn writers_of(path: &Path, me: u32) -> Vec<FtsWriter> {
        let Ok(out) = Command::new("lsof").args(["-nP", "-w", "-F", "pfa", "--"]).arg(path).output() else {
            return Vec::new();
        };
        let pids = parse_lsof(&String::from_utf8_lossy(&out.stdout), me);
        if pids.is_empty() {
            return Vec::new();
        }
        let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let ps = Command::new("ps").args(["-o", "pid=,command=", "-p", &list]).output();
        let text = ps.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        pids.into_iter()
            .map(|pid| {
                let line = text
                    .lines()
                    .map(str::trim_start)
                    .find_map(|l| l.strip_prefix(&format!("{pid} ")))
                    .unwrap_or_default();
                let args: Vec<&str> = line.split_whitespace().collect();
                FtsWriter { pid, command: short_command(&args) }
            })
            .collect()
    }

    /// `lsof -F pfa` output: `p<pid>` starts a process, `f<fd>` a descriptor, `a<mode>` its access.
    pub fn parse_lsof(text: &str, me: u32) -> Vec<u32> {
        let (mut pid, mut found) = (None::<u32>, Vec::new());
        for line in text.lines() {
            let Some((tag, rest)) = line.split_at_checked(1) else { continue };
            match tag {
                "p" => pid = rest.parse().ok(),
                "a" if matches!(rest, "w" | "u") => {
                    if let Some(p) = pid.filter(|p| *p != me) {
                        found.push(p);
                    }
                }
                _ => {}
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn lsof_fields_keep_writers_only() {
            let text = "p10\nf9\nau\np11\nf12\nar\np12\nf3\naw\nf4\nar\np13\nf1\nau\n";
            assert_eq!(parse_lsof(text, 13), vec![10, 12]);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn writers_of(path: &Path, me: u32) -> Vec<FtsWriter> {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        let Ok(procs) = std::fs::read_dir("/proc") else { return Vec::new() };
        let mut out = Vec::new();
        for entry in procs.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            if pid == me {
                continue;
            }
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else { continue };
            let writes = fds.flatten().any(|fd| {
                std::fs::read_link(fd.path()).is_ok_and(|l| l == target)
                    && std::fs::read_to_string(entry.path().join("fdinfo").join(fd.file_name()))
                        .ok()
                        .and_then(|info| {
                            let flags = info.lines().find_map(|l| l.strip_prefix("flags:"))?;
                            u32::from_str_radix(flags.trim(), 8).ok()
                        })
                        .is_some_and(|flags| flags & 0o3 != 0)
            });
            if writes {
                let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
                let text = String::from_utf8_lossy(&cmdline).into_owned();
                let args: Vec<&str> = text.split('\0').filter(|a| !a.is_empty()).collect();
                out.push(FtsWriter { pid, command: short_command(&args) });
            }
        }
        out
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;

    pub fn writers_of(_path: &Path, _me: u32) -> Vec<FtsWriter> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_keeps_executable_and_subcommand_only() {
        assert_eq!(short_command(&["/bin/uniflo", "mcp"]), "/bin/uniflo mcp");
        assert_eq!(short_command(&["/bin/uniflo", "daemon", "--token", "secret"]), "/bin/uniflo daemon");
        assert_eq!(short_command(&["/bin/uniflo", "--token", "secret"]), "/bin/uniflo");
        assert_eq!(short_command(&[]), "");
    }
}
