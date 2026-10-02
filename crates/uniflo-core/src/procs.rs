//! Process probe for harnesses without a live registry: `ps` for start time + argv,
//! one batched `lsof` for working directories and open files. Results are memoized
//! briefly because the engine polls liveness every second.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    /// Process start, epoch ms.
    pub start_ms: i64,
    pub args: String,
    pub cwd: Option<PathBuf>,
    /// Open files accepted by the cache's file filter (empty without one).
    pub files: Vec<PathBuf>,
}

impl Proc {
    /// Value following `flag` in argv (`--resume <path>`).
    pub fn arg_after(&self, flag: &str) -> Option<&str> {
        let mut it = self.args.split_whitespace();
        while let Some(a) = it.next() {
            if a == flag {
                return it.next();
            }
            if let Some(v) = a.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
                return Some(v);
            }
        }
        None
    }
}

/// Memoized process listing filtered by an argv predicate.
pub struct ProcCache {
    ttl: Duration,
    keep: fn(&str) -> bool,
    files: Option<fn(&Path) -> bool>,
    last: Mutex<Option<(Instant, Vec<Proc>)>>,
}

impl ProcCache {
    pub const fn new(ttl: Duration, keep: fn(&str) -> bool) -> Self {
        ProcCache { ttl, keep, files: None, last: Mutex::new(None) }
    }

    /// Also collect open files matching `pred` (e.g. transcripts held open by the agent).
    pub const fn with_files(mut self, pred: fn(&Path) -> bool) -> Self {
        self.files = Some(pred);
        self
    }

    pub fn get(&self) -> Vec<Proc> {
        let mut g = self.last.lock().unwrap();
        if let Some((at, v)) = g.as_ref()
            && at.elapsed() < self.ttl
        {
            return v.clone();
        }
        let v = list(self.keep, self.files);
        *g = Some((Instant::now(), v.clone()));
        v
    }
}

/// All processes whose command line satisfies `keep`, with cwd (and open files matching
/// `files`) resolved.
pub fn list(keep: fn(&str) -> bool, files: Option<fn(&Path) -> bool>) -> Vec<Proc> {
    let Ok(out) = Command::new("ps").args(["-Aww", "-o", "pid=,lstart=,command="]).env("LC_ALL", "C").output() else {
        return Vec::new();
    };
    let mut procs: Vec<Proc> =
        String::from_utf8_lossy(&out.stdout).lines().filter_map(parse_ps_line).filter(|p| keep(&p.args)).collect();
    if procs.is_empty() {
        return procs;
    }
    let mut info = inspect(&procs.iter().map(|p| p.pid).collect::<Vec<_>>(), files);
    for p in &mut procs {
        if let Some((cwd, files)) = info.remove(&p.pid) {
            p.cwd = cwd;
            p.files = files;
        }
    }
    procs
}

/// `  123 Fri Oct  2 12:02:01 2026     /usr/bin/foo --bar`
fn parse_ps_line(line: &str) -> Option<Proc> {
    let mut it = line.split_whitespace();
    let pid: u32 = it.next()?.parse().ok()?;
    let parts: Vec<&str> = it.by_ref().take(5).collect();
    if parts.len() < 5 {
        return None;
    }
    let stamp = format!("{} {} {} {}", parts[1], parts[2], parts[3], parts[4]);
    let start = chrono::NaiveDateTime::parse_from_str(&stamp, "%b %d %H:%M:%S %Y").ok()?;
    let start_ms = chrono::TimeZone::from_local_datetime(&chrono::Local, &start).single()?.timestamp_millis();
    let args = it.collect::<Vec<_>>().join(" ");
    Some(Proc { pid, start_ms, args, cwd: None, files: Vec::new() })
}

type Inspected = HashMap<u32, (Option<PathBuf>, Vec<PathBuf>)>;

fn inspect(pids: &[u32], files: Option<fn(&Path) -> bool>) -> Inspected {
    #[cfg(target_os = "linux")]
    {
        pids.iter()
            .map(|p| {
                let cwd = std::fs::read_link(format!("/proc/{p}/cwd")).ok();
                let open = files
                    .and_then(|pred| {
                        let rd = std::fs::read_dir(format!("/proc/{p}/fd")).ok()?;
                        Some(
                            rd.flatten()
                                .filter_map(|e| std::fs::read_link(e.path()).ok())
                                .filter(|f| pred(f))
                                .collect(),
                        )
                    })
                    .unwrap_or_default();
                (*p, (cwd, open))
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        // Numbered descriptors only: skipping txt/mem mappings keeps lsof fast.
        let fds = if files.is_some() { "cwd,0-65535" } else { "cwd" };
        let Ok(out) = Command::new("lsof").args(["-a", "-d", fds, "-Ffn", "-p", &list]).output() else {
            return HashMap::new();
        };
        parse_lsof(&String::from_utf8_lossy(&out.stdout), files)
    }
}

/// `lsof -Ffn` field output: `p<pid>`, then per descriptor `f<fd>` and `n<name>`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_lsof(out: &str, files: Option<fn(&Path) -> bool>) -> Inspected {
    let mut map: Inspected = HashMap::new();
    let (mut pid, mut fd) = (None::<u32>, String::new());
    for l in out.lines() {
        match l.split_at_checked(1) {
            Some(("p", v)) => pid = v.parse().ok(),
            Some(("f", v)) => fd = v.to_owned(),
            Some(("n", name)) => {
                let Some(pid) = pid else { continue };
                let slot = map.entry(pid).or_default();
                if fd == "cwd" {
                    slot.0 = Some(PathBuf::from(name));
                } else if files.is_some_and(|pred| pred(Path::new(name))) {
                    slot.1.push(PathBuf::from(name));
                }
            }
            _ => {}
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_lines() {
        let p = parse_ps_line("  4242 Fri Oct  2 12:02:01 2026     bun /x/bin/omp --resume /s/a.jsonl").unwrap();
        assert_eq!(p.pid, 4242);
        assert!(p.start_ms > 1_790_000_000_000);
        assert_eq!(p.args, "bun /x/bin/omp --resume /s/a.jsonl");
        assert_eq!(p.arg_after("--resume"), Some("/s/a.jsonl"));
        assert_eq!(parse_ps_line("garbage"), None);
    }

    #[test]
    fn finds_this_test_process_with_cwd() {
        fn me(args: &str) -> bool {
            args.contains("uniflo_core") || args.contains("procs::")
        }
        let found = list(me, None);
        let mine = found.iter().find(|p| p.pid == std::process::id()).expect("own process listed");
        assert_eq!(mine.cwd.as_deref(), std::env::current_dir().ok().as_deref());
        assert!((crate::util::now_ms() - mine.start_ms) < 600_000);
    }

    #[test]
    fn open_files_of_this_process() {
        fn me(args: &str) -> bool {
            args.contains("uniflo_core") || args.contains("procs::")
        }
        fn marker(p: &Path) -> bool {
            p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("uniflo-procs-probe"))
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uniflo-procs-probe.jsonl");
        let _held = std::fs::File::create(&path).unwrap();
        let found = list(me, Some(marker));
        let mine = found.iter().find(|p| p.pid == std::process::id()).expect("own process listed");
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(mine.files.iter().map(|f| canon(f)).collect::<Vec<_>>(), vec![canon(&path)]);
    }

    #[test]
    fn parses_lsof_fields() {
        fn jsonl(p: &Path) -> bool {
            p.extension().is_some_and(|e| e == "jsonl")
        }
        let out = "p10\nfcwd\nn/work/a\nf3\nn/s/x.jsonl\nf4\nn/s/y.log\np11\nfcwd\nn/work/b\n";
        let m = parse_lsof(out, Some(jsonl));
        assert_eq!(m[&10], (Some(PathBuf::from("/work/a")), vec![PathBuf::from("/s/x.jsonl")]));
        assert_eq!(m[&11], (Some(PathBuf::from("/work/b")), vec![]));
        assert!(parse_lsof(out, None)[&10].1.is_empty());
    }
}
