//! Agent-access responses: resume commands (`GET /v1/sessions/{key}/resume`), agent memory and
//! instruction files (`GET /v1/memory`, `GET /v1/memory/file`) and the hand-off context of
//! `uniflo context --json` / the `uniflo_context` MCP tool.

use serde::{Deserialize, Serialize};

/// How to continue a session in its own harness. Nothing is executed by the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeInfo {
    pub key: String,
    pub harness: String,
    pub supported: bool,
    /// Program and arguments, run from `cwd`. Empty when not supported.
    #[serde(default)]
    pub argv: Vec<String>,
    /// The session's working directory, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// One POSIX shell line (`cd <cwd> && <argv>` when the harness resumes per directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The same for PowerShell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_powershell: Option<String>,
    /// Why the session cannot be resumed, when `supported` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Per-user instructions every project of a harness loads.
    Global,
    /// Files belonging to one project (instruction files between cwd and the git root, Claude
    /// project memory).
    Project,
}

/// One agent memory / instruction file (`GET /v1/memory`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFile {
    pub path: String,
    pub scope: MemoryScope,
    /// Harness that loads the file; `agents` for the shared `AGENTS.md` convention.
    pub harness: String,
    pub bytes: u64,
    /// File modification time, Unix ms.
    pub updated_at: i64,
}

/// `GET /v1/memory/file`: one listed file with its text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryContent {
    #[serde(flatten)]
    pub file: MemoryFile,
    /// UTF-8 text (invalid sequences replaced), at most 256 KB.
    pub content: String,
    /// The file is larger than the returned `content`.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub truncated: bool,
}

/// Recent sessions of one project (`uniflo context --json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextReport {
    /// Git root of the requested directory (the directory itself outside a repository).
    pub project: String,
    /// Only sessions active at or after this time (Unix ms).
    pub since: i64,
    /// Newest first.
    pub sessions: Vec<ContextSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSession {
    pub key: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub updated_at: i64,
    /// First human prompt, at most 80 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// API-equivalent cost of the session so far; `null` when unknown or unpriced.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resume_and_memory_wire_shapes() {
        let r = ResumeInfo {
            key: "dsh:x".into(),
            harness: "dsh".into(),
            supported: false,
            argv: vec![],
            cwd: None,
            command: None,
            command_powershell: None,
            reason: Some("unsupported".into()),
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"key":"dsh:x","harness":"dsh","supported":false,"argv":[],"reason":"unsupported"})
        );
        let m = MemoryContent {
            file: MemoryFile {
                path: "/p/AGENTS.md".into(),
                scope: MemoryScope::Project,
                harness: "agents".into(),
                bytes: 3,
                updated_at: 1,
            },
            content: "abc".into(),
            truncated: false,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v,
            json!({"path":"/p/AGENTS.md","scope":"project","harness":"agents","bytes":3,"updated_at":1,"content":"abc"})
        );
        assert_eq!(serde_json::from_value::<MemoryContent>(v).unwrap(), m);
    }
}
