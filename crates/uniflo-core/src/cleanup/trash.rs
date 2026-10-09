//! Where cleaned-up files go. Production uses the system trash (`trash` crate); tests and
//! sandboxes inject a directory, so they never touch the user's real trash.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub trait Trash: Send + Sync {
    /// Move `path` (file or directory) out of the way, restorable by the user. Never deletes.
    fn trash(&self, path: &Path) -> Result<()>;
    /// For logs and errors.
    fn describe(&self) -> String;
}

/// macOS Trash (via `NSFileManager`: no Finder automation prompt, which a launchd daemon cannot
/// answer; "Put Back" may be missing, dragging the item out restores it), freedesktop trash on
/// Linux, Recycle Bin on Windows.
pub struct SystemTrash;

impl Trash for SystemTrash {
    fn trash(&self, path: &Path) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            let mut ctx = trash::TrashContext::default();
            ctx.set_delete_method(DeleteMethod::NsFileManager);
            ctx.delete(path).map_err(|e| anyhow!("{e}"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            trash::delete(path).map_err(|e| anyhow!("{e}"))
        }
    }

    fn describe(&self) -> String {
        "system trash".into()
    }
}

/// Moves into `<dir>/<original absolute path>` (rename; same volume only). Tests restore by
/// renaming back.
pub struct DirTrash(pub PathBuf);

impl DirTrash {
    /// Where `path` lands (or landed) for the `n`-th time.
    pub fn slot(&self, path: &Path, n: usize) -> PathBuf {
        let rel: PathBuf = path.components().filter(|c| matches!(c, Component::Normal(_))).collect();
        let mut dst = self.0.join(rel);
        if n > 0 {
            let name = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            dst.set_file_name(format!("{name}~{n}"));
        }
        dst
    }
}

impl Trash for DirTrash {
    fn trash(&self, path: &Path) -> Result<()> {
        let dst = (0..).map(|n| self.slot(path, n)).find(|d| std::fs::symlink_metadata(d).is_err()).unwrap();
        if let Some(dir) = dst.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::rename(path, &dst).with_context(|| format!("move {} to {}", path.display(), dst.display()))
    }

    fn describe(&self) -> String {
        format!("trash directory {}", self.0.display())
    }
}

/// Refuses: under `UNIFLO_HOME` (tests, sandboxes) the real trash is off unless a directory is given.
pub struct NoTrash(pub String);

impl Trash for NoTrash {
    fn trash(&self, _: &Path) -> Result<()> {
        bail!("{}", self.0)
    }

    fn describe(&self) -> String {
        self.0.clone()
    }
}

/// `UNIFLO_TRASH_DIR` → [`DirTrash`]; else with `UNIFLO_HOME` set → [`NoTrash`]; else the system trash.
pub fn from_env() -> Arc<dyn Trash> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(d) = var("UNIFLO_TRASH_DIR") {
        return Arc::new(DirTrash(PathBuf::from(d)));
    }
    if var("UNIFLO_HOME").is_some() {
        return Arc::new(NoTrash("system trash is disabled under UNIFLO_HOME; set UNIFLO_TRASH_DIR".into()));
    }
    Arc::new(SystemTrash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_trash_mirrors_paths_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let t = DirTrash(d.path().join("bin"));
        let f = d.path().join("w/a.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        for _ in 0..2 {
            std::fs::write(&f, "x").unwrap();
            t.trash(&f).unwrap();
            assert!(!f.exists());
        }
        assert!(t.slot(&f, 0).is_file() && t.slot(&f, 1).is_file());
        assert!(t.slot(&f, 0).starts_with(d.path().join("bin")));
        assert!(NoTrash("off".into()).trash(&f).is_err());
    }
}
