//! Cursor agent transcripts, plus the Cursor IDE store (`cursor_ide.rs`).
//!
//! Layout: `~/.cursor/projects/[<slug>/]agent-transcripts/<chat>/<chat>.jsonl`. Records are
//! `{role, message:{content[...]}}` with no ids or timestamps, so events use position ids and
//! `ts = 0`. Tool calls carry no id (call ids are position-derived) and results are not logged;
//! a text-only assistant record ends the turn. The chat id is the IDE composer id: title, cwd,
//! model, times and sub-agent parent come from the IDE store's `composerData:<id>`. Composers
//! that kept their messages in the IDE store and have no transcript are listed from the store
//! (source = `state.vscdb`); composers without any message are not listed. A transcript is
//! read again when the store changes, so its metadata follows renames in the IDE.

use crate::common::under_any;
use crate::cursor_ide::{Ide, Sig, default_db, sig};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::util::{home, json_arg, str_of};
use uniflo_core::{
    Adapter, Cursor as SrcCursor, Cx, HarnessInfo, HistoryQuery, JsonlAdapter, LineDecoder, ReadOutput, Record,
    SourceId,
};
use uniflo_schema::{Body, Event};

pub struct Cursor {
    roots: Vec<PathBuf>,
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(CursorAgent::new(home().join(".cursor/projects"), default_db()))]
}

/// Agent transcripts (one file per chat) and the IDE store (many composers) as one harness.
pub struct CursorAgent {
    transcripts: JsonlAdapter<Cursor>,
    ide: Ide,
}

impl CursorAgent {
    pub fn new(projects: PathBuf, db: PathBuf) -> Self {
        CursorAgent { transcripts: JsonlAdapter::new(Cursor { roots: vec![projects] }), ide: Ide::new(db) }
    }

    fn is_ide(&self, src: &Path) -> bool {
        src == self.ide.db
    }

    fn transcript_ids(&self) -> HashSet<String> {
        self.transcripts.discover().iter().filter_map(|p| self.transcripts.decoder.identify(p)).map(|s| s.id).collect()
    }

    fn with_ide_meta(&self, src: &Path, records: &mut Vec<(String, Record)>) {
        let Some(id) = self.transcripts.decoder.identify(src).map(|s| s.id) else { return };
        if let Some(m) = self.ide.meta(&id) {
            records.push((id, Record::Meta(m)));
        }
    }
}

/// Transcript cursor: the transcript's own state plus the store signature its metadata was
/// read at.
#[derive(Serialize, Deserialize)]
struct TranscriptState {
    d: Value,
    ide: Sig,
}

fn split(c: &SrcCursor) -> (SrcCursor, Option<Sig>) {
    match serde_json::from_value::<TranscriptState>(c.state.clone()) {
        Ok(t) => (SrcCursor { state: t.d, ..c.clone() }, Some(t.ide)),
        Err(_) => (c.clone(), None),
    }
}

fn wrap(mut c: SrcCursor, ide: Sig) -> SrcCursor {
    let d = std::mem::take(&mut c.state);
    c.state = serde_json::to_value(TranscriptState { d, ide }).unwrap_or(Value::Null);
    c
}

impl Adapter for CursorAgent {
    fn info(&self) -> HarnessInfo {
        self.transcripts.info()
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.transcripts.roots()
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        crate::sqlite::source_for_db(&self.ide.db, path).or_else(|| self.transcripts.source_for(path))
    }

    fn discover(&self) -> Vec<PathBuf> {
        let mut out = self.transcripts.discover();
        if self.ide.db.is_file() {
            out.push(self.ide.db.clone());
        }
        out
    }

    fn read(&self, src: &Path, cursor: Option<&SrcCursor>) -> Result<ReadOutput> {
        if self.is_ide(src) {
            if let Some(c) = cursor.filter(|c| !self.ide.changed(c)) {
                return Ok(ReadOutput { cursor: c.clone(), ..Default::default() });
            }
            let (batch, c) = self.ide.read(&self.transcript_ids(), None)?;
            return Ok(ReadOutput { cursor: c, batch, summary: cursor.is_none(), reset: cursor.is_some() });
        }
        let inner = cursor.map(|c| split(c).0);
        let mut out = self.transcripts.read(src, inner.as_ref())?;
        let ide = sig(&self.ide.db);
        self.with_ide_meta(src, &mut out.batch.items);
        out.cursor = wrap(out.cursor, ide);
        Ok(out)
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        if !self.is_ide(src) {
            return self.transcripts.history(src, session_id, q);
        }
        let (batch, _) = self.ide.read(&HashSet::new(), Some(session_id))?;
        let mut evs: Vec<Event> = batch
            .items
            .into_iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                Record::Meta(_) => None,
            })
            .filter(|e| q.before.is_none_or(|b| e.pos.unwrap_or(0) < b))
            .collect();
        let skip = evs.len().saturating_sub(q.limit.max(1));
        Ok(evs.split_off(skip))
    }

    fn read_all(&self, src: &Path, sessions: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<SrcCursor> {
        if self.is_ide(src) {
            let (batch, c) = self.ide.read(&self.transcript_ids(), None)?;
            for (id, r) in batch.items {
                sink(&id, r);
            }
            return Ok(c);
        }
        let c = self.transcripts.read_all(src, sessions, sink)?;
        let ide = sig(&self.ide.db);
        let mut meta = Vec::new();
        self.with_ide_meta(src, &mut meta);
        for (id, r) in meta {
            sink(&id, r);
        }
        Ok(wrap(c, ide))
    }

    fn changed(&self, src: &Path, cursor: &SrcCursor) -> bool {
        if self.is_ide(src) {
            return self.ide.changed(cursor);
        }
        let (inner, ide) = split(cursor);
        self.transcripts.changed(src, &inner) || ide != Some(sig(&self.ide.db))
    }
}

/// Cursor wraps the human's prompt in `<user_query>` tags.
fn unwrap_query(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix("<user_query>").and_then(|r| r.strip_suffix("</user_query>")).map_or(s, str::trim)
}

impl LineDecoder for Cursor {
    type State = ();

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "cursor", name: "Cursor Agent" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { return false };
        p.extension().is_some_and(|e| e == "jsonl")
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == stem)
            && p.iter().any(|c| c == "agent-transcripts")
            && under_any(p, &self.roots)
    }

    /// agent-transcripts/<id>/ holds this chat's transcript only.
    fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> {
        uniflo_core::cleanup::targets::parent_dir(src)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let role = str_of(v, "role").unwrap_or("");
        let content = v.pointer("/message/content").unwrap_or(&Value::Null);
        let parts: Vec<Value> = match content {
            Value::String(s) => vec![serde_json::json!({"type":"text","text":s})],
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        match role {
            "user" => {
                let mut text = String::new();
                for p in &parts {
                    let piece = match str_of(p, "type").unwrap_or("") {
                        "text" => unwrap_query(str_of(p, "text").unwrap_or("")).to_owned(),
                        other => format!("[{other}]"),
                    };
                    if piece.is_empty() {
                        continue;
                    }
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&piece);
                }
                if !text.is_empty() {
                    cx.emit_at(0, Body::UserMessage { text, synthetic: false });
                }
            }
            "assistant" => {
                let (mut calls, mut texts) = (0, 0);
                for (i, p) in parts.iter().enumerate() {
                    let body = match str_of(p, "type").unwrap_or("") {
                        "text" => {
                            texts += 1;
                            Body::AssistantMessage { text: str_of(p, "text").unwrap_or("").to_owned(), model: None }
                        }
                        "tool_use" => {
                            calls += 1;
                            Body::ToolCall {
                                call_id: format!("c{}.{i}", cx.pos),
                                name: str_of(p, "name").unwrap_or("").to_owned(),
                                input: json_arg(p.get("input").unwrap_or(&Value::Null)),
                            }
                        }
                        other => {
                            cx.unknown(format!("assistant.part={other}"));
                            continue;
                        }
                    };
                    cx.emit_at(0, body);
                }
                if calls == 0 && texts > 0 {
                    cx.emit_at(0, Body::TurnEnd { reason: Some("stop".into()) });
                }
            }
            other => cx.unknown(format!("role={other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn cu(fx: &Fixture) -> JsonlAdapter<Cursor> {
        JsonlAdapter::new(Cursor { roots: vec![fx.root().join("projects")] })
    }

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    const P: &str = "projects/slug/agent-transcripts/chat1/chat1.jsonl";

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = cu(&fx);
        let mut s = l(
            json!({"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfix it\n</user_query>"}]}}),
        );
        s += &l(
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"looking"},{"type":"tool_use","name":"Shell","input":{"command":"ls"}}]}}),
        );
        s += &l(json!({"role":"assistant","message":{"content":[{"type":"text","text":"done"}]}}));
        let p = fx.write(P, &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "assistant_message", "tool_call", "assistant_message", "turn_end"]
        );
        assert_eq!(r.status(), Status::Idle);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert_eq!(r.preview.as_deref(), Some("fix it"));
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { name, input, call_id } if name == "Shell" && input["command"] == "ls" && !call_id.is_empty())
        );
        assert!(r.events.iter().all(|e| e.ts == 0));
        let ids: std::collections::HashSet<_> = r.events.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), r.events.len(), "position ids are unique");
    }

    #[test]
    fn mid_turn_is_work_and_unknown_reported() {
        let fx = Fixture::new();
        let a = cu(&fx);
        let mut s = l(json!({"role":"user","message":{"content":"plain"}}));
        s += &l(json!({"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}));
        s += &l(json!({"role":"assistant","message":{"content":[{"type":"mystery"}]}}));
        s += &l(json!({"role":"tool"}));
        let p = fx.write(P, &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.unknown, vec!["assistant.part=mystery".to_string(), "role=tool".to_string()]);
    }

    #[test]
    fn is_source_and_identity() {
        let fx = Fixture::new();
        let a = cu(&fx);
        let main = fx.write(P, "");
        let global = fx.write("projects/agent-transcripts/chat2/chat2.jsonl", "");
        let mismatch = fx.write("projects/slug/agent-transcripts/chat3/other.jsonl", "");
        let not_transcripts = fx.write("projects/slug/chat4/chat4.jsonl", "");
        let outside = fx.write("elsewhere/agent-transcripts/chat5/chat5.jsonl", "");
        for p in [&main, &global] {
            assert!(uniflo_core::Adapter::source_for(&a, p).is_some(), "{p:?}");
        }
        for p in [&mismatch, &not_transcripts, &outside] {
            assert!(uniflo_core::Adapter::source_for(&a, p).is_none(), "{p:?}");
        }
        assert_eq!(uniflo_core::Adapter::discover(&a).len(), 2);
        assert_eq!(a.decoder.identify(&main), Some(SourceId { id: "chat1".into(), parent: None }));
    }

    #[test]
    fn ide_store_fills_transcript_metadata_and_lists_ide_only_composers() {
        use crate::cursor_ide::tests::store;
        let fx = Fixture::new();
        let tid = "c-transcript";
        let p = fx.write(
            &format!("projects/slug/agent-transcripts/{tid}/{tid}.jsonl"),
            &(l(json!({"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}))
                + &l(json!({"role":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}))),
        );
        let db = fx.root().join("state.vscdb");
        let data = |name: &str, heads: Value| {
            json!({"composerId":"x","name":name,"createdAt":1790000000000i64,"lastUpdatedAt":1790000900000i64,
                "modelConfig":{"modelName":"gpt-5"},"workspaceIdentifier":{"id":"w","uri":{"fsPath":"/w/app"}},
                "fullConversationHeadersOnly":heads})
        };
        let bubble = |extra: Value| {
            let mut b = json!({"createdAt":"2026-09-21T14:13:30Z"});
            b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b
        };
        let heads = json!([{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2},{"bubbleId":"b3","type":1},{"bubbleId":"b4","type":2}]);
        let kv = vec![
            (format!("composerData:{tid}"), data("Transcript chat", json!([]))),
            ("composerData:c-ide".to_owned(), data("Old chat", heads)),
            ("composerData:c-empty".to_owned(), data("Draft", json!([]))),
            // Key order differs from conversation order on purpose.
            ("bubbleId:c-ide:b4".to_owned(), bubble(json!({"type":2,"text":"second answer"}))),
            (
                "bubbleId:c-ide:b2".to_owned(),
                bubble(json!({"type":2,"text":"first answer","thinking":{"text":"hmm"},
                "tokenCount":{"inputTokens":40,"outputTokens":9},"modelInfo":{"modelName":"claude-4"}})),
            ),
            ("bubbleId:c-ide:b1".to_owned(), bubble(json!({"type":1,"text":"first question"}))),
            ("bubbleId:c-ide:b3".to_owned(), bubble(json!({"type":1,"text":"second question"}))),
        ];
        store(&db, &kv, &[(tid, 1, json!({"subagentInfo":{"parentComposerId":"c-ide"}})), ("c-ide", 0, json!({}))]);
        let a = CursorAgent::new(fx.root().join("projects"), db.clone());
        let mut found = a.discover();
        found.sort();
        assert_eq!(found, vec![p.clone(), db.clone()]);

        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events), ["user_message", "assistant_message", "turn_end"]);
        assert_eq!(r.meta.title.as_deref(), Some("Transcript chat"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/app"));
        assert_eq!(r.meta.model.as_deref(), Some("gpt-5"));
        assert_eq!(r.meta.started_at, Some(1790000000000));
        assert_eq!(r.meta.updated_at, Some(1790000900000));
        assert_eq!(r.meta.parent.as_deref(), Some("c-ide"));

        // Renamed in the IDE: only the store changes, the transcript is read again for it.
        let first = a.read(&p, None).unwrap();
        assert!(!a.changed(&p, &first.cursor));
        let w = rusqlite::Connection::open(&db).unwrap();
        w.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![data("Renamed chat", json!([])).to_string(), format!("composerData:{tid}")],
        )
        .unwrap();
        drop(w);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&db).unwrap().set_modified(later).unwrap();
        assert!(a.changed(&p, &first.cursor));
        let renamed = crate::common::testkit::group(a.read(&p, Some(&first.cursor)).unwrap().batch);
        assert_eq!(renamed[tid].meta.title.as_deref(), Some("Renamed chat"));
        assert!(renamed[tid].events.is_empty(), "nothing new in the transcript");

        let out = a.read(&db, None).unwrap();
        let g = crate::common::testkit::group(out.batch);
        assert_eq!(
            g.keys().collect::<Vec<_>>(),
            ["c-ide"],
            "transcript and empty composers are not listed from the store"
        );
        let ide = &g["c-ide"];
        assert_eq!(
            kinds(&ide.events),
            [
                "user_message",
                "reasoning",
                "assistant_message",
                "usage",
                "turn_end",
                "user_message",
                "assistant_message",
                "turn_end"
            ]
        );
        let texts: Vec<&str> = ide
            .events
            .iter()
            .filter_map(|e| match &e.body {
                Body::UserMessage { text, .. } | Body::AssistantMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["first question", "first answer", "second question", "second answer"]);
        assert!(
            matches!(&ide.events[3].body, Body::Usage(u) if (u.input, u.output) == (40, 9) && u.model.as_deref() == Some("claude-4"))
        );
        assert_eq!(ide.meta.title.as_deref(), Some("Old chat"));
        assert_eq!(ide.status(), uniflo_schema::Status::Idle);
        assert!(!a.changed(&db, &out.cursor), "unchanged store is not re-read");
        let again = a.read(&db, Some(&out.cursor)).unwrap();
        assert!(again.batch.items.is_empty());
        let h = a.history(&db, "c-ide", &HistoryQuery { before: Some(3), limit: 10 }).unwrap();
        assert_eq!(kinds(&h), ["user_message", "reasoning", "assistant_message", "usage", "turn_end"]);
    }
}
