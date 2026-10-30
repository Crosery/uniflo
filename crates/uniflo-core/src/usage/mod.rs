//! Per-step usage ledgers: every `usage` event of every session, with its model, turn and
//! cost, plus the human prompts — the data behind `Session.usage`, `/v1/usage`,
//! `/v1/sessions/{key}/usage` and `/v1/models`.
//!
//! Ledgers are fed by complete, in-order reads of a source ([`crate::Adapter::read_all`]
//! once, then [`crate::Adapter::read`] follows from the ledger's own cursor). Events with an
//! id already seen replace the earlier step, so overlapping reads never double count.
//! Costs are derived from the current price view and recomputed when it changes; they
//! depend only on price segments and event time, so a rebuild yields the same numbers.
//!
//! Ledgers belong to the source that produced them. A session key held by more than one
//! source (colliding harness ids, e.g. a Claude sub-agent and a workflow agent with the
//! same `agent-<id>`) is answered from all of them combined, independent of read order.

pub mod report;
pub mod store;
pub mod tz;

use crate::adapter::{Cursor, Record};
use crate::pricing::Loaded;
use crate::pricing::catalog::Tokens;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_schema::{
    Body, CostParts, CostSource, Event, ModelUsage, SessionUsage, SessionUsageDetail, StepPrice, StepUsage, TurnUsage,
};

/// Bump when ledger semantics change so cached ledgers are rebuilt.
pub const LEDGER_VERSION: u32 = 1;

/// Stable 64-bit FNV-1a: event ids are hashed into persisted dedupe sets, so the hash must
/// not depend on the Rust version.
pub fn id_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Where the event id sits in the owning ledger's id arena; read it with [`Ledger::event_id`].
    pub(crate) event: (u32, u32),
    pub ts: i64,
    pub turn: u32,
    pub model: Option<Arc<str>>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Amount reported by the harness.
    pub reported: Option<f64>,
    /// Derived from `reported` or the price view of [`Ledger::priced_at`].
    pub cost: Option<f64>,
    pub source: Option<CostSource>,
}

impl Step {
    pub fn context_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    fn tokens(&self) -> Tokens {
        Tokens { input: self.input, output: self.output, cache_read: self.cache_read, cache_write: self.cache_write }
    }

    /// The split of a catalog-priced step's amount and the unit prices behind it, from the
    /// same lookup that priced it. Harness-reported and unpriced steps have none.
    fn breakdown(&self, p: &Loaded) -> Option<(CostParts, StepPrice)> {
        if !matches!(self.source, Some(CostSource::Catalog | CostSource::Approx)) {
            return None;
        }
        let t = self.tokens();
        let (r, _) = p.rates(self.model.as_deref()?, self.ts, t)?;
        let [input, output, cache_read, cache_write] = r.parts(t);
        Some((
            CostParts { input, output, cache_read, cache_write },
            StepPrice {
                input: r.input,
                output: r.output,
                cache_read: r.cache_read,
                cache_write: r.cache_write,
                tier_above: r.tier_above,
            },
        ))
    }

    fn price(&mut self, p: &Loaded) {
        (self.cost, self.source) = match self.reported.filter(|c| c.is_finite() && *c > 0.0) {
            Some(c) => (Some(c), Some(CostSource::Harness)),
            None => match self.model.as_deref().and_then(|m| p.price(m, self.ts, self.tokens())) {
                Some((c, s)) => (Some(c), Some(s)),
                // Nothing was billed, so there is nothing to price: an unknown model name on a
                // step without tokens is not an unpriced step.
                None if self.tokens() == Tokens::default() && self.reasoning == 0 => (Some(0.0), None),
                None => (None, None),
            },
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prompt {
    pub id: u64,
    pub ts: i64,
    pub turn: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Step(u32),
    Seen,
}

/// Usage of one session, in source order.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    pub steps: Vec<Step>,
    /// Event ids of `steps`, back to back: one allocation per ledger instead of one per step,
    /// which would pin heap pages long after a build's temporary data is freed.
    events: String,
    pub prompts: Vec<Prompt>,
    ids: HashMap<u64, Slot>,
    /// Model of the latest assistant message / metadata: fills steps that carry none.
    pub model: Option<Arc<str>>,
    pub turn: u32,
    /// A turn boundary was seen and nothing happened since (merges `turn_start` + prompt).
    open: bool,
    /// Pricing generation the step costs were computed with.
    pub priced_at: u64,
}

/// Interns model names within one build so steps share their strings.
#[derive(Default)]
pub struct Names(HashMap<String, Arc<str>>);

impl Names {
    pub fn get(&mut self, s: &str) -> Arc<str> {
        if let Some(a) = self.0.get(s) {
            return a.clone();
        }
        let a: Arc<str> = Arc::from(s);
        self.0.insert(s.to_owned(), a.clone());
        a
    }
}

impl Ledger {
    /// An empty ledger whose (no) steps count as priced with `generation`.
    pub fn priced(generation: u64) -> Ledger {
        Ledger { priced_at: generation, ..Default::default() }
    }

    /// Apply one record. Returns whether totals may have changed.
    pub fn apply(&mut self, rec: &Record, names: &mut Names, p: &Loaded) -> bool {
        match rec {
            Record::Meta(m) => {
                if let Some(model) = m.model.as_deref().filter(|s| !s.is_empty()) {
                    self.model = Some(names.get(model));
                }
                false
            }
            Record::Event(e) => self.event(e, names, p),
        }
    }

    fn boundary(&mut self, id: u64) -> bool {
        if self.ids.contains_key(&id) {
            return false;
        }
        self.ids.insert(id, Slot::Seen);
        if !self.open {
            self.turn += 1;
            self.open = true;
        }
        true
    }

    fn event(&mut self, e: &Event, names: &mut Names, p: &Loaded) -> bool {
        match &e.body {
            Body::UserMessage { synthetic: false, .. } => {
                let id = id_hash(&e.id);
                if self.boundary(id) {
                    self.prompts.push(Prompt { id, ts: e.ts, turn: self.turn });
                    return true;
                }
                false
            }
            Body::TurnStart {} => {
                self.boundary(id_hash(&e.id));
                false
            }
            Body::Usage(u) => {
                self.open = false;
                let id = id_hash(&e.id);
                let model = u.model.as_deref().filter(|s| !s.is_empty()).map(|m| names.get(m));
                let mut step = Step {
                    event: (0, 0),
                    ts: e.ts,
                    turn: self.turn,
                    model: model.or_else(|| self.model.clone()),
                    input: u.input,
                    output: u.output,
                    cache_read: u.cache_read,
                    cache_write: u.cache_write,
                    reasoning: u.reasoning,
                    reported: u.cost_usd,
                    cost: None,
                    source: None,
                };
                step.price(p);
                match self.ids.get(&id) {
                    Some(Slot::Step(i)) => {
                        let old = &mut self.steps[*i as usize];
                        step.event = old.event;
                        step.turn = old.turn;
                        if u.model.is_none() {
                            step.model = old.model.clone().or(step.model);
                            step.price(p);
                        }
                        *old = step;
                    }
                    _ => {
                        step.event = (self.events.len() as u32, e.id.len() as u32);
                        self.events.push_str(&e.id);
                        self.ids.insert(id, Slot::Step(self.steps.len() as u32));
                        self.steps.push(step);
                    }
                }
                true
            }
            Body::AssistantMessage { model, .. } => {
                if let Some(m) = model.as_deref().filter(|s| !s.is_empty()) {
                    self.model = Some(names.get(m));
                }
                self.open = false;
                false
            }
            Body::Reasoning { .. } | Body::ToolCall { .. } | Body::ToolResult { .. } | Body::TurnEnd { .. } => {
                self.open = false;
                false
            }
            Body::UserMessage { .. } | Body::System { .. } => false,
        }
    }

    /// Recompute step costs for a new price view.
    pub fn reprice(&mut self, p: &Loaded, generation: u64) {
        for s in &mut self.steps {
            s.price(p);
        }
        self.priced_at = generation;
    }

    pub fn summary(&self, p: &Loaded) -> Option<SessionUsage> {
        let last = self.steps.last()?;
        let mut u = SessionUsage {
            steps: self.steps.len() as u64,
            last_context_tokens: last.context_tokens(),
            context_limit: last.model.as_deref().and_then(|m| p.resolve(m).entry?.context_limit),
            ..Default::default()
        };
        for s in &self.steps {
            u.input += s.input;
            u.output += s.output;
            u.cache_read += s.cache_read;
            u.cache_write += s.cache_write;
            u.reasoning += s.reasoning;
            match s.cost {
                Some(c) => u.cost_usd = Some(u.cost_usd.unwrap_or(0.0) + c),
                None => u.unpriced_steps += 1,
            }
        }
        Some(u)
    }

    pub fn detail(&self, key: &str, p: &Loaded) -> SessionUsageDetail {
        let mut limits: HashMap<&str, Option<u64>> = HashMap::new();
        let steps: Vec<StepUsage> =
            self.steps
                .iter()
                .map(|s| {
                    let limit = s.model.as_deref().and_then(|m| {
                        *limits.entry(m).or_insert_with(|| p.resolve(m).entry.and_then(|e| e.context_limit))
                    });
                    let ctx = s.context_tokens();
                    let (cost_parts, price) = s.breakdown(p).unzip();
                    StepUsage {
                        event: self.event_id(s).to_owned(),
                        ts: s.ts,
                        turn: s.turn,
                        model: s.model.as_deref().map(str::to_owned),
                        input: s.input,
                        output: s.output,
                        cache_read: s.cache_read,
                        cache_write: s.cache_write,
                        reasoning: s.reasoning,
                        cost_usd: s.cost,
                        cost_source: s.source,
                        cost_parts,
                        price,
                        context_tokens: ctx,
                        context_limit: limit,
                        context_pct: limit.filter(|l| *l > 0).map(|l| ctx as f64 * 100.0 / l as f64),
                    }
                })
                .collect();
        let mut turns: Vec<TurnUsage> = Vec::new();
        let mut at: HashMap<u32, usize> = HashMap::new();
        let mut slot = |turn: u32, ts: i64, turns: &mut Vec<TurnUsage>| {
            let i = *at.entry(turn).or_insert_with(|| {
                turns.push(TurnUsage { turn, started_at: ts, ..Default::default() });
                turns.len() - 1
            });
            if ts > 0 && (turns[i].started_at == 0 || ts < turns[i].started_at) {
                turns[i].started_at = ts;
            }
            i
        };
        for pr in &self.prompts {
            let i = slot(pr.turn, pr.ts, &mut turns);
            turns[i].prompts += 1;
        }
        for s in &self.steps {
            let i = slot(s.turn, s.ts, &mut turns);
            let t = &mut turns[i];
            t.steps += 1;
            t.input += s.input;
            t.output += s.output;
            t.cache_read += s.cache_read;
            t.cache_write += s.cache_write;
            t.reasoning += s.reasoning;
            match s.cost {
                Some(c) => t.cost_usd = Some(t.cost_usd.unwrap_or(0.0) + c),
                None => t.unpriced_steps += 1,
            }
        }
        turns.sort_by_key(|t| t.turn);
        SessionUsageDetail { session: key.to_owned(), steps, turns, totals: self.summary(p).unwrap_or_default() }
    }

    pub(crate) fn seen_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.ids.iter().filter(|(_, s)| **s == Slot::Seen).map(|(k, _)| *k)
    }

    /// Rebuild from persisted parts (see [`store`]).
    pub(crate) fn restore(
        steps: Vec<Step>,
        events: String,
        prompts: Vec<Prompt>,
        seen: impl IntoIterator<Item = u64>,
        model: Option<Arc<str>>,
        turn: u32,
        open: bool,
    ) -> Ledger {
        let mut ids: HashMap<u64, Slot> = seen.into_iter().map(|k| (k, Slot::Seen)).collect();
        let mut l = Ledger { steps, events, prompts, ids: HashMap::new(), model, turn, open, priced_at: 0 };
        for (i, s) in l.steps.iter().enumerate() {
            ids.insert(id_hash(l.event_id(s)), Slot::Step(i as u32));
        }
        l.ids = ids;
        l.shrink();
        l
    }

    /// The event id of `s`, a step of this ledger.
    pub fn event_id(&self, s: &Step) -> &str {
        let (at, len) = (s.event.0 as usize, s.event.1 as usize);
        self.events.get(at..at + len).unwrap_or_default()
    }

    /// Drop the spare capacity growing by doubling left behind; ledgers live as long as the
    /// daemon, and a full build leaves a quarter of their memory unused on average.
    pub fn shrink(&mut self) {
        refit(&mut self.steps);
        if self.events.capacity() > self.events.len() {
            self.events = self.events.as_str().to_owned();
        }
        refit(&mut self.prompts);
        self.ids.shrink_to_fit();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// One view over several ledgers of the same session key: steps and prompts by time,
    /// each part's turn numbers shifted past the previous part's.
    pub fn combine(parts: &[&Ledger]) -> Ledger {
        let mut parts = parts.to_vec();
        parts.sort_by_key(|l| l.steps.first().map(|s| s.ts).or(l.prompts.first().map(|p| p.ts)).unwrap_or(0));
        let mut out = Ledger::default();
        let mut base = 0;
        for l in parts {
            let at = out.events.len() as u32;
            out.events.push_str(&l.events);
            out.steps.extend(l.steps.iter().map(|s| Step {
                event: (s.event.0 + at, s.event.1),
                turn: s.turn + base,
                ..s.clone()
            }));
            out.prompts.extend(l.prompts.iter().map(|p| Prompt { turn: p.turn + base, ..*p }));
            base += l.turn;
            out.priced_at = out.priced_at.max(l.priced_at);
        }
        out.steps.sort_by_key(|s| s.ts);
        out.prompts.sort_by_key(|p| p.ts);
        out.turn = base;
        out
    }
}

/// Moves `v` into an allocation of exactly its length. `shrink_to_fit` goes through `realloc`,
/// which macOS answers by keeping the whole block unless it would shrink by more than half.
fn refit<T>(v: &mut Vec<T>) {
    if v.capacity() > v.len() {
        let mut exact = Vec::with_capacity(v.len());
        exact.append(v);
        *v = exact;
    }
}

/// Where a source stands in the background build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for (or running) a full read.
    Queued,
    Ready,
}

#[derive(Debug, Clone)]
pub struct SourceLedger {
    pub adapter: usize,
    pub harness: String,
    /// Resume point of the ledger's own reads (independent of the engine's cursor).
    pub cursor: Option<Cursor>,
    pub phase: Phase,
    /// Session key → its ledger from this source.
    pub ledgers: HashMap<String, Ledger>,
}

impl SourceLedger {
    pub fn queued(adapter: usize, harness: String) -> SourceLedger {
        SourceLedger { adapter, harness, cursor: None, phase: Phase::Queued, ledgers: HashMap::new() }
    }
}

/// All ledgers of an engine.
#[derive(Default)]
pub struct UsageIndex {
    sources: HashMap<PathBuf, SourceLedger>,
    /// Session key → the sources holding a ledger for it (one, unless ids collide).
    owners: HashMap<String, Vec<PathBuf>>,
    pub dirty: bool,
    /// Sources were registered (cache restored, the rest queued): progress is meaningful.
    pub started: bool,
}

impl UsageIndex {
    pub fn source(&self, path: &Path) -> Option<&SourceLedger> {
        self.sources.get(path)
    }

    pub fn source_mut(&mut self, path: &Path) -> Option<&mut SourceLedger> {
        self.sources.get_mut(path)
    }

    pub fn sources(&self) -> impl Iterator<Item = (&PathBuf, &SourceLedger)> {
        self.sources.iter()
    }

    /// Insert or replace a source with its ledgers.
    pub fn put_source(&mut self, path: PathBuf, s: SourceLedger) {
        self.remove_source(&path);
        for k in s.ledgers.keys() {
            self.owners.entry(k.clone()).or_default().push(path.clone());
        }
        self.sources.insert(path, s);
    }

    pub fn remove_source(&mut self, path: &Path) -> Option<SourceLedger> {
        let s = self.sources.remove(path)?;
        for k in s.ledgers.keys() {
            if let Some(v) = self.owners.get_mut(k) {
                v.retain(|p| p != path);
                if v.is_empty() {
                    self.owners.remove(k);
                }
            }
        }
        Some(s)
    }

    /// The ledger of `key` from `path`, created (priced with `generation`) when missing.
    /// `None` when the source is unknown.
    pub fn ledger_mut(&mut self, path: &Path, key: &str, generation: u64) -> Option<&mut Ledger> {
        let s = self.sources.get_mut(path)?;
        if !s.ledgers.contains_key(key) {
            self.owners.entry(key.to_owned()).or_default().push(path.to_path_buf());
        }
        Some(s.ledgers.entry(key.to_owned()).or_insert_with(|| Ledger::priced(generation)))
    }

    /// Every ledger of `key`, in source path order.
    pub fn ledgers(&self, key: &str) -> Vec<&Ledger> {
        let Some(paths) = self.owners.get(key) else { return Vec::new() };
        let mut paths: Vec<&PathBuf> = paths.iter().collect();
        paths.sort();
        paths.into_iter().filter_map(|p| self.sources.get(p)?.ledgers.get(key)).collect()
    }

    /// `key`'s usage as one ledger (borrowed in the usual single-source case).
    pub fn view(&self, key: &str) -> Option<Cow<'_, Ledger>> {
        match self.ledgers(key).as_slice() {
            [] => None,
            [one] => Some(Cow::Borrowed(*one)),
            many => Some(Cow::Owned(Ledger::combine(many))),
        }
    }

    pub fn ledgers_mut(&mut self) -> impl Iterator<Item = (&String, &mut Ledger)> {
        self.sources.values_mut().flat_map(|s| s.ledgers.iter_mut())
    }

    pub fn progress(&self) -> uniflo_schema::UsageProgress {
        let total = self.sources.len() as u64;
        let done = self.sources.values().filter(|s| s.phase == Phase::Ready).count() as u64;
        uniflo_schema::UsageProgress { ready: self.started && done == total, done, total }
    }

    /// Every model name seen in `keys`' steps, with its catalog match and totals.
    pub fn models<'a>(&self, keys: impl IntoIterator<Item = &'a str>, p: &Loaded) -> Vec<ModelUsage> {
        let mut by: HashMap<&str, (ModelUsage, Option<usize>)> = HashMap::new();
        for (si, key) in keys.into_iter().enumerate() {
            for s in self.ledgers(key).into_iter().flat_map(|l| &l.steps) {
                let name = s.model.as_deref().unwrap_or("");
                let (m, last) = by.entry(name).or_insert_with(|| {
                    let r = p.resolve(name);
                    let e = r.entry.as_deref();
                    let m = ModelUsage {
                        model: name.to_owned(),
                        match_kind: Some(r.kind),
                        catalog_id: e.map(|e| e.id.clone()),
                        provider: e.map(|e| e.provider.clone()).filter(|p| !p.is_empty()),
                        prices: e.map(|e| e.prices.clone()).unwrap_or_default(),
                        context_limit: e.and_then(|e| e.context_limit),
                        output_limit: e.and_then(|e| e.output_limit),
                        ..Default::default()
                    };
                    (m, None)
                });
                if *last != Some(si) {
                    *last = Some(si);
                    m.sessions += 1;
                }
                m.steps += 1;
                match s.cost {
                    Some(c) => m.cost_usd = Some(m.cost_usd.unwrap_or(0.0) + c),
                    None => m.unpriced_steps += 1,
                }
            }
        }
        let mut out: Vec<ModelUsage> = by.into_values().map(|(m, _)| m).collect();
        out.sort_by(|a, b| {
            b.cost_usd
                .unwrap_or(0.0)
                .total_cmp(&a.cost_usd.unwrap_or(0.0))
                .then(b.steps.cmp(&a.steps))
                .then(a.model.cmp(&b.model))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::MetaPatch;
    use crate::pricing::Pricing;
    use uniflo_schema::Usage;

    fn ev(id: &str, ts: i64, body: Body) -> Record {
        Record::Event(Event {
            id: id.into(),
            session: "t:1".into(),
            ts,
            pos: None,
            partial: false,
            truncated: false,
            body,
        })
    }

    fn usage(input: u64, cache_read: u64, model: Option<&str>) -> Body {
        Body::Usage(Usage { input, output: 10, cache_read, model: model.map(Into::into), ..Default::default() })
    }

    #[test]
    fn turns_models_and_upserts() {
        let p = Pricing::new(None).current();
        let mut l = Ledger::default();
        let mut n = Names::default();
        let recs = [
            Record::Meta(MetaPatch { model: Some("meta-model".into()), ..Default::default() }),
            ev("ts1", 1, Body::TurnStart {}),
            ev("u1", 2, Body::UserMessage { text: "q".into(), synthetic: false }),
            ev("s1", 3, usage(100, 0, None)),
            ev("a1", 4, Body::AssistantMessage { text: "x".into(), model: Some("asst-model".into()) }),
            ev("s2", 5, usage(50, 1000, None)),
            ev("s2", 5, usage(60, 1000, None)),
            ev("sys", 6, Body::UserMessage { text: "reminder".into(), synthetic: true }),
            ev("u2", 7, Body::UserMessage { text: "q2".into(), synthetic: false }),
            ev("s3", 8, usage(1, 2, Some("own-model"))),
            ev("u1", 2, Body::UserMessage { text: "q".into(), synthetic: false }),
        ];
        for r in &recs {
            l.apply(r, &mut n, &p);
        }
        assert_eq!(l.steps.len(), 3, "s2 upserted, not duplicated");
        assert_eq!(l.prompts.len(), 2, "synthetic and re-delivered prompts do not count");
        let models: Vec<_> = l.steps.iter().map(|s| s.model.as_deref().unwrap()).collect();
        assert_eq!(models, ["meta-model", "asst-model", "own-model"]);
        assert_eq!(l.steps.iter().map(|s| s.turn).collect::<Vec<_>>(), [1, 1, 2]);
        assert_eq!(l.steps[1].input, 60);
        let d = l.detail("t:1", &p);
        assert_eq!(d.steps.iter().map(|s| s.event.as_str()).collect::<Vec<_>>(), ["s1", "s2", "s3"]);
        assert_eq!(d.turns.len(), 2);
        assert_eq!(d.turns[0].steps, 2);
        assert_eq!(d.turns[0].input, 160);
        assert_eq!(d.totals.last_context_tokens, 3);
        assert_eq!(d.totals.unpriced_steps, 3);
        assert_eq!(d.totals.cost_usd, None);
    }

    #[test]
    fn cost_and_source_priority() {
        use uniflo_schema::{CatalogEntry, CostSource, PriceSegment, PriceTier};
        let dir = tempfile::tempdir().unwrap();
        let m = CatalogEntry {
            id: "model-m".into(),
            prices: vec![PriceSegment {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                tiers: vec![PriceTier { above: 200_000, input: 6.0, output: 22.5, cache_read: 0.6, cache_write: None }],
                ..Default::default()
            }],
            ..Default::default()
        };
        std::fs::write(dir.path().join("overrides.json"), serde_json::to_vec(&vec![m]).unwrap()).unwrap();
        let p = Pricing::new(Some(dir.path().to_path_buf())).current();
        let step = |input, output, cache_read, cache_write, model: &str, cost_usd| {
            Body::Usage(Usage {
                input,
                output,
                cache_read,
                cache_write,
                model: Some(model.into()),
                cost_usd,
                ..Default::default()
            })
        };
        let mut l = Ledger::default();
        let mut n = Names::default();
        for r in [
            ev("plain", 1, step(1_000, 500, 10_000, 2_000, "model-m", None)),
            ev("big", 2, step(150_000, 100, 60_000, 0, "model-m", None)),
            ev("own", 3, step(10, 10, 0, 0, "model-m", Some(0.5))),
            ev("unknown", 4, step(10, 10, 0, 0, "totally-unknown-model", None)),
            ev("empty", 5, step(0, 0, 0, 0, "totally-unknown-model", None)),
        ] {
            l.apply(&r, &mut n, &p);
        }
        let d = l.detail("t:1", &p);
        let plain = (1_000.0 * 3.0 + 500.0 * 15.0 + 10_000.0 * 0.3 + 2_000.0 * 3.0 * 1.25) / 1e6;
        let big = (150_000.0 * 6.0 + 100.0 * 22.5 + 60_000.0 * 0.6) / 1e6;
        assert!((d.steps[0].cost_usd.unwrap() - plain).abs() < 1e-9);
        assert!((d.steps[1].cost_usd.unwrap() - big).abs() < 1e-9, "whole request at the tier price");
        assert_eq!((d.steps[2].cost_usd, d.steps[2].cost_source), (Some(0.5), Some(CostSource::Harness)));
        assert_eq!(d.steps[0].cost_source, Some(CostSource::Catalog));
        assert_eq!((d.steps[3].cost_usd, d.steps[3].cost_source), (None, None));
        assert_eq!((d.steps[4].cost_usd, d.steps[4].cost_source), (Some(0.0), None), "no tokens, nothing to price");
        assert_eq!(d.totals.unpriced_steps, 1, "unknown model is unpriced, not $0");
        assert!((d.totals.cost_usd.unwrap() - (plain + big + 0.5)).abs() < 1e-9);

        // The split comes from the rates that priced the step.
        let parts = |s: &StepUsage| {
            let c = s.cost_parts.as_ref().unwrap();
            c.input + c.output + c.cache_read + c.cache_write
        };
        for (s, whole) in [(&d.steps[0], plain), (&d.steps[1], big)] {
            assert!((parts(s) - s.cost_usd.unwrap()).abs() < 1e-9 && (parts(s) - whole).abs() < 1e-9);
        }
        let c = d.steps[0].cost_parts.as_ref().unwrap();
        assert!((c.cache_write - 2_000.0 * 3.75 / 1e6).abs() < 1e-12);
        assert_eq!(
            d.steps[0].price,
            Some(StepPrice { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75, tier_above: None })
        );
        assert_eq!(
            d.steps[1].price,
            Some(StepPrice { input: 6.0, output: 22.5, cache_read: 0.6, cache_write: 7.5, tier_above: Some(200_000) })
        );
        for s in &d.steps[2..] {
            assert!(s.cost_parts.is_none() && s.price.is_none(), "reported, unpriced and empty steps have no split");
        }
    }

    #[test]
    fn a_key_held_by_two_sources_counts_both() {
        let p = Pricing::new(None).current();
        let mut n = Names::default();
        let mut ledger = |base: i64, steps: usize| {
            let mut l = Ledger::default();
            l.apply(
                &ev(&format!("u{base}"), base, Body::UserMessage { text: "q".into(), synthetic: false }),
                &mut n,
                &p,
            );
            for i in 0..steps {
                l.apply(&ev(&format!("s{base}-{i}"), base + 1 + i as i64, usage(10, 0, Some("m"))), &mut n, &p);
            }
            l
        };
        let mut ix = UsageIndex::default();
        for (path, base, steps) in [("/b", 100, 2), ("/a", 10, 3)] {
            let mut s = SourceLedger::queued(0, "t".into());
            s.ledgers.insert("t:1".into(), ledger(base, steps));
            ix.put_source(path.into(), s);
        }
        let v = ix.view("t:1").unwrap();
        assert_eq!((v.steps.len(), v.prompts.len()), (5, 2));
        assert_eq!(v.steps.iter().map(|s| s.turn).collect::<Vec<_>>(), [1, 1, 1, 2, 2], "turns kept apart, by time");
        assert_eq!(v.detail("t:1", &p).turns.len(), 2);
        drop(v);
        // Rebuilding one source replaces only its own part.
        let mut s = SourceLedger::queued(0, "t".into());
        s.ledgers.insert("t:1".into(), ledger(100, 1));
        ix.put_source("/b".into(), s);
        assert_eq!(ix.view("t:1").unwrap().steps.len(), 4);
        ix.remove_source(Path::new("/a"));
        assert_eq!(ix.view("t:1").unwrap().steps.len(), 1);
        ix.remove_source(Path::new("/b"));
        assert!(ix.view("t:1").is_none());
    }

    #[test]
    fn combined_ledgers_keep_each_step_event_id() {
        let p = Pricing::new(None).current();
        let mut n = Names::default();
        let build = |recs: &[Record], n: &mut Names| {
            let mut l = Ledger::default();
            for r in recs {
                l.apply(r, n, &p);
            }
            l
        };
        let a = build(&[ev("a1", 1, usage(1, 0, None)), ev("a2", 3, usage(2, 0, None))], &mut n);
        let b = build(&[ev("b-long-id", 2, usage(3, 0, None))], &mut n);
        let c = Ledger::combine(&[&b, &a]);
        let ids: Vec<_> = c.steps.iter().map(|s| (c.event_id(s), s.input)).collect();
        assert_eq!(ids, [("a1", 1), ("b-long-id", 3), ("a2", 2)]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shrink_gives_spare_capacity_back_to_malloc() {
        unsafe extern "C" {
            fn malloc_size(p: *const std::ffi::c_void) -> usize;
        }
        // SAFETY: the pointer is the live heap block of a non-empty vector.
        let block = |v: &Vec<Step>| unsafe { malloc_size(v.as_ptr().cast()) };
        let p = Pricing::new(None).current();
        let mut n = Names::default();
        let mut l = Ledger::default();
        for i in 0..80 {
            l.apply(&ev(&format!("s{i}"), i, usage(1, 0, None)), &mut n, &p);
        }
        let before = block(&l.steps);
        l.shrink();
        assert!(block(&l.steps) < before * 3 / 4, "{} -> {}", before, block(&l.steps));
        assert_eq!((l.steps.len(), l.event_id(&l.steps[79])), (80, "s79"));
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(id_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(id_hash("a"), 0xaf63_dc4c_8601_ec8c);
    }
}
