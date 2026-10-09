//! User-confirmed session cleanup: plan → confirm → execute.
//!
//! A plan lists, per requested session, whether it may be cleaned and what would move: the
//! files its adapter declares ([`crate::Adapter::cleanup_targets`]) for it and its whole
//! sub-agent tree. Executing re-checks every target against the plan, hashes the files, writes
//! the compact archive and the manifest (cleanup log, flushed), and only then moves the files
//! to the trash. Nothing is ever deleted; a session that fails leaves its files in place.
//! Decision and safety boundary: `docs/decisions/ADR-0006-会话清理与写接口.md`.

pub mod targets;
pub mod trash;

use crate::archive::{ArchiveStore, Archived, FileRecord, compact, sha256_file};
use crate::engine::{CleanupView, Engine};
use crate::util::{file_mtime_ms, now_ms};
use anyhow::{Result, anyhow};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use trash::Trash;
use uniflo_schema::cleanup::{
    ArchiveEntry, ArchiveList, ArchiveRemoved, CleanupCandidate, CleanupPlan, CleanupReport, CleanupResult,
    CleanupStatus, CleanupTarget,
};
use uniflo_schema::{Event, Status};

/// How long a plan stays executable.
pub const PLAN_TTL: Duration = Duration::from_secs(10 * 60);
/// Plan-time archive estimate: one tenth of the bytes moved (the result has the real size).
const ARCHIVE_RATIO: u64 = 10;

pub struct CleanupOptions {
    pub trash: Arc<dyn Trash>,
    pub plan_ttl: Duration,
}

impl CleanupOptions {
    /// System trash, or what `UNIFLO_TRASH_DIR` / `UNIFLO_HOME` select (see [`trash::from_env`]).
    pub fn from_env() -> Self {
        CleanupOptions { trash: trash::from_env(), plan_ttl: PLAN_TTL }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// No such plan (or it already ran).
    Unknown,
    /// Older than the plan TTL: plan again.
    Expired,
}

/// Why a session cannot be cleaned (stable `reason` codes of the API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Unknown,
    Archived,
    Subagent,
    Unsupported,
    Working,
    Live,
    SourceMissing,
    OutsideRoot,
    Symlink,
    Hardlink,
    Shared,
    Changed,
    ArchiveFailed,
    TrashFailed,
    TrashPartial,
}

impl Why {
    fn code(self) -> &'static str {
        match self {
            Why::Unknown => "unknown_session",
            Why::Archived => "archived",
            Why::Subagent => "subagent",
            Why::Unsupported => "unsupported",
            Why::Working => "working",
            Why::Live => "live_process",
            Why::SourceMissing => "source_missing",
            Why::OutsideRoot => "outside_root",
            Why::Symlink => "symlink",
            Why::Hardlink => "hardlink",
            Why::Shared => "shared_target",
            Why::Changed => "source_changed",
            Why::ArchiveFailed => "archive_failed",
            Why::TrashFailed => "trash_failed",
            Why::TrashPartial => "trash_partial",
        }
    }

    fn text(self) -> &'static str {
        match self {
            Why::Unknown => "未知会话",
            Why::Archived => "已归档",
            Why::Subagent => "需随父会话一起清理",
            Why::Unsupported => "不支持清理",
            Why::Working => "会话运行中",
            Why::Live => "有存活进程",
            Why::SourceMissing => "源文件不存在",
            Why::OutsideRoot => "目标不在 harness 根目录内",
            Why::Symlink => "目标是符号链接",
            Why::Hardlink => "目标是共享硬链接",
            Why::Shared => "目标同时属于另一个会话",
            Why::Changed => "源文件已变化",
            Why::ArchiveFailed => "写入归档失败",
            Why::TrashFailed => "移入回收站失败",
            Why::TrashPartial => "只有部分文件移入了回收站",
        }
    }

    fn message(self, detail: Option<&str>) -> String {
        match detail {
            Some(d) => format!("{}：{d}", self.text()),
            None => self.text().to_owned(),
        }
    }
}

/// A plan entry plus what execution needs.
struct Eval {
    cand: CleanupCandidate,
    tree: Vec<String>,
    targets: Vec<PathBuf>,
}

impl Eval {
    fn reject(mut self, why: Why, detail: Option<&str>) -> Eval {
        self.cand.eligible = false;
        self.cand.reason = Some(why.code().into());
        self.cand.message = Some(why.message(detail));
        self
    }
}

pub struct Cleanup {
    engine: Arc<Engine>,
    store: Arc<ArchiveStore>,
    trash: Arc<dyn Trash>,
    ttl: Duration,
    plans: Mutex<HashMap<String, CleanupPlan>>,
    /// One execution at a time.
    running: Mutex<()>,
    seq: AtomicU64,
}

impl Cleanup {
    /// Fails when the engine has no data directory to archive into.
    pub fn new(engine: Arc<Engine>, opts: CleanupOptions) -> Result<Cleanup> {
        let store = engine.archive().cloned().ok_or_else(|| anyhow!("no data directory: archives are unavailable"))?;
        Ok(Cleanup {
            engine,
            store,
            trash: opts.trash,
            ttl: opts.plan_ttl,
            plans: Mutex::new(HashMap::new()),
            running: Mutex::new(()),
            seq: AtomicU64::new(0),
        })
    }

    pub fn trash(&self) -> &dyn Trash {
        &*self.trash
    }

    // ---------------------------------------------------------------- plan

    /// Evaluate `keys` and keep the plan for [`Cleanup::execute`]. A requested sub-agent is
    /// listed as `subagent`: it goes with its parent's tree. Reads metadata only; changes nothing.
    pub fn plan(&self, keys: &[String]) -> CleanupPlan {
        let view = self.engine.cleanup_view();
        let mut seen = HashSet::new();
        let sessions: Vec<CleanupCandidate> =
            keys.iter().filter(|k| seen.insert(k.as_str())).map(|k| self.evaluate(&view, k).cand).collect();
        let now = now_ms();
        let eligible = sessions.iter().filter(|c| c.eligible);
        let plan = CleanupPlan {
            plan_id: self.new_id(now),
            created_at: now,
            expires_at: now + self.ttl.as_millis() as i64,
            freed_bytes: eligible.clone().map(|c| c.bytes).sum(),
            archive_bytes: eligible.map(|c| c.archive_bytes).sum(),
            sessions,
        };
        let mut plans = self.plans.lock().unwrap();
        // Expired plans linger one more TTL so executing them answers "expired", not "unknown".
        plans.retain(|_, p| p.expires_at + self.ttl.as_millis() as i64 > now);
        plans.insert(plan.plan_id.clone(), plan.clone());
        plan
    }

    fn new_id(&self, now: i64) -> String {
        let mut h = Sha256::new();
        h.update(now.to_le_bytes());
        h.update(std::process::id().to_le_bytes());
        h.update(self.seq.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        h.update(nanos.to_le_bytes());
        h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
    }

    fn evaluate(&self, view: &CleanupView, key: &str) -> Eval {
        let mut ev = Eval {
            cand: CleanupCandidate { key: key.to_owned(), ..Default::default() },
            tree: Vec::new(),
            targets: Vec::new(),
        };
        let Some(item) = view.sessions.get(key) else { return ev.reject(Why::Unknown, None) };
        ev.cand.harness = item.session.harness.clone();
        ev.cand.title = item.session.title.clone().or_else(|| item.session.preview.clone());
        if item.archived {
            return ev.reject(Why::Archived, None);
        }
        if let Some(p) = &item.session.parent
            && view.sessions.get(p).is_some_and(|x| !x.archived)
        {
            return ev.reject(Why::Subagent, Some(p));
        }
        ev.tree = descendants(view, key);
        ev.cand.children = ev.tree[1..].to_vec();
        let adapters = self.engine.adapters();
        let mut targets: Vec<PathBuf> = Vec::new();
        let mut roots: Vec<PathBuf> = Vec::new();
        for k in &ev.tree {
            let it = &view.sessions[k];
            let child = (k != key).then(|| format!("子会话 {k}"));
            let a = &adapters[it.adapter];
            let Some(ts) = a.cleanup_targets(&it.src, &it.session.id) else {
                return ev.reject(Why::Unsupported, child.as_deref());
            };
            if it.session.status == Status::Work {
                return ev.reject(Why::Working, child.as_deref());
            }
            if let Some(pid) = it.session.pid {
                return ev.reject(Why::Live, Some(&child.unwrap_or_else(|| format!("pid {pid}"))));
            }
            if std::fs::symlink_metadata(&it.src).is_err() {
                return ev.reject(Why::SourceMissing, Some(&it.src.display().to_string()));
            }
            targets.extend(ts.into_iter().filter(|t| std::fs::symlink_metadata(t).is_ok()));
            roots.extend(a.roots().iter().filter_map(|r| r.canonicalize().ok()));
        }
        let mut uniq = HashSet::new();
        targets.retain(|t| uniq.insert(t.clone()));
        let all = targets.clone();
        targets.retain(|t| !all.iter().any(|o| o != t && t.starts_with(o)));
        for t in &targets {
            if !within(t, &roots) {
                return ev.reject(Why::OutsideRoot, Some(&t.display().to_string()));
            }
        }
        for t in &targets {
            match stamp(t) {
                Ok(s) => ev.cand.targets.push(s),
                Err((why, p)) => return ev.reject(why, Some(&p.display().to_string())),
            }
        }
        let tree: HashSet<&str> = ev.tree.iter().map(String::as_str).collect();
        for (p, keys) in &view.sources {
            if targets.iter().any(|t| p.starts_with(t))
                && let Some(other) = keys.iter().find(|k| !tree.contains(k.as_str()))
            {
                return ev.reject(Why::Shared, Some(other));
            }
        }
        ev.cand.eligible = true;
        ev.cand.bytes = ev.cand.targets.iter().map(|t| t.bytes).sum();
        ev.cand.archive_bytes = ev.cand.bytes.div_ceil(ARCHIVE_RATIO);
        ev.targets = targets;
        ev
    }

    // ---------------------------------------------------------------- execute

    /// Run a plan once: each eligible session in turn, a failure never stops the others.
    pub fn execute(&self, plan_id: &str) -> Result<CleanupReport, ExecError> {
        let plan = {
            let mut plans = self.plans.lock().unwrap();
            match plans.get(plan_id) {
                None => return Err(ExecError::Unknown),
                Some(p) if p.expires_at <= now_ms() => return Err(ExecError::Expired),
                Some(_) => plans.remove(plan_id).unwrap(),
            }
        };
        let _one = self.running.lock().unwrap();
        let mut report = CleanupReport { plan_id: plan.plan_id.clone(), ..Default::default() };
        for c in &plan.sessions {
            let r = if c.eligible {
                self.clean(&plan.plan_id, c)
            } else {
                CleanupResult {
                    key: c.key.clone(),
                    status: CleanupStatus::Skipped,
                    reason: c.reason.clone(),
                    message: c.message.clone(),
                    freed_bytes: 0,
                    archive_bytes: 0,
                    children: Vec::new(),
                }
            };
            report.freed_bytes += r.freed_bytes;
            report.archive_bytes += r.archive_bytes;
            report.results.push(r);
        }
        Ok(report)
    }

    fn clean(&self, plan_id: &str, want: &CleanupCandidate) -> CleanupResult {
        let key = want.key.as_str();
        let mut res = CleanupResult {
            key: key.to_owned(),
            status: CleanupStatus::Failed,
            reason: None,
            message: None,
            freed_bytes: 0,
            archive_bytes: 0,
            children: want.children.clone(),
        };
        let fail = |mut r: CleanupResult, why: Why, detail: Option<&str>| {
            r.reason = Some(why.code().into());
            r.message = Some(why.message(detail));
            r
        };
        let log_fail = |why: Why, detail: &str| {
            let _ = self.store.log(&json!({"at": now_ms(), "plan": plan_id, "key": key, "event": "failed", "reason": why.code(), "detail": detail}));
        };

        // 1. Ownership and file stamps exactly as planned.
        let now = self.evaluate(&self.engine.cleanup_view(), key);
        if !now.cand.eligible {
            let why = now.cand.reason.clone();
            res.reason = why;
            res.message = now.cand.message;
            return res;
        }
        if now.cand.targets != want.targets || now.cand.children != want.children {
            return fail(res, Why::Changed, None);
        }
        let (tree, targets) = (now.tree, now.targets);

        // 2. Read and hash every file.
        let files = match manifest(&targets) {
            Ok(f) if f.iter().map(|f| f.size).sum::<u64>() == want.bytes => f,
            _ => return fail(res, Why::Changed, None),
        };

        // 3. Archives, then the manifest in the log and the index, all durable before anything moves.
        let at = now_ms();
        let mut entries: Vec<Archived> = Vec::new();
        let mut events: HashMap<String, Vec<Event>> = HashMap::new();
        for k in &tree {
            match self.archive_one(k, key, at) {
                Ok((a, evs)) => {
                    events.insert(k.clone(), evs);
                    entries.push(a);
                }
                Err(err) => {
                    self.drop_files(&entries);
                    let detail = format!("{k}: {err:#}");
                    log_fail(Why::ArchiveFailed, &detail);
                    return fail(res, Why::ArchiveFailed, Some(&detail));
                }
            }
        }
        entries[0].files = files.clone();
        entries[0].source_bytes = want.bytes;
        let archives: Vec<_> =
            entries.iter().map(|a| json!({"key": a.session.key, "file": a.file, "bytes": a.bytes})).collect();
        let logged = self.store.log(&json!({
            "at": at, "plan": plan_id, "key": key, "event": "archived", "files": files, "archives": archives,
        }));
        if let Err(err) = logged.and_then(|_| self.store.insert(entries.clone())) {
            self.drop_files(&entries);
            let _ = self.store.discard(&tree);
            return fail(res, Why::ArchiveFailed, Some(&format!("{err:#}")));
        }
        let archive_bytes: u64 = entries.iter().map(|a| a.bytes).sum();

        // 4. Last look, then into the trash.
        let unchanged = targets.iter().map(|t| stamp(t).ok()).collect::<Vec<_>>()
            == want.targets.iter().cloned().map(Some).collect::<Vec<_>>();
        if !unchanged {
            let _ = self.store.discard(&tree);
            log_fail(Why::Changed, "after archiving");
            return fail(res, Why::Changed, None);
        }
        let retired = self.engine.retire(&entries, &events, &targets, key);
        let mut moved = 0;
        let mut error = None;
        for t in &targets {
            if let Err(err) = self.trash.trash(t) {
                error = Some(format!("{}: {err:#}", t.display()));
                break;
            }
            moved += 1;
        }
        match error {
            None => {
                let _ = self.store.save_tombstones();
                let _ = self.store.log(&json!({"at": now_ms(), "plan": plan_id, "key": key, "event": "trashed", "trash": self.trash.describe()}));
                res.status = CleanupStatus::Archived;
                res.freed_bytes = want.bytes;
                res.archive_bytes = archive_bytes;
                res
            }
            Some(detail) if moved == 0 => {
                self.engine.unretire(retired);
                let _ = self.store.discard(&tree);
                log_fail(Why::TrashFailed, &detail);
                fail(res, Why::TrashFailed, Some(&detail))
            }
            Some(detail) => {
                // What moved cannot come back on its own: keep the archive, re-index what stayed.
                self.engine.retire_partial(&targets[moved..]);
                let _ = self.store.save_tombstones();
                log_fail(Why::TrashPartial, &detail);
                res.freed_bytes = want.targets[..moved].iter().map(|t| t.bytes).sum();
                res.archive_bytes = archive_bytes;
                fail(res, Why::TrashPartial, Some(&detail))
            }
        }
    }

    fn archive_one(&self, key: &str, root: &str, at: i64) -> Result<(Archived, Vec<Event>)> {
        let (mut session, mut events) = self.engine.archive_material(key)?;
        for e in &mut events {
            compact::compact(e);
        }
        let file = ArchiveStore::file_for(&session.harness, &session.id);
        let source = PathBuf::from(&session.source);
        session.archived = true;
        session.status = Status::Idle;
        session.status_reason = Some("archived".into());
        session.pid = None;
        session.source = self.store.dir().join(&file).display().to_string();
        let bytes = self.store.write_file(&file, &compact::encode(&session, &events)?)?;
        let a = Archived {
            session,
            file,
            bytes,
            archived_at: at,
            root: root.to_owned(),
            source,
            files: Vec::new(),
            source_bytes: 0,
        };
        Ok((a, events))
    }

    fn drop_files(&self, entries: &[Archived]) {
        for a in entries {
            let _ = std::fs::remove_file(self.store.dir().join(&a.file));
        }
    }

    // ---------------------------------------------------------------- archive management

    /// Every archived session, newest cleanup first.
    pub fn archives(&self) -> ArchiveList {
        let mut archives: Vec<ArchiveEntry> = self
            .store
            .entries()
            .into_iter()
            .map(|a| {
                let restored = self.engine.session(&a.session.key).is_some_and(|s| !s.archived);
                let s = a.session;
                ArchiveEntry {
                    root: (a.root != s.key).then_some(a.root),
                    path: self.store.dir().join(&a.file).display().to_string(),
                    bytes: a.bytes,
                    source: a.source.display().to_string(),
                    source_bytes: a.source_bytes,
                    archived_at: a.archived_at,
                    restored,
                    key: s.key,
                    harness: s.harness,
                    id: s.id,
                    title: s.title.or(s.preview),
                    cwd: s.cwd,
                }
            })
            .collect();
        archives.sort_by(|a, b| b.archived_at.cmp(&a.archived_at).then_with(|| a.key.cmp(&b.key)));
        ArchiveList { bytes: archives.iter().map(|a| a.bytes).sum(), archives }
    }

    /// Permanently delete an archive (a root's sub-agent archives go with it). Sessions that only
    /// lived in those archives leave the list. `None`: no such archive.
    pub fn remove_archive(&self, key: &str) -> Result<Option<ArchiveRemoved>> {
        let _one = self.running.lock().unwrap();
        let gone = self.store.remove(key)?;
        if gone.is_empty() {
            return Ok(None);
        }
        let removed: Vec<String> = gone.iter().map(|a| a.session.key.clone()).collect();
        self.engine.forget_archived(&removed);
        let _ = self.store.log(&json!({"at": now_ms(), "key": key, "event": "archive_removed", "removed": removed}));
        Ok(Some(ArchiveRemoved { bytes: gone.iter().map(|a| a.bytes).sum(), removed }))
    }
}

/// `key` and every session below it (by `parent`), skipping archived ones.
fn descendants(view: &CleanupView, key: &str) -> Vec<String> {
    let mut kids: HashMap<&str, Vec<&str>> = HashMap::new();
    for (k, it) in &view.sessions {
        if let Some(p) = &it.session.parent
            && !it.archived
        {
            kids.entry(p.as_str()).or_default().push(k.as_str());
        }
    }
    let mut out = vec![key.to_owned()];
    let mut i = 0;
    while i < out.len() {
        let mut next: Vec<&str> = kids.get(out[i].as_str()).cloned().unwrap_or_default();
        next.sort();
        for k in next {
            if !out.iter().any(|o| o == k) {
                out.push(k.to_owned());
            }
        }
        i += 1;
    }
    out
}

/// Strictly inside one of `roots` (canonical), without `..`, by its real parent directory.
fn within(t: &Path, roots: &[PathBuf]) -> bool {
    if t.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let (Some(parent), Some(name)) = (t.parent().and_then(|p| p.canonicalize().ok()), t.file_name()) else {
        return false;
    };
    let full = parent.join(name);
    roots.iter().any(|r| full.starts_with(r) && &full != r)
}

#[cfg(unix)]
fn file_id(md: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.ino())
}

#[cfg(not(unix))]
fn file_id(_: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn shared_link(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    md.nlink() > 1
}

#[cfg(not(unix))]
fn shared_link(_: &std::fs::Metadata) -> bool {
    false
}

/// Every regular file under `t` (or `t` itself), never following links. Symlinks and files with
/// other hard links make the whole target ineligible.
fn walk(t: &Path, f: &mut dyn FnMut(&Path, &std::fs::Metadata)) -> Result<std::fs::Metadata, (Why, PathBuf)> {
    let md = std::fs::symlink_metadata(t).map_err(|_| (Why::SourceMissing, t.to_path_buf()))?;
    let mut stack = vec![(t.to_path_buf(), md.clone())];
    while let Some((p, m)) = stack.pop() {
        let ft = m.file_type();
        if ft.is_symlink() {
            return Err((Why::Symlink, p));
        }
        if ft.is_dir() {
            let rd = std::fs::read_dir(&p).map_err(|_| (Why::SourceMissing, p.clone()))?;
            for ent in rd.flatten() {
                let m = std::fs::symlink_metadata(ent.path()).map_err(|_| (Why::SourceMissing, ent.path()))?;
                stack.push((ent.path(), m));
            }
        } else if shared_link(&m) {
            return Err((Why::Hardlink, p));
        } else {
            f(&p, &m);
        }
    }
    Ok(md)
}

fn stamp(t: &Path) -> Result<CleanupTarget, (Why, PathBuf)> {
    let mut s = CleanupTarget { path: t.display().to_string(), ..Default::default() };
    let md = walk(t, &mut |_, m| {
        s.bytes += m.len();
        s.files += 1;
        s.mtime_ms = s.mtime_ms.max(file_mtime_ms(m));
    })?;
    s.dir = md.is_dir();
    s.file_id = file_id(&md);
    Ok(s)
}

/// Path, size, mtime and SHA-256 of every file the targets hold.
fn manifest(targets: &[PathBuf]) -> Result<Vec<FileRecord>> {
    let mut files: Vec<(PathBuf, u64, i64)> = Vec::new();
    for t in targets {
        walk(t, &mut |p, m| files.push((p.to_path_buf(), m.len(), file_mtime_ms(m))))
            .map_err(|(why, p)| anyhow!("{}: {}", why.text(), p.display()))?;
    }
    files.sort();
    files
        .into_iter()
        .map(|(path, size, mtime_ms)| Ok(FileRecord { sha256: sha256_file(&path)?, path, size, mtime_ms }))
        .collect()
}
