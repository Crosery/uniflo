//! Agent memory and instruction files, read-only: each harness's per-user instruction file,
//! Claude Code's per-project memory, and the instruction files between a directory and its git
//! root.
//!
//! [`read`] serves only paths [`list`] can return for some directory (same file-name tables),
//! never an arbitrary file.

use crate::util::file_mtime_ms;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use uniflo_schema::{MemoryContent, MemoryFile, MemoryScope};

/// Home-relative per-user instruction files and the harness that loads each.
const GLOBAL: &[(&str, &str)] = &[
    (".claude/CLAUDE.md", "claude"),
    (".codex/AGENTS.md", "codex"),
    (".gemini/GEMINI.md", "gemini"),
    (".agents/AGENTS.md", "agents"),
    (".config/opencode/AGENTS.md", "opencode"),
    (".omp/agent/AGENTS.md", "omp"),
    (".qwen/QWEN.md", "qwen"),
    (".kimi-code/AGENTS.md", "kimi"),
];

/// Instruction file names looked up in every directory from cwd to the git root.
const PROJECT: &[(&str, &str)] = &[
    ("AGENTS.md", "agents"),
    ("CLAUDE.md", "claude"),
    ("GEMINI.md", "gemini"),
    ("QWEN.md", "qwen"),
    (".cursorrules", "cursor"),
];

/// Largest `content` [`read`] returns.
pub const MAX_READ: usize = 256 * 1024;

#[derive(Debug)]
pub enum ReadError {
    /// Not a path [`list`] could return.
    Forbidden,
    NotFound,
    Io(std::io::Error),
}

/// Global files, then for each directory from the git root above `cwd` down to `cwd` (only
/// `cwd` outside a repository): its Claude project memory and its instruction files. Only
/// existing regular files (symlinks followed).
pub fn list(home: &Path, cwd: Option<&Path>) -> Vec<MemoryFile> {
    let mut out = Vec::new();
    for (rel, h) in GLOBAL {
        push(&mut out, home.join(rel), MemoryScope::Global, h);
    }
    let Some(cwd) = cwd else { return out };
    let mut dirs: Vec<&Path> = Vec::new();
    for d in cwd.ancestors() {
        dirs.push(d);
        if d.join(".git").exists() {
            break;
        }
    }
    if !cwd.ancestors().any(|d| d.join(".git").exists()) {
        dirs.truncate(1);
    }
    for d in dirs.iter().rev() {
        for p in sorted(&home.join(".claude/projects").join(claude_slug(d)).join("memory"), "md") {
            push(&mut out, p, MemoryScope::Project, "claude");
        }
        for (name, h) in PROJECT {
            push(&mut out, d.join(name), MemoryScope::Project, h);
        }
        for p in sorted(&d.join(".cursor/rules"), "mdc") {
            push(&mut out, p, MemoryScope::Project, "cursor");
        }
    }
    out
}

/// One file from the tables above, at most [`MAX_READ`] bytes of it.
pub fn read(home: &Path, path: &Path) -> Result<MemoryContent, ReadError> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) {
        return Err(ReadError::Forbidden);
    }
    let (scope, harness) = classify(home, path).ok_or(ReadError::Forbidden)?;
    let md = match std::fs::metadata(path) {
        Ok(md) if md.is_file() => md,
        Ok(_) => return Err(ReadError::NotFound),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::NotFound),
        Err(e) => return Err(ReadError::Io(e)),
    };
    let mut buf = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(MAX_READ as u64 + 1).read_to_end(&mut buf)).map_err(ReadError::Io)?;
    let truncated = buf.len() > MAX_READ;
    buf.truncate(MAX_READ);
    if truncated
        && let Err(e) = std::str::from_utf8(&buf)
        && e.error_len().is_none()
    {
        buf.truncate(e.valid_up_to());
    }
    Ok(MemoryContent {
        file: MemoryFile {
            path: path.display().to_string(),
            scope,
            harness: harness.to_owned(),
            bytes: md.len(),
            updated_at: file_mtime_ms(&md),
        },
        content: String::from_utf8_lossy(&buf).into_owned(),
        truncated,
    })
}

/// Which table entry `path` matches, if any.
fn classify(home: &Path, path: &Path) -> Option<(MemoryScope, &'static str)> {
    if let Some((_, h)) = GLOBAL.iter().find(|(rel, _)| home.join(rel) == path) {
        return Some((MemoryScope::Global, h));
    }
    let name = path.file_name()?.to_str()?;
    let parent = path.parent()?;
    let dir = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    if name.ends_with(".md")
        && dir(parent).as_deref() == Some("memory")
        && parent.parent().and_then(Path::parent) == Some(&home.join(".claude/projects"))
    {
        return Some((MemoryScope::Project, "claude"));
    }
    if let Some((_, h)) = PROJECT.iter().find(|(n, _)| *n == name) {
        return Some((MemoryScope::Project, h));
    }
    if name.ends_with(".mdc")
        && dir(parent).as_deref() == Some("rules")
        && parent.parent().and_then(dir).as_deref() == Some(".cursor")
    {
        return Some((MemoryScope::Project, "cursor"));
    }
    None
}

/// Claude Code's project directory name: every UTF-16 unit that is not an ASCII letter or digit
/// becomes `-` (`/Users/me/work_x` → `-Users-me-work-x`).
pub fn claude_slug(dir: &Path) -> String {
    let mut out = String::new();
    for c in dir.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    out
}

fn push(out: &mut Vec<MemoryFile>, path: PathBuf, scope: MemoryScope, harness: &str) {
    let Ok(md) = std::fs::metadata(&path) else { return };
    if md.is_file() && !out.iter().any(|f| Path::new(&f.path) == path) {
        out.push(MemoryFile {
            path: path.display().to_string(),
            scope,
            harness: harness.to_owned(),
            bytes: md.len(),
            updated_at: file_mtime_ms(&md),
        });
    }
}

/// Files with extension `ext` directly in `dir`, by name.
fn sorted(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == ext)).collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn lists_global_project_memory_and_rules() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let proj = t.path().join("proj");
        let sub = proj.join("crates/x");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        write(&home.join(".claude/CLAUDE.md"), "g");
        write(&home.join(".codex/AGENTS.md"), "g2");
        write(&proj.join("AGENTS.md"), "root agents");
        write(&sub.join("CLAUDE.md"), "sub");
        write(&proj.join(".cursor/rules/a.mdc"), "rule");
        write(&proj.join(".cursor/rules/notes.txt"), "no");
        write(&proj.join("README.md"), "no");
        let mem = home.join(".claude/projects").join(claude_slug(&proj)).join("memory");
        write(&mem.join("MEMORY.md"), "idx");
        write(&mem.join("x.md"), "fact");
        write(&home.join(".claude/projects/-other/memory/y.md"), "other project");

        let got: Vec<(String, MemoryScope, String)> = list(&home, Some(&sub))
            .into_iter()
            .map(|f| (f.path.replace(&t.path().display().to_string(), ""), f.scope, f.harness))
            .collect();
        let g = MemoryScope::Global;
        let p = MemoryScope::Project;
        assert_eq!(
            got,
            vec![
                ("/home/.claude/CLAUDE.md".into(), g, "claude".into()),
                ("/home/.codex/AGENTS.md".into(), g, "codex".into()),
                (format!("/home/.claude/projects/{}/memory/MEMORY.md", claude_slug(&proj)), p, "claude".into()),
                (format!("/home/.claude/projects/{}/memory/x.md", claude_slug(&proj)), p, "claude".into()),
                ("/proj/AGENTS.md".into(), p, "agents".into()),
                ("/proj/.cursor/rules/a.mdc".into(), p, "cursor".into()),
                ("/proj/crates/x/CLAUDE.md".into(), p, "claude".into()),
            ]
        );
        assert_eq!(list(&home, None).len(), 2);

        let r = read(&home, &proj.join(".cursor/rules/a.mdc")).unwrap();
        assert_eq!((r.content.as_str(), r.file.scope, r.truncated), ("rule", p, false));
        assert!(read(&home, &mem.join("x.md")).is_ok());
        for bad in [proj.join("README.md"), proj.join(".cursor/rules/notes.txt"), sub.join("../../AGENTS.md")] {
            assert!(matches!(read(&home, &bad), Err(ReadError::Forbidden)), "{bad:?}");
        }
        assert!(matches!(read(&home, Path::new("AGENTS.md")), Err(ReadError::Forbidden)));
        assert!(matches!(read(&home, &proj.join("GEMINI.md")), Err(ReadError::NotFound)));
    }

    #[test]
    fn outside_a_repository_only_cwd_and_reads_are_capped() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let dir = t.path().join("a/b");
        write(&t.path().join("a/AGENTS.md"), "parent, not a repo");
        write(&dir.join("AGENTS.md"), &"字".repeat(MAX_READ));
        let files = list(&home, Some(&dir));
        assert_eq!(files.len(), 1);
        let r = read(&home, &dir.join("AGENTS.md")).unwrap();
        assert!(r.truncated && r.content.len() <= MAX_READ && !r.content.ends_with('\u{FFFD}'));
        assert_eq!(r.file.bytes, 3 * MAX_READ as u64);
    }

    #[test]
    fn claude_slugs() {
        assert_eq!(claude_slug(Path::new("/Users/me/work_file/Uniflo")), "-Users-me-work-file-Uniflo");
        assert_eq!(claude_slug(Path::new("/a/测试")), "-a---");
    }
}
