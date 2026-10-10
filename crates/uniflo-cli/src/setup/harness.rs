//! Which harnesses `uniflo setup` knows, how each is detected, how it registers an MCP server
//! and where it looks for skills. Paths are home-relative.

use std::path::{Path, PathBuf};

pub struct Spec {
    pub id: &'static str,
    pub name: &'static str,
    /// Executables looked up in `PATH`.
    pub bins: &'static [&'static str],
    /// Configuration directories; any existing one counts as installed.
    pub dirs: &'static [&'static str],
    pub mcp: Mcp,
    pub skill: Skill,
}

pub enum Mcp {
    /// The harness's own `mcp add` / `mcp remove` (`{bin}` = the uniflo executable). `probe` is
    /// the file that command writes, only read to see whether a `uniflo` entry is already there.
    Cli { add: &'static [&'static str], remove: &'static [&'static str], probe: Probe, toml_fallback: bool },
    /// Uniflo edits `file` itself: the `uniflo` member of the `key` object.
    File { file: &'static str, key: &'static str, shape: Shape },
    /// No usable MCP client: the Skill is installed instead.
    None,
}

pub enum Probe {
    /// `mcpServers.uniflo` of a JSON file.
    Json(&'static str),
    /// `[mcp_servers.uniflo]` of a TOML file.
    Toml(&'static str),
}

/// The JSON a harness expects for one stdio server.
#[derive(Clone, Copy)]
pub enum Shape {
    /// `{command, args}`
    Plain,
    /// `{type: "stdio", command, args}` (omp)
    Stdio,
    /// `{type: "local", command, args, tools: ["*"]}` (GitHub Copilot CLI)
    Copilot,
    /// `{type: "local", command: [program, ...args], enabled: true}` (OpenCode)
    OpenCode,
}

pub enum Skill {
    /// Loads `~/.agents/skills` itself.
    Shared,
    /// Needs a `uniflo` symlink in this skills directory.
    Link(&'static str),
}

pub const SPECS: &[Spec] = &[
    Spec {
        id: "claude",
        name: "Claude Code",
        bins: &["claude"],
        dirs: &[".claude"],
        mcp: Mcp::Cli {
            add: &["mcp", "add", "--scope", "user", "uniflo", "--", "{bin}", "mcp"],
            remove: &["mcp", "remove", "--scope", "user", "uniflo"],
            probe: Probe::Json(".claude.json"),
            toml_fallback: false,
        },
        skill: Skill::Link(".claude/skills"),
    },
    Spec {
        id: "codex",
        name: "Codex",
        bins: &["codex"],
        dirs: &[".codex"],
        mcp: Mcp::Cli {
            add: &["mcp", "add", "uniflo", "--", "{bin}", "mcp"],
            remove: &["mcp", "remove", "uniflo"],
            probe: Probe::Toml(".codex/config.toml"),
            toml_fallback: true,
        },
        skill: Skill::Shared,
    },
    Spec {
        id: "gemini",
        name: "Gemini CLI",
        bins: &["gemini"],
        dirs: &[".gemini"],
        mcp: Mcp::Cli {
            add: &["mcp", "add", "-s", "user", "uniflo", "{bin}", "mcp"],
            remove: &["mcp", "remove", "-s", "user", "uniflo"],
            probe: Probe::Json(".gemini/settings.json"),
            toml_fallback: false,
        },
        skill: Skill::Shared,
    },
    Spec {
        id: "factory",
        name: "Factory Droid",
        bins: &["droid"],
        dirs: &[".factory"],
        mcp: Mcp::Cli {
            add: &["mcp", "add", "uniflo", "--type", "stdio", "--", "{bin}", "mcp"],
            remove: &["mcp", "remove", "uniflo"],
            probe: Probe::Json(".factory/mcp.json"),
            toml_fallback: false,
        },
        skill: Skill::Link(".factory/skills"),
    },
    Spec {
        id: "codebuddy",
        name: "CodeBuddy",
        bins: &["codebuddy"],
        dirs: &[".codebuddy"],
        mcp: Mcp::Cli {
            add: &["mcp", "add", "-s", "user", "uniflo", "--", "{bin}", "mcp"],
            remove: &["mcp", "remove", "-s", "user", "uniflo"],
            probe: Probe::Json(".codebuddy/.mcp.json"),
            toml_fallback: false,
        },
        skill: Skill::Link(".codebuddy/skills"),
    },
    Spec {
        id: "qoder",
        name: "Qoder",
        bins: &["qodercli"],
        dirs: &[".qoder"],
        mcp: Mcp::Cli {
            add: &["mcp", "add", "-s", "user", "uniflo", "--", "{bin}", "mcp"],
            remove: &["mcp", "remove", "-s", "user", "uniflo"],
            probe: Probe::Json(".qoder/settings.json"),
            toml_fallback: false,
        },
        skill: Skill::Link(".qoder/skills"),
    },
    Spec {
        id: "cursor",
        name: "Cursor",
        bins: &["cursor-agent"],
        dirs: &[".cursor"],
        mcp: Mcp::File { file: ".cursor/mcp.json", key: "mcpServers", shape: Shape::Plain },
        skill: Skill::Link(".cursor/skills"),
    },
    Spec {
        id: "omp",
        name: "oh-my-pi",
        bins: &["omp"],
        dirs: &[".omp"],
        mcp: Mcp::File { file: ".omp/agent/mcp.json", key: "mcpServers", shape: Shape::Stdio },
        skill: Skill::Shared,
    },
    Spec {
        id: "opencode",
        name: "OpenCode",
        bins: &["opencode"],
        dirs: &[".config/opencode"],
        mcp: Mcp::File { file: ".config/opencode/opencode.json", key: "mcp", shape: Shape::OpenCode },
        skill: Skill::Shared,
    },
    Spec {
        id: "kimi",
        name: "Kimi Code",
        bins: &["kimi"],
        dirs: &[".kimi-code"],
        mcp: Mcp::File { file: ".kimi-code/mcp.json", key: "mcpServers", shape: Shape::Plain },
        skill: Skill::Shared,
    },
    Spec {
        id: "kiro",
        name: "Kiro",
        bins: &["kiro-cli", "kiro"],
        dirs: &[".kiro"],
        mcp: Mcp::File { file: ".kiro/settings/mcp.json", key: "mcpServers", shape: Shape::Plain },
        skill: Skill::Link(".kiro/skills"),
    },
    Spec {
        id: "copilot",
        name: "GitHub Copilot CLI",
        bins: &["copilot"],
        dirs: &[".copilot"],
        mcp: Mcp::File { file: ".copilot/mcp-config.json", key: "mcpServers", shape: Shape::Copilot },
        skill: Skill::Link(".copilot/skills"),
    },
    Spec {
        id: "qwen",
        name: "Qwen Code",
        bins: &["qwen"],
        dirs: &[".qwen"],
        mcp: Mcp::File { file: ".qwen/settings.json", key: "mcpServers", shape: Shape::Plain },
        skill: Skill::Link(".qwen/skills"),
    },
    Spec {
        id: "pi",
        name: "Pi",
        bins: &["pi"],
        dirs: &[".pi"],
        mcp: Mcp::None,
        skill: Skill::Link(".pi/agent/skills"),
    },
    Spec {
        id: "grok",
        name: "Grok",
        bins: &["grok"],
        dirs: &[".grok"],
        mcp: Mcp::None,
        skill: Skill::Link(".grok/skills"),
    },
];

/// `--agents` also accepts the executable names.
pub fn lookup(id: &str) -> Option<&'static Spec> {
    let id = match id.trim().to_ascii_lowercase().as_str() {
        "droid" => "factory".to_owned(),
        "qodercli" => "qoder".to_owned(),
        "cursor-agent" => "cursor".to_owned(),
        "claude-code" => "claude".to_owned(),
        other => other.to_owned(),
    };
    SPECS.iter().find(|s| s.id == id)
}

/// Where setup looks: the home directory, `PATH`, and harness overrides of their config dirs.
pub struct Env {
    pub home: PathBuf,
    pub path: Vec<PathBuf>,
    /// `UNIFLO_HOME` is set: harness CLIs run with `HOME` pointed there and without their
    /// config-dir overrides, so nothing outside it is touched.
    pub sandboxed: bool,
    pub claude_config_dir: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Env {
        let sandboxed = std::env::var_os("UNIFLO_HOME").is_some_and(|h| !h.is_empty());
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty() && !sandboxed).map(PathBuf::from);
        Env {
            home: uniflo_core::util::home(),
            path: std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
            sandboxed,
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
        }
    }

    /// A home-relative path, honouring `CLAUDE_CONFIG_DIR` / `CODEX_HOME`.
    pub fn file(&self, rel: &str) -> PathBuf {
        if let Some(d) = &self.codex_home
            && let Some(rest) = rel.strip_prefix(".codex/").or((rel == ".codex").then_some(""))
        {
            return d.join(rest);
        }
        if let Some(d) = &self.claude_config_dir {
            if rel == ".claude.json" {
                return d.join(".claude.json");
            }
            if let Some(rest) = rel.strip_prefix(".claude/").or((rel == ".claude").then_some("")) {
                return d.join(rest);
            }
        }
        self.home.join(rel)
    }

    pub fn which(&self, bin: &str) -> Option<PathBuf> {
        let names: Vec<String> = if cfg!(windows) {
            vec![format!("{bin}.exe"), format!("{bin}.cmd"), bin.to_owned()]
        } else {
            vec![bin.to_owned()]
        };
        self.path.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| is_executable(p))
    }

    /// `~/x/y` for display.
    pub fn show(&self, p: &Path) -> String {
        match p.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        }
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(md) = std::fs::metadata(p) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.is_file() && md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        md.is_file()
    }
}

/// A harness found on this machine.
pub struct Detected {
    pub spec: &'static Spec,
    /// Its executable, when on `PATH`.
    pub cli: Option<PathBuf>,
}

/// Installed = an executable on `PATH` or an existing configuration directory.
pub fn detect(env: &Env) -> Vec<Detected> {
    SPECS
        .iter()
        .filter_map(|spec| {
            let cli = spec.bins.iter().find_map(|b| env.which(b));
            let dir = spec.dirs.iter().any(|d| env.file(d).is_dir());
            (cli.is_some() || dir).then_some(Detected { spec, cli })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        let mut ids: Vec<&str> = SPECS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SPECS.len());
        for s in SPECS {
            if let Mcp::Cli { add, .. } = s.mcp {
                assert!(add.contains(&"uniflo") && add.contains(&"{bin}") && add.last() == Some(&"mcp"), "{}", s.id);
            }
        }
        assert_eq!(lookup("droid").unwrap().id, "factory");
        assert_eq!(lookup(" Claude ").unwrap().id, "claude");
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn detects_by_path_or_config_dir_only() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        let bin = t.path().join("bin");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("droid"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join("droid"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(bin.join("codex"), "not executable").unwrap();
        let env = Env {
            home: home.clone(),
            path: vec![bin.clone()],
            sandboxed: true,
            claude_config_dir: None,
            codex_home: None,
        };
        let found: Vec<(&str, bool)> = detect(&env).iter().map(|d| (d.spec.id, d.cli.is_some())).collect();
        assert_eq!(found, vec![("factory", true), ("cursor", false)]);
        let before: Vec<_> = std::fs::read_dir(&home).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(before.len(), 1, "detection creates nothing");
    }

    #[test]
    fn config_dir_overrides() {
        let env = Env {
            home: "/h".into(),
            path: vec![],
            sandboxed: false,
            claude_config_dir: Some("/c".into()),
            codex_home: Some("/x".into()),
        };
        assert_eq!(env.file(".claude.json"), PathBuf::from("/c/.claude.json"));
        assert_eq!(env.file(".claude/skills"), PathBuf::from("/c/skills"));
        assert_eq!(env.file(".codex/config.toml"), PathBuf::from("/x/config.toml"));
        assert_eq!(env.file(".cursor/mcp.json"), PathBuf::from("/h/.cursor/mcp.json"));
        assert_eq!(env.show(Path::new("/h/.cursor/mcp.json")), "~/.cursor/mcp.json");
    }
}
