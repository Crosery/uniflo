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
    /// Optional native hook configuration for context injection and/or cross-agent message pull.
    pub hooks: Option<HookSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookFormat {
    /// `hooks.<Event> = [{ hooks: [{ type: "command", command }] }]` (Claude Code, Codex, CodeBuddy, Qoder, WorkBuddy, Command Code, Devin).
    Claude,
    /// `hooks.<Event> = [{ matcher: "", hooks: [{ name, type: "command", command }] }]` (Gemini CLI).
    Gemini,
    /// Top-level `uniflo.{PreInvocation, PostToolUse}` namespace (Antigravity `hooks.json`).
    Antigravity,
    /// `hooks.<Event> = [{ match: "*", command }]` (Reasonix).
    Reasonix,
}

#[derive(Clone, Copy)]
pub struct HookSpec {
    pub file: &'static str,
    pub format: HookFormat,
    /// Event used for `SessionStart` context injection (if supported).
    pub context_event: Option<&'static str>,
    /// Events used for silent cross-agent message pull hooks (`--message`).
    pub message_events: &'static [&'static str],
}

pub enum Mcp {
    /// The harness's own `mcp add` / `mcp remove` (`{bin}` = the uniflo executable). `probe` is
    /// the file that command writes, only read to see whether a `uniflo` entry is already there.
    Cli { add: &'static [&'static str], remove: &'static [&'static str], probe: Probe, toml_fallback: bool },
    /// Uniflo edits `file` itself: the `uniflo` member under `key` (supports nested paths like `&["mcp", "servers"]`).
    File { file: &'static str, key: &'static [&'static str], shape: Shape },
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
        hooks: Some(HookSpec {
            file: ".claude/settings.json",
            format: HookFormat::Claude,
            context_event: Some("SessionStart"),
            message_events: &["PostToolUse", "UserPromptSubmit"],
        }),
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
        hooks: Some(HookSpec {
            file: ".codex/hooks.json",
            format: HookFormat::Claude,
            context_event: Some("SessionStart"),
            message_events: &["PostToolUse", "UserPromptSubmit"],
        }),
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
        hooks: Some(HookSpec {
            file: ".gemini/settings.json",
            format: HookFormat::Gemini,
            context_event: Some("SessionStart"),
            message_events: &["AfterTool", "BeforeAgent"],
        }),
    },
    Spec {
        id: "antigravity",
        name: "Antigravity",
        bins: &["agy"],
        dirs: &[".gemini/antigravity", ".gemini/antigravity-cli", ".gemini/config"],
        mcp: Mcp::File { file: ".gemini/config/mcp_config.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".gemini/config/skills"),
        hooks: Some(HookSpec {
            file: ".gemini/config/hooks.json",
            format: HookFormat::Antigravity,
            context_event: None,
            message_events: &["PreInvocation", "PostToolUse"],
        }),
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
        hooks: None,
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
        hooks: Some(HookSpec {
            file: ".codebuddy/settings.json",
            format: HookFormat::Claude,
            context_event: Some("SessionStart"),
            message_events: &["PostToolUse", "UserPromptSubmit"],
        }),
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
        hooks: Some(HookSpec {
            file: ".qoder/settings.json",
            format: HookFormat::Claude,
            context_event: Some("SessionStart"),
            message_events: &["PostToolUse", "UserPromptSubmit"],
        }),
    },
    Spec {
        id: "workbuddy",
        name: "WorkBuddy",
        bins: &["workbuddy"],
        dirs: &[".workbuddy"],
        mcp: Mcp::File { file: ".workbuddy/mcp.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".workbuddy/skills"),
        hooks: Some(HookSpec {
            file: ".workbuddy/settings.json",
            format: HookFormat::Claude,
            context_event: Some("SessionStart"),
            message_events: &["PostToolUse", "UserPromptSubmit"],
        }),
    },
    Spec {
        id: "cursor",
        name: "Cursor",
        bins: &["cursor-agent"],
        dirs: &[".cursor"],
        mcp: Mcp::File { file: ".cursor/mcp.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".cursor/skills"),
        hooks: None,
    },
    Spec {
        id: "omp",
        name: "oh-my-pi",
        bins: &["omp"],
        dirs: &[".omp"],
        mcp: Mcp::File { file: ".omp/agent/mcp.json", key: &["mcpServers"], shape: Shape::Stdio },
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "opencode",
        name: "OpenCode",
        bins: &["opencode"],
        dirs: &[".config/opencode"],
        mcp: Mcp::File { file: ".config/opencode/opencode.json", key: &["mcp"], shape: Shape::OpenCode },
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "pi",
        name: "Pi",
        bins: &["pi"],
        dirs: &[".pi"],
        mcp: Mcp::File { file: ".pi/agent/mcp.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".pi/agent/skills"),
        hooks: None,
    },
    Spec {
        id: "commandcode",
        name: "Command Code",
        bins: &["commandcode"],
        dirs: &[".commandcode"],
        mcp: Mcp::File { file: ".commandcode/mcp.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".commandcode/skills"),
        hooks: Some(HookSpec {
            file: ".commandcode/settings.json",
            format: HookFormat::Claude,
            context_event: None,
            message_events: &["PostToolUse"],
        }),
    },
    Spec {
        id: "kilo",
        name: "Kilo Code",
        bins: &["kilo"],
        dirs: &[".config/kilo", ".local/share/kilo"],
        mcp: Mcp::File { file: ".config/kilo/kilo.jsonc", key: &["mcp"], shape: Shape::OpenCode },
        skill: Skill::Link(".config/kilo/skills"),
        hooks: None,
    },
    Spec {
        id: "mimocode",
        name: "MiMo Code",
        bins: &["mimocode"],
        dirs: &[".config/mimocode", ".local/share/mimocode"],
        mcp: Mcp::File { file: ".config/mimocode/mimocode.jsonc", key: &["mcp"], shape: Shape::OpenCode },
        skill: Skill::Link(".config/mimocode/skills"),
        hooks: None,
    },
    Spec {
        id: "zcode",
        name: "ZCode",
        bins: &["zcode"],
        dirs: &[".zcode"],
        mcp: Mcp::File { file: ".zcode/cli/config.json", key: &["mcp", "servers"], shape: Shape::Stdio },
        skill: Skill::Link(".zcode/skills"),
        hooks: None,
    },
    Spec {
        id: "prime",
        name: "Prime Agent",
        bins: &["prime"],
        dirs: &[".prime"],
        mcp: Mcp::File { file: ".prime/agent/settings.json", key: &["mcpServers"], shape: Shape::Stdio },
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "kimi",
        name: "Kimi Code",
        bins: &["kimi"],
        dirs: &[".kimi-code"],
        mcp: Mcp::File { file: ".kimi-code/mcp.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "kiro",
        name: "Kiro",
        bins: &["kiro-cli", "kiro"],
        dirs: &[".kiro"],
        mcp: Mcp::File { file: ".kiro/settings/mcp.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".kiro/skills"),
        hooks: None,
    },
    Spec {
        id: "copilot",
        name: "GitHub Copilot CLI",
        bins: &["copilot"],
        dirs: &[".copilot"],
        mcp: Mcp::File { file: ".copilot/mcp-config.json", key: &["mcpServers"], shape: Shape::Copilot },
        skill: Skill::Link(".copilot/skills"),
        hooks: None,
    },
    Spec {
        id: "qwen",
        name: "Qwen Code",
        bins: &["qwen"],
        dirs: &[".qwen", ".qwenworkcn"],
        mcp: Mcp::File { file: ".qwen/settings.json", key: &["mcpServers"], shape: Shape::Plain },
        skill: Skill::Link(".qwen/skills"),
        hooks: None,
    },
    Spec {
        id: "reasonix",
        name: "Reasonix",
        bins: &["reasonix"],
        dirs: &[".reasonix"],
        mcp: Mcp::None,
        skill: Skill::Link(".reasonix/skills"),
        hooks: Some(HookSpec {
            file: ".reasonix/settings.json",
            format: HookFormat::Reasonix,
            context_event: Some("SessionStart"),
            message_events: &["PostToolUse", "UserPromptSubmit"],
        }),
    },
    Spec {
        id: "devin",
        name: "Devin CLI",
        bins: &["devin"],
        dirs: &[".config/devin", ".local/share/devin"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: Some(HookSpec {
            file: ".config/devin/config.json",
            format: HookFormat::Claude,
            context_event: Some("SessionStart"),
            message_events: &["UserPromptSubmit"],
        }),
    },
    Spec {
        id: "hermes",
        name: "Hermes",
        bins: &["hermes"],
        dirs: &[".hermes"],
        mcp: Mcp::None,
        skill: Skill::Link(".hermes/skills"),
        hooks: None,
    },
    Spec {
        id: "minimax",
        name: "MiniMax Code",
        bins: &["minimax"],
        dirs: &[".minimax"],
        mcp: Mcp::None,
        skill: Skill::Link(".minimax/skills"),
        hooks: None,
    },
    Spec {
        id: "grok",
        name: "Grok",
        bins: &["grok"],
        dirs: &[".grok"],
        mcp: Mcp::None,
        skill: Skill::Link(".grok/skills"),
        hooks: None,
    },
    Spec {
        id: "dsh",
        name: "DeepSeek Harness",
        bins: &["dsh"],
        dirs: &[".dsh"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "crosery",
        name: "Crosery Agent",
        bins: &["crosery"],
        dirs: &[".crosery"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "openclaw",
        name: "OpenClaw",
        bins: &["openclaw"],
        dirs: &[".openclaw", ".clawdbot"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "craft",
        name: "Craft Agents",
        bins: &["craft"],
        dirs: &[".craft-agent"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "cline",
        name: "Cline",
        bins: &["cline"],
        dirs: &[".cline"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "roo",
        name: "Roo Code",
        bins: &["roo"],
        dirs: &[".roo"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
    Spec {
        id: "kodu",
        name: "Kodu",
        bins: &["kodu"],
        dirs: &[".kodu"],
        mcp: Mcp::None,
        skill: Skill::Shared,
        hooks: None,
    },
];

/// `--agents` also accepts executable names and common aliases.
pub fn lookup(id: &str) -> Option<&'static Spec> {
    let id = match id.trim().to_ascii_lowercase().as_str() {
        "droid" => "factory".to_owned(),
        "qodercli" => "qoder".to_owned(),
        "cursor-agent" => "cursor".to_owned(),
        "claude-code" => "claude".to_owned(),
        "agy" => "antigravity".to_owned(),
        "kiro-cli" => "kiro".to_owned(),
        "clawdbot" => "openclaw".to_owned(),
        "craft-agent" => "craft".to_owned(),
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
        let join =
            |base: &Path, r: &str| r.split('/').filter(|s| !s.is_empty()).fold(base.to_path_buf(), |p, c| p.join(c));
        if let Some(d) = &self.codex_home
            && let Some(rest) = rel.strip_prefix(".codex/").or((rel == ".codex").then_some(""))
        {
            return join(d, rest);
        }
        if let Some(d) = &self.claude_config_dir {
            if rel == ".claude.json" {
                return d.join(".claude.json");
            }
            if let Some(rest) = rel.strip_prefix(".claude/").or((rel == ".claude").then_some("")) {
                return join(d, rest);
            }
        }
        join(&self.home, rel)
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
            Ok(rest) => format!("~/{}", rest.display().to_string().replace('\\', "/")),
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
            && p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["exe", "cmd", "bat", "com"].iter().any(|x| e.eq_ignore_ascii_case(x)))
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
        assert_eq!(SPECS.len(), 33, "setup SPECS covers all 33 adapters");
        for s in SPECS {
            if let Mcp::Cli { add, .. } = s.mcp {
                assert!(add.contains(&"uniflo") && add.contains(&"{bin}") && add.last() == Some(&"mcp"), "{}", s.id);
            }
        }
        assert_eq!(lookup("droid").unwrap().id, "factory");
        assert_eq!(lookup("agy").unwrap().id, "antigravity");
        assert_eq!(lookup("Antigravity").unwrap().id, "antigravity");
        assert_eq!(lookup(" Claude ").unwrap().id, "claude");
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn detects_antigravity_by_agy_or_subdirs_not_plain_gemini() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        let bin = t.path().join("bin");
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let mk_env = |path: Vec<PathBuf>| Env {
            home: home.clone(),
            path,
            sandboxed: true,
            claude_config_dir: None,
            codex_home: None,
        };
        let ids = |e: &Env| detect(e).into_iter().map(|d| d.spec.id).collect::<Vec<_>>();
        assert_eq!(ids(&mk_env(vec![])), vec!["gemini"], "plain ~/.gemini only matches Gemini CLI");

        for sub in [".gemini/antigravity", ".gemini/antigravity-cli", ".gemini/config"] {
            let p = home.join(sub);
            std::fs::create_dir_all(&p).unwrap();
            assert_eq!(ids(&mk_env(vec![])), vec!["gemini", "antigravity"], "{sub}");
            std::fs::remove_dir(&p).unwrap();
        }

        let agy = bin.join(if cfg!(windows) { "agy.exe" } else { "agy" });
        std::fs::write(&agy, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let found: Vec<(&str, bool)> =
            detect(&mk_env(vec![bin])).into_iter().map(|d| (d.spec.id, d.cli.is_some())).collect();
        assert_eq!(found, vec![("gemini", false), ("antigravity", true)]);
    }

    #[test]
    fn detects_by_path_or_config_dir_only() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("h");
        let bin = t.path().join("bin");
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let droid = bin.join(if cfg!(windows) { "droid.exe" } else { "droid" });
        std::fs::write(&droid, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&droid, std::fs::Permissions::from_mode(0o755)).unwrap();
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
