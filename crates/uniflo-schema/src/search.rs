//! Full-text search responses (`GET /v1/search`, `uniflo grep --json`).

use serde::{Deserialize, Serialize};

/// Opens a highlighted range inside [`SearchHit::snippet`].
pub const HIGHLIGHT_START: char = '\u{2}';
/// Closes a highlighted range inside [`SearchHit::snippet`].
pub const HIGHLIGHT_END: char = '\u{3}';

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub q: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    pub order: SearchOrder,
    /// Matching sessions before `offset` / `limit`.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    /// The index is still being built or caught up: results may be incomplete.
    pub indexing: bool,
    pub progress: IndexProgress,
    pub results: Vec<SearchSession>,
    /// The substring scan of a short-term query ran out of time: `results` are the newest matches
    /// it found, older ones may be missing.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub partial: bool,
    /// The substring scan stopped before the oldest sessions (enough results, or `partial`):
    /// sessions last active at or before this time (Unix ms) may not have been searched, and
    /// `total` is a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchOrder {
    /// bm25 × recency weight, best first.
    Relevance,
    /// A term shorter than 3 characters forced a substring scan: newest first.
    Recent,
}

/// Hits of one session; `session` is the session key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchSession {
    pub session: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    /// Best hit score, higher is better; `0` in [`SearchOrder::Recent`].
    pub score: f64,
    /// At most three, best first.
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Event id: open it with `GET /v1/sessions/{key}/events?around=<event>`.
    pub event: String,
    pub kind: String,
    pub ts: i64,
    /// Text around the match; highlighted ranges are wrapped in [`HIGHLIGHT_START`] / [`HIGHLIGHT_END`].
    pub snippet: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexProgress {
    /// Sessions whose content is fully indexed.
    pub done: usize,
    pub total: usize,
    /// Indexed events.
    pub events: u64,
}

/// Full-text index state for `GET /v1/stats` (`fts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FtsStatus {
    pub indexing: bool,
    pub progress: IndexProgress,
    pub path: String,
    /// Size of the index files on disk.
    pub bytes: u64,
    /// The index was discarded at startup (format tag changed or file unreadable).
    pub rebuilt: bool,
    /// After startup, the index is still being merged into one segment and read into the OS page
    /// cache: until then a term's first query may wait on the disk.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub warming: bool,
    /// Duration of the last backfill that started from an empty index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ms: Option<u64>,
    pub errors: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Tiered storage (ADR-0011): sessions active within this many days are kept in the hot
    /// (ADR-0008) format; older ones are compressed into the cold tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_days: Option<u32>,
    /// Event rows currently in the hot tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_events: Option<u64>,
    /// Event rows currently in the cold tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_events: Option<u64>,
    /// Compressed blocks currently in the cold tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_blocks: Option<u64>,
    /// Converting a leftover `fts-v1.sqlite` into the tiered format.
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub migrating: bool,
    /// Bytes reclaimed by deleting the old-format file after this start's migration (0 if none).
    #[serde(default)]
    pub reclaimed_bytes: u64,
    /// Times another process changed the index file while this one held the write lease (an older
    /// Uniflo that ignores the lease, or a stray SQL client). Not counted in `errors`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub foreign_writes: u64,
    /// Other processes that currently have the index file open for writing, best effort (empty
    /// when none or when the platform cannot tell).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_writers: Vec<FtsWriter>,
}

/// A process other than this one that has the full-text index file open for writing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtsWriter {
    pub pid: u32,
    /// Executable and subcommand, e.g. `/usr/local/bin/uniflo mcp`; no other arguments.
    pub command: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fts_status_foreign_fields_are_added_only() {
        let mut v = json!({"indexing":false,"progress":{"done":1,"total":1,"events":2},"path":"p","bytes":3,
            "rebuilt":false,"errors":0});
        let st: FtsStatus = serde_json::from_value(v.clone()).unwrap();
        assert_eq!((st.foreign_writes, st.foreign_writers.len()), (0, 0));
        let out = serde_json::to_value(&st).unwrap();
        assert!(out.get("foreign_writes").is_none() && out.get("foreign_writers").is_none(), "{out}");
        v["foreign_writes"] = json!(2);
        v["foreign_writers"] = json!([{"pid":7,"command":"uniflo mcp"}]);
        let st: FtsStatus = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&st).unwrap()["foreign_writers"][0]["pid"], json!(7));
    }

    #[test]
    fn search_response_wire_shape() {
        let r = SearchResponse {
            q: "缓存击穿".into(),
            filter: None,
            order: SearchOrder::Relevance,
            total: 1,
            offset: 0,
            limit: 20,
            indexing: true,
            progress: IndexProgress { done: 1, total: 2, events: 9 },
            results: vec![SearchSession {
                session: "claude:s1".into(),
                harness: "claude".into(),
                title: Some("t".into()),
                cwd: None,
                updated_at: Some(5),
                score: 1.5,
                hits: vec![SearchHit {
                    event: "u1".into(),
                    kind: "user_message".into(),
                    ts: 5,
                    snippet: "修复\u{2}缓存击穿\u{3}问题".into(),
                }],
            }],
            partial: false,
            scanned_until: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            json!({"q":"缓存击穿","order":"relevance","total":1,"offset":0,"limit":20,"indexing":true,
                "progress":{"done":1,"total":2,"events":9},
                "results":[{"session":"claude:s1","harness":"claude","title":"t","updated_at":5,"score":1.5,
                    "hits":[{"event":"u1","kind":"user_message","ts":5,"snippet":"修复\u{2}缓存击穿\u{3}问题"}]}]})
        );
        assert_eq!(serde_json::from_value::<SearchResponse>(v).unwrap(), r);

        let cut = SearchResponse { partial: true, scanned_until: Some(4), ..r };
        let v = serde_json::to_value(&cut).unwrap();
        assert_eq!((&v["partial"], &v["scanned_until"]), (&json!(true), &json!(4)));
    }
}
