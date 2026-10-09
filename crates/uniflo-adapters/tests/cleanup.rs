//! Which harnesses declare cleanup targets (`docs/adapters.md#会话清理`): one-file-per-session
//! JSONL/JSON harnesses do, databases and shared files do not.

const SUPPORTED: &[&str] = &[
    "claude",
    "qoder",
    "qwen",
    "codex",
    "pi",
    "omp",
    "crosery",
    "commandcode",
    "prime",
    "workbuddy",
    "factory",
    "gemini",
    "antigravity",
    "reasonix",
    "cursor",
    "dsh",
];

const UNSUPPORTED: &[&str] = &["opencode", "kilo", "zcode", "mimocode", "minimax", "hermes", "cline", "roo", "kodu"];

#[test]
fn cleanup_support_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s/abc.events.jsonl");
    for a in uniflo_adapters::all() {
        let id = a.info().id;
        let t = a.cleanup_targets(&src, "abc");
        if SUPPORTED.contains(&id) {
            let t = t.unwrap_or_else(|| panic!("{id} should support cleanup"));
            assert!(t.iter().any(|p| src.starts_with(p)), "{id}: targets must cover the transcript: {t:?}");
        } else {
            assert!(UNSUPPORTED.contains(&id), "{id}: add it to SUPPORTED or UNSUPPORTED");
            assert!(t.is_none(), "{id} must not support cleanup");
        }
    }
}
