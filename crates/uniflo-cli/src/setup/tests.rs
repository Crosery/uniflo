//! The setup flow against a temporary home with no harness executables on `PATH`: every write
//! goes through the file editors, nothing outside the temp directory is touched.

use super::*;
use serde_json::{Value, json};

const BIN: &str = "/opt/uniflo/bin/uniflo";

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        Home { dir: tempfile::tempdir().unwrap() }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }

    fn json(&self, rel: &str) -> Value {
        serde_json::from_str(&self.read(rel)).unwrap()
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            env: Env { home: self.path(""), path: vec![], sandboxed: true, claude_config_dir: None, codex_home: None },
            bin: BIN.into(),
            state_path: self.path("config/uniflo/setup.json"),
            stamp: "20261009-120000".into(),
            var: |_| None,
        }
    }

    fn setup(&self, args: SetupArgs, tty: bool, input: &str) -> String {
        let mut out = Vec::new();
        flow(&self.ctx(), &args, tty, &mut input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn state(&self) -> State {
        State::load(&self.path("config/uniflo/setup.json")).unwrap()
    }

    /// Every file and symlink under the home: relative path → content or `-> target`.
    fn snapshot(&self) -> BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                let ft = e.file_type().unwrap();
                if ft.is_symlink() {
                    out.insert(rel, format!("-> {}", std::fs::read_link(&p).unwrap().display()));
                } else if ft.is_dir() {
                    walk(root, &p, out);
                } else {
                    out.insert(rel, std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(self.dir.path(), self.dir.path(), &mut out);
        out
    }
}

fn args(f: impl FnOnce(&mut SetupArgs)) -> SetupArgs {
    let mut a = SetupArgs::default();
    f(&mut a);
    a
}

const CURSOR: &str = "{\n  \"mcpServers\": {\n    \"zeta\": {\"command\": \"z\"},\n    \"alpha\": {\"command\": \"a\"}\n  },\n  \"theme\": \"dark\"\n}\n";
const OMP: &str = "{\n  \"$schema\": \"x\",\n  \"mcpServers\": {\n    \"codegraph\": {\"type\": \"stdio\", \"command\": \"cg\", \"args\": []}\n  }\n}\n";
const OPENCODE: &str = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"model\": \"m\"\n}\n";
const QWEN: &str = "{\n  // user comment\n  \"mcpServers\": {}\n}\n";
const KIRO: &str =
    "{\n  \"mcpServers\": {\n    \"uniflo\": {\"command\": \"npx\", \"args\": [\"someone-elses-uniflo\"]}\n  }\n}\n";

fn fixture() -> Home {
    let h = Home::new();
    h.write(".cursor/mcp.json", CURSOR);
    h.write(".omp/agent/mcp.json", OMP);
    h.write(".config/opencode/opencode.json", OPENCODE);
    h.write(".qwen/settings.json", QWEN);
    h.write(".kiro/settings/mcp.json", KIRO);
    std::fs::create_dir_all(h.path(".claude/skills/existing-skill")).unwrap();
    std::fs::create_dir_all(h.path(".codex")).unwrap();
    std::fs::create_dir_all(h.path(".gemini")).unwrap();
    h
}

/// Scenario: 直接编辑配置文件与 Skill 软链（隔离 HOME）.
#[test]
fn both_edits_files_links_skill_and_uninstalls_symmetrically() {
    let h = fixture();
    let before = h.snapshot();
    let out = h.setup(args(|a| (a.both, a.yes) = (true, true)), false, "");

    // New entries appended, existing ones and their order untouched.
    let cursor = h.json(".cursor/mcp.json");
    let keys: Vec<&String> = cursor["mcpServers"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["zeta", "alpha", "uniflo"]);
    assert_eq!(cursor["mcpServers"]["uniflo"], json!({"command": BIN, "args": ["mcp"]}));
    let top: Vec<&String> = cursor.as_object().unwrap().keys().collect();
    assert_eq!(top, ["mcpServers", "theme"]);
    assert_eq!(
        h.json(".omp/agent/mcp.json")["mcpServers"]["uniflo"],
        json!({"type":"stdio","command":BIN,"args":["mcp"]})
    );
    let oc = h.json(".config/opencode/opencode.json");
    assert_eq!(oc["mcp"]["uniflo"]["command"], json!([BIN, "mcp"]), "opencode command is an array");
    assert_eq!(oc.as_object().unwrap().keys().collect::<Vec<_>>(), ["$schema", "model", "mcp"]);
    for f in [".cursor/mcp.json", ".omp/agent/mcp.json", ".config/opencode/opencode.json"] {
        let b = h.path(&format!("{f}.bak-uniflo-20261009-120000"));
        assert_eq!(std::fs::read_to_string(&b).unwrap(), before[f], "{f} backed up before the change");
    }
    // Codex has no CLI on PATH: its TOML is written with toml_edit.
    assert!(h.read(".codex/config.toml").contains("[mcp_servers.uniflo]"));

    // Parse failure and someone else's entry: reported, untouched.
    assert_eq!(h.read(".qwen/settings.json"), QWEN);
    assert!(out.lines().any(|l| l.contains("qwen") && l.contains("解析失败")), "{out}");
    assert_eq!(h.read(".kiro/settings/mcp.json"), KIRO);
    assert!(out.lines().any(|l| l.contains("kiro") && l.contains("occupied") && l.contains("未覆盖")), "{out}");
    assert!(!h.path(".qwen/settings.json.bak-uniflo-20261009-120000").exists());

    // The Skill: one real file, relative links only where the harness does not read ~/.agents/skills.
    let skill = h.path(".agents/skills/uniflo/SKILL.md");
    assert!(skill.is_file() && !skill.is_symlink());
    assert_eq!(std::fs::read_to_string(&skill).unwrap(), crate::agent::SKILL);
    let link = h.path(".claude/skills/uniflo");
    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("../../.agents/skills/uniflo"));
    assert!(link.join("SKILL.md").is_file());
    assert!(std::fs::read_link(h.path(".kiro/skills/uniflo")).is_ok());
    for shared in [".codex/skills", ".gemini/skills", ".omp/agent/skills", ".config/opencode/skills"] {
        assert!(!h.path(shared).exists(), "{shared}: reads ~/.agents/skills, no link");
    }
    assert!(!h.path(".pi").exists() && !h.path(".copilot").exists(), "undetected harnesses get nothing");

    let st = h.state();
    assert_eq!(st.mode, Some(Mode::Both));
    assert_eq!(st.harnesses["qwen"].result, "skipped");
    assert_eq!(st.harnesses["kiro"].result, "occupied");
    assert_eq!(st.harnesses["cursor"].result, "connected");
    assert!(st.links.iter().any(|l| l.ends_with(".claude/skills/uniflo")));
    assert_eq!(st.backups.len(), 3);

    // Again: nothing to do, no new backups.
    let harness_files = |h: &Home| {
        let mut s = h.snapshot();
        s.retain(|k, _| !k.starts_with("config/"));
        s
    };
    let snap = harness_files(&h);
    let out = h.setup(args(|a| (a.both, a.yes) = (true, true)), false, "");
    assert_eq!(harness_files(&h), snap, "second run changes nothing:\n{out}");
    assert!(out.contains("未变化"));

    // Uninstall: only Uniflo's entries and links go; backups stay; everything else as before.
    h.setup(args(|a| (a.uninstall, a.yes) = (true, true)), false, "");
    let after = h.snapshot();
    for (k, v) in &before {
        if k.ends_with(".json") {
            assert_eq!(
                serde_json::from_str::<Value>(&after[k]).ok(),
                serde_json::from_str::<Value>(v).ok(),
                "{k} back to the original content"
            );
        } else {
            assert_eq!(&after[k], v, "{k}");
        }
    }
    let cursor = h.json(".cursor/mcp.json");
    assert_eq!(cursor["mcpServers"].as_object().unwrap().keys().collect::<Vec<_>>(), ["zeta", "alpha"]);
    assert!(!h.path(".claude/skills/uniflo").exists() && h.path(".claude/skills/existing-skill").is_dir());
    assert!(!skill.exists());
    assert!(!h.path(".codex/config.toml").exists(), "created by setup, removed when empty again");
    assert!(after.keys().filter(|k| k.contains(".bak-uniflo-")).count() >= 3, "backups kept");
    let st = h.state();
    assert!(st.harnesses.is_empty() && st.asked && st.mode.is_none());
}

/// Uninstall leaves a link that no longer points at the Skill, and a `uniflo` entry a user put
/// back, alone.
#[test]
fn uninstall_only_touches_what_is_still_uniflos() {
    let h = fixture();
    h.setup(args(|a| (a.both, a.yes) = (true, true)), false, "");
    std::fs::remove_file(h.path(".claude/skills/uniflo")).unwrap();
    std::os::unix::fs::symlink("/somewhere/else", h.path(".claude/skills/uniflo")).unwrap();
    let mut cursor = h.json(".cursor/mcp.json");
    cursor["mcpServers"]["uniflo"] = json!({"command": "npx", "args": ["mine"]});
    h.write(".cursor/mcp.json", &cursor.to_string());
    let out = h.setup(args(|a| (a.uninstall, a.yes) = (true, true)), false, "");
    assert_eq!(std::fs::read_link(h.path(".claude/skills/uniflo")).unwrap(), PathBuf::from("/somewhere/else"));
    assert_eq!(h.json(".cursor/mcp.json")["mcpServers"]["uniflo"]["command"], "npx");
    assert!(out.contains("保留"), "{out}");
}

/// Scenario: 非交互、dry-run 与触发时机 — no terminal, no `--yes`: plan only.
#[test]
fn without_a_terminal_only_the_plan_is_printed() {
    let h = fixture();
    let before = h.snapshot();
    for a in [args(|_| {}), args(|a| a.dry_run = true), args(|a| (a.dry_run, a.yes) = (true, true))] {
        let out = h.setup(a, false, "");
        assert!(out.contains("cursor") && out.contains("编辑 ~/.cursor/mcp.json"), "{out}");
        assert!(out.contains("未写入任何文件"), "{out}");
    }
    let out = h.setup(args(|a| a.dry_run = true), true, "");
    assert!(out.contains("预演"), "a terminal does not turn a dry run into a write: {out}");
    assert_eq!(h.snapshot(), before);
    assert!(!h.path("config/uniflo/setup.json").exists());
}

#[test]
fn interactive_questions_and_confirmation() {
    let h = fixture();
    // Skill only, harnesses 1 and claude by id, no hook, then decline the plan.
    let out = h.setup(args(|_| {}), true, "2\n1,cursor\nn\nn\n");
    assert!(out.contains("选择 agent 接入方式") && out.contains("检测到") && out.contains("已取消"), "{out}");
    assert!(!h.path(".agents").exists());
    // MCP for claude + cursor with the SessionStart hook, confirmed.
    let out = h.setup(args(|_| {}), true, "\nclaude,cursor\ny\n\n");
    assert!(out.contains("SessionStart"), "{out}");
    let settings = h.json(".claude/settings.json");
    let cmd = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
    assert_eq!(cmd, edit::hook_command(BIN));
    assert!(h.json(".cursor/mcp.json")["mcpServers"]["uniflo"].is_object());
    assert_eq!(h.state().harnesses["claude"].result, "skipped", "no claude CLI on PATH");
    assert!(h.state().hook.is_some());
    h.setup(args(|a| (a.uninstall, a.yes) = (true, true)), false, "");
    assert!(!h.path(".claude/settings.json").exists(), "created only for the hook, removed with it");
    // Choosing "skip" is remembered.
    let h = fixture();
    h.setup(args(|_| {}), true, "4\n");
    let st = h.state();
    assert!(st.asked && st.mode.is_none());
    assert!(!should_ask_first_run(true, |_| None, Some(&st)));
}

#[test]
fn first_run_prompt_gating() {
    let none = |_: &str| None;
    assert!(should_ask_first_run(true, none, None));
    assert!(!should_ask_first_run(false, none, None), "never outside a terminal");
    assert!(!should_ask_first_run(true, |k| (k == "CI").then(|| "true".into()), None));
    assert!(!should_ask_first_run(true, |k| (k == "UNIFLO_NO_SETUP").then(|| "1".into()), None));
    assert!(should_ask_first_run(true, |k| (k == "UNIFLO_NO_SETUP").then(|| "0".into()), None));
    let declined = State { asked: true, ..Default::default() };
    assert!(!should_ask_first_run(true, none, Some(&declined)), "a refusal is remembered");
    let configured = State { mode: Some(Mode::Mcp), ..Default::default() };
    assert!(!should_ask_first_run(true, none, Some(&configured)));
}

/// Scenario: update 收尾 — never configured / configured / a new harness.
#[test]
fn after_update_three_states() {
    assert_eq!(after_update_decision(None, &["claude"]), AfterUpdate::Ask);
    let mut st = State { mode: Some(Mode::Mcp), asked: true, ..Default::default() };
    st.harnesses
        .insert("claude".into(), Rec { result: "connected".into(), mcp: Some(McpRec::Cli), ..Default::default() });
    st.harnesses
        .insert("codex".into(), Rec { result: "connected".into(), mcp: Some(McpRec::Cli), ..Default::default() });
    assert_eq!(after_update_decision(Some(&st), &["claude", "codex"]), AfterUpdate::Reload { new: vec![] });
    assert_eq!(
        after_update_decision(Some(&st), &["claude", "codex", "cursor"]),
        AfterUpdate::Reload { new: vec!["cursor"] }
    );
    st.harnesses.insert("cursor".into(), Rec { result: "declined".into(), ..Default::default() });
    assert_eq!(after_update_decision(Some(&st), &["claude", "codex", "cursor"]), AfterUpdate::Reload { new: vec![] });
    let declined = State { asked: true, ..Default::default() };
    assert_eq!(after_update_decision(Some(&declined), &["claude"]), AfterUpdate::Nothing);
}

/// The after-update run end to end: recorded entries follow the new executable silently, only
/// the new harness is asked about, and a "no" is remembered.
#[test]
fn after_update_refreshes_silently_and_asks_only_new() {
    let h = Home::new();
    h.write(".cursor/mcp.json", "{}");
    h.setup(args(|a| (a.mcp, a.yes) = (true, true)), false, "");
    assert_eq!(h.json(".cursor/mcp.json")["mcpServers"]["uniflo"]["command"], BIN);
    h.write(".kiro/settings/mcp.json", "{}");
    let mut ctx = h.ctx();
    ctx.bin = "/new/place/uniflo".into();
    let mut out = Vec::new();
    flow(&ctx, &args(|a| a.after_update = true), true, &mut "n\n".as_bytes(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert_eq!(h.json(".cursor/mcp.json")["mcpServers"]["uniflo"]["command"], "/new/place/uniflo");
    assert_eq!(out.matches("[Y/n]").count(), 1, "one question, about kiro only: {out}");
    assert!(out.contains("Kiro") && !out.contains("Cursor"), "{out}");
    assert_eq!(h.read(".kiro/settings/mcp.json"), "{}");
    assert_eq!(h.state().harnesses["kiro"].result, "declined");
    let mut out = Vec::new();
    flow(&ctx, &args(|a| a.after_update = true), true, &mut "".as_bytes(), &mut out).unwrap();
    assert!(!String::from_utf8(out).unwrap().contains("[Y/n]"), "not asked again");
}

#[test]
fn reload_points_entries_at_the_current_executable_and_adds_new_harnesses() {
    let h = Home::new();
    h.write(".cursor/mcp.json", "{}");
    h.setup(args(|a| (a.mcp, a.yes) = (true, true)), false, "");
    h.write(".omp/agent/mcp.json", "{}");
    let mut ctx = h.ctx();
    ctx.bin = "/moved/uniflo".into();
    let mut out = Vec::new();
    flow(&ctx, &args(|a| (a.reload, a.yes) = (true, true)), false, &mut "".as_bytes(), &mut out).unwrap();
    assert_eq!(h.json(".cursor/mcp.json")["mcpServers"]["uniflo"]["command"], "/moved/uniflo");
    assert_eq!(h.json(".omp/agent/mcp.json")["mcpServers"]["uniflo"]["command"], "/moved/uniflo");
}

#[test]
fn relative_links() {
    assert_eq!(
        relative(Path::new("/h/.claude/skills"), Path::new("/h/.agents/skills/uniflo")),
        PathBuf::from("../../.agents/skills/uniflo")
    );
    assert_eq!(
        relative(Path::new("/h/.pi/agent/skills"), Path::new("/h/.agents/skills/uniflo")),
        PathBuf::from("../../../.agents/skills/uniflo")
    );
}
