//! On-disk ledger cache (`usage-v1.json` next to the index cache): per source, the ledger
//! cursor plus every step and prompt, so a restart only reads what was appended since.

use super::{Ledger, Phase, Prompt, SourceLedger, Step, UsageIndex};
use crate::adapter::Cursor;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
struct File {
    tag: String,
    sources: Vec<StoredSource>,
}

#[derive(Serialize, Deserialize)]
struct StoredSource {
    harness: String,
    path: PathBuf,
    cursor: Cursor,
    /// Model names referenced by index (+1) from steps; 0 = none.
    models: Vec<String>,
    sessions: Vec<StoredLedger>,
}

/// `[event, ts, turn, model, input, output, cache_read, cache_write, reasoning, reported]`
type StoredStep = (String, i64, u32, u32, u64, u64, u64, u64, u64, Option<f64>);

#[derive(Serialize, Deserialize)]
struct StoredLedger {
    key: String,
    model: u32,
    turn: u32,
    open: bool,
    steps: Vec<StoredStep>,
    /// `[id hash, ts, turn]`
    prompts: Vec<(u64, i64, u32)>,
    seen: Vec<u64>,
}

pub fn path_for(index_cache: &Path) -> PathBuf {
    index_cache.with_file_name("usage-v1.json")
}

pub fn save(path: &Path, tag: &str, index: &UsageIndex) -> Result<()> {
    let mut sources = Vec::new();
    for (p, s) in index.sources() {
        let (Phase::Ready, Some(cursor)) = (s.phase, &s.cursor) else { continue };
        let mut models: Vec<String> = Vec::new();
        let mut ids: HashMap<Arc<str>, u32> = HashMap::new();
        let mut idx = |m: Option<&Arc<str>>| -> u32 {
            let Some(m) = m else { return 0 };
            *ids.entry(m.clone()).or_insert_with(|| {
                models.push(m.to_string());
                models.len() as u32
            })
        };
        let mut sessions = Vec::new();
        for (k, l) in &s.ledgers {
            let model = idx(l.model.as_ref());
            let steps = l
                .steps
                .iter()
                .map(|x| {
                    let m = idx(x.model.as_ref());
                    (
                        x.event.to_string(),
                        x.ts,
                        x.turn,
                        m,
                        x.input,
                        x.output,
                        x.cache_read,
                        x.cache_write,
                        x.reasoning,
                        x.reported,
                    )
                })
                .collect();
            sessions.push(StoredLedger {
                key: k.clone(),
                model,
                turn: l.turn,
                open: l.is_open(),
                steps,
                prompts: l.prompts.iter().map(|p| (p.id, p.ts, p.turn)).collect(),
                seen: l.seen_ids().collect(),
            });
        }
        sources.push(StoredSource {
            harness: s.harness.clone(),
            path: p.clone(),
            cursor: cursor.clone(),
            models,
            sessions,
        });
    }
    let file = File { tag: tag.to_owned(), sources };
    crate::pricing::sync::write_atomic(path, &serde_json::to_vec(&file)?)?;
    Ok(())
}

/// A restored source: harness id, ledger cursor, ledgers by session key.
pub type Restored = (String, Cursor, HashMap<String, Ledger>);

/// Sources from a cache written with the same `tag`; empty otherwise.
pub fn load(path: &Path, tag: &str) -> HashMap<PathBuf, Restored> {
    let mut sources = HashMap::new();
    let Ok(bytes) = std::fs::read(path) else { return sources };
    let Ok(f) = serde_json::from_slice::<File>(&bytes) else { return sources };
    if f.tag != tag {
        return sources;
    }
    for s in f.sources {
        let names: Vec<Arc<str>> = s.models.iter().map(|m| Arc::from(m.as_str())).collect();
        let name = |i: u32| (i > 0).then(|| names.get(i as usize - 1).cloned()).flatten();
        let mut ledgers = HashMap::new();
        for l in s.sessions {
            let steps = l
                .steps
                .into_iter()
                .map(|(event, ts, turn, m, input, output, cache_read, cache_write, reasoning, reported)| Step {
                    event: event.into(),
                    ts,
                    turn,
                    model: name(m),
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                    reported,
                    cost: None,
                    source: None,
                })
                .collect();
            let prompts = l.prompts.into_iter().map(|(id, ts, turn)| Prompt { id, ts, turn }).collect();
            ledgers.insert(l.key, Ledger::restore(steps, prompts, l.seen, name(l.model), l.turn, l.open));
        }
        sources.insert(s.path, (s.harness, s.cursor, ledgers));
    }
    sources
}

/// Install a loaded source into `index` as ready.
pub fn install(
    index: &mut UsageIndex,
    path: PathBuf,
    adapter: usize,
    harness: String,
    cursor: Cursor,
    ledgers: HashMap<String, Ledger>,
) {
    index.put_source(path, SourceLedger { adapter, harness, cursor: Some(cursor), phase: Phase::Ready, ledgers });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Record;
    use crate::pricing::Pricing;
    use uniflo_schema::{Body, Event, Usage};

    #[test]
    fn roundtrip_keeps_steps_prompts_and_dedupe() {
        let p = Pricing::new(None).current();
        let mut l = Ledger::default();
        let mut n = super::super::Names::default();
        let ev = |id: &str, body: Body| {
            Record::Event(Event {
                id: id.into(),
                session: "t:1".into(),
                ts: 5,
                pos: None,
                partial: false,
                truncated: false,
                body,
            })
        };
        l.apply(&ev("ts", Body::TurnStart {}), &mut n, &p);
        l.apply(&ev("u", Body::UserMessage { text: "q".into(), synthetic: false }), &mut n, &p);
        l.apply(
            &ev("s", Body::Usage(Usage { input: 7, model: Some("gpt-5".into()), ..Default::default() })),
            &mut n,
            &p,
        );
        let mut ix = UsageIndex::default();
        let cursor = Cursor { offset: 9, ..Default::default() };
        install(&mut ix, "/src".into(), 0, "t".into(), cursor, HashMap::from([("t:1".to_owned(), l)]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-v1.json");
        save(&path, "tag", &ix).unwrap();
        assert!(load(&path, "other").is_empty(), "tag mismatch ignored");
        let mut sources = load(&path, "tag");
        let src = sources.get_mut(Path::new("/src")).unwrap();
        assert_eq!(src.1.offset, 9);
        let l = src.2.get_mut("t:1").unwrap();
        assert_eq!((l.steps.len(), l.prompts.len(), l.turn), (1, 1, 1));
        assert_eq!(l.steps[0].model.as_deref(), Some("gpt-5"));
        // Re-delivered events stay deduplicated after a restore.
        l.apply(&ev("ts", Body::TurnStart {}), &mut n, &p);
        l.apply(&ev("u", Body::UserMessage { text: "q".into(), synthetic: false }), &mut n, &p);
        l.apply(&ev("s", Body::Usage(Usage { input: 8, ..Default::default() })), &mut n, &p);
        assert_eq!((l.steps.len(), l.prompts.len(), l.turn, l.steps[0].input), (1, 1, 1, 8));
        assert_eq!(l.steps[0].model.as_deref(), Some("gpt-5"), "upsert without model keeps the old one");
    }
}
