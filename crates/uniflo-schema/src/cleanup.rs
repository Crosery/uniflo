//! Session cleanup and archive responses: `POST /v1/cleanup/plan`,
//! `POST /v1/cleanup/plans/{id}/execute`, `GET /v1/archive`, `DELETE /v1/archive/{key}`
//! and the matching `uniflo clean` / `uniflo archive` `--json` output.

use serde::{Deserialize, Serialize};

/// Body of `POST /v1/cleanup/plan`: session keys, or a session query (`docs/search.md`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupRequest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

/// What a cleanup would do; executable once, until `expires_at`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub plan_id: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// One entry per requested session, in request order.
    pub sessions: Vec<CleanupCandidate>,
    /// Bytes the eligible sessions would free.
    pub freed_bytes: u64,
    /// Estimated size of their archives.
    pub archive_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupCandidate {
    pub key: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub eligible: bool,
    /// Stable code when not eligible (`unsupported`, `working`, `subagent`, …; see `docs/api.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable explanation of `reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Sub-agent sessions cleaned and archived together with this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    /// Files and directories moved to the trash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<CleanupTarget>,
    pub bytes: u64,
    /// Estimated archive size (the real size is in the execution result).
    pub archive_bytes: u64,
}

/// One file or directory as the plan saw it; execution refuses when any of this changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupTarget {
    pub path: String,
    pub dir: bool,
    /// Size of the file, or of every file under the directory.
    pub bytes: u64,
    pub files: u64,
    /// Modification time (newest file for a directory), Unix ms.
    pub mtime_ms: i64,
    /// Inode (Unix); absent where the platform has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    /// Archived, then moved to the trash.
    Archived,
    /// Nothing was moved (see `reason`); a partial move says so in `reason`.
    Failed,
    /// Not eligible in the plan.
    Skipped,
}

/// Result of `POST /v1/cleanup/plans/{id}/execute`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub plan_id: String,
    pub results: Vec<CleanupResult>,
    pub freed_bytes: u64,
    pub archive_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CleanupResult {
    pub key: String,
    pub status: CleanupStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Bytes moved to the trash.
    pub freed_bytes: u64,
    /// Bytes of the archives written (this session and its `children`).
    pub archive_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
}

/// `GET /v1/archive`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchiveList {
    pub archives: Vec<ArchiveEntry>,
    /// Sum of `bytes`.
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub key: String,
    pub harness: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Key of the session whose cleanup produced this archive, when it is one of its sub-agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// The archive file.
    pub path: String,
    pub bytes: u64,
    /// Where the transcript lived before the cleanup.
    pub source: String,
    /// Bytes the cleanup moved to the trash (root entries: the whole tree).
    pub source_bytes: u64,
    pub archived_at: i64,
    /// The source is back (restored from the trash); the session is served from it again.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub restored: bool,
}

/// `DELETE /v1/archive/{key}`: removing a root archive removes its sub-agents' archives too.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArchiveRemoved {
    pub removed: Vec<String>,
    pub bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_optional_fields() {
        let r = CleanupResult {
            key: "claude:a".into(),
            status: CleanupStatus::Failed,
            reason: Some("source_changed".into()),
            message: None,
            freed_bytes: 0,
            archive_bytes: 0,
            children: vec![],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["status"], "failed");
        assert!(v.get("children").is_none() && v.get("message").is_none());
        let req: CleanupRequest = serde_json::from_str(r#"{"q":"h:claude"}"#).unwrap();
        assert_eq!((req.sessions.len(), req.q.as_deref()), (0, Some("h:claude")));
    }
}
