//! The call chain of one session: turns → steps → items, behind
//! `GET /v1/sessions/{key}/trajectory`.
//!
//! [`Builder`] folds a session's records, in source order and duplicates included, the way the
//! usage ledger reads them. A [`Ledger`] fed the same records supplies every number of a step,
//! so they are those of `/usage` for the same step. A usage event closes the records since the
//! previous one into its step; a usage id seen again (Claude repeats a message's usage on each
//! of its content lines) extends the step it already made. [`link_children`] then hangs the
//! sub-sessions on the calls that started them.

use crate::adapter::Record;
use crate::pricing::Loaded;
use crate::usage::{Ledger, Names};
use std::collections::HashMap;
use uniflo_schema::{
    Body, Event, Session, SessionTrajectory, StepUsage, TrajectoryChild, TrajectoryItem, TrajectoryStep, TrajectoryTurn,
};

/// Characters kept of a human prompt.
const PROMPT_CHARS: usize = 300;
/// Characters kept of a reply, tool arguments or tool output.
const PREVIEW_CHARS: usize = 200;

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn item_ts(i: &TrajectoryItem) -> i64 {
    match i {
        TrajectoryItem::Reasoning { ts, .. } | TrajectoryItem::Text { ts, .. } | TrajectoryItem::System { ts, .. } => {
            *ts
        }
        TrajectoryItem::Tool { ts, .. } => *ts,
    }
}

/// When the item was over: a call ends with its result.
fn item_end(i: &TrajectoryItem) -> i64 {
    match i {
        TrajectoryItem::Tool { ts, ended_at, .. } => (*ts).max(ended_at.unwrap_or(0)),
        other => item_ts(other),
    }
}

struct TurnAcc {
    turn: u32,
    event: Option<String>,
    prompt: Option<String>,
    started_at: i64,
    ended_at: i64,
    steps: Vec<usize>,
    items: Vec<usize>,
}

struct StepAcc {
    event: String,
    items: Vec<usize>,
}

/// Incremental fold of a session's records into a [`SessionTrajectory`] (without children).
pub struct Builder<'p> {
    key: String,
    p: &'p Loaded,
    ledger: Ledger,
    names: Names,
    /// Every item; steps and turns list indices into it.
    arena: Vec<TrajectoryItem>,
    /// Event id → arena index: a re-delivered event replaces its item.
    item_at: HashMap<String, usize>,
    /// Call id → arena index of the `tool` item, for results.
    calls: HashMap<String, usize>,
    steps: Vec<StepAcc>,
    step_at: HashMap<String, usize>,
    turns: Vec<TurnAcc>,
    turn_at: HashMap<u32, usize>,
    turn_start: HashMap<u32, i64>,
    /// Ledger turn of the latest record.
    cur: u32,
    /// Items since the last usage.
    pending: Vec<usize>,
}

impl<'p> Builder<'p> {
    pub fn new(key: &str, p: &'p Loaded) -> Builder<'p> {
        Builder {
            key: key.to_owned(),
            p,
            ledger: Ledger::default(),
            names: Names::default(),
            arena: Vec::new(),
            item_at: HashMap::new(),
            calls: HashMap::new(),
            steps: Vec::new(),
            step_at: HashMap::new(),
            turns: Vec::new(),
            turn_at: HashMap::new(),
            turn_start: HashMap::new(),
            cur: 0,
            pending: Vec::new(),
        }
    }

    fn turn_mut(&mut self, turn: u32, hint_ts: i64) -> &mut TurnAcc {
        let i = match self.turn_at.get(&turn) {
            Some(&i) => i,
            None => {
                let started = [self.turn_start.get(&turn).copied().unwrap_or(0), hint_ts]
                    .into_iter()
                    .filter(|t| *t > 0)
                    .min()
                    .unwrap_or(0);
                self.turns.push(TurnAcc {
                    turn,
                    event: None,
                    prompt: None,
                    started_at: started,
                    ended_at: 0,
                    steps: Vec::new(),
                    items: Vec::new(),
                });
                self.turn_at.insert(turn, self.turns.len() - 1);
                self.turns.len() - 1
            }
        };
        &mut self.turns[i]
    }

    /// The turn ended: what is still pending was never followed by a usage.
    fn close_turn(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let first = item_ts(&self.arena[self.pending[0]]);
        let pending = std::mem::take(&mut self.pending);
        let turn = self.cur;
        self.turn_mut(turn, first).items.extend(pending);
    }

    fn add(&mut self, e: &Event, item: TrajectoryItem) {
        if let Some(&i) = self.item_at.get(&e.id) {
            match (&mut self.arena[i], item) {
                // The same call delivered again: keep what its result already added.
                (
                    TrajectoryItem::Tool { name, input_preview, .. },
                    TrajectoryItem::Tool { name: n, input_preview: p, .. },
                ) => {
                    *name = n;
                    *input_preview = p;
                }
                (slot, item) => *slot = item,
            }
            return;
        }
        let i = self.arena.len();
        if let TrajectoryItem::Tool { call_id, .. } = &item {
            self.calls.insert(call_id.clone(), i);
        }
        self.arena.push(item);
        self.item_at.insert(e.id.clone(), i);
        self.pending.push(i);
    }

    /// Feed the next record of the session (source order).
    pub fn push(&mut self, rec: &Record) {
        self.ledger.apply(rec, &mut self.names, self.p);
        let Record::Event(e) = rec else { return };
        let turn = self.ledger.turn;
        if turn != self.cur {
            self.close_turn();
            self.cur = turn;
        }
        match &e.body {
            Body::UserMessage { text, synthetic: false } => {
                let t = self.turn_mut(turn, e.ts);
                if t.prompt.is_none() {
                    t.event = Some(e.id.clone());
                    t.prompt = Some(clip(text, PROMPT_CHARS));
                    if e.ts > 0 && (t.started_at == 0 || e.ts < t.started_at) {
                        t.started_at = e.ts;
                    }
                }
            }
            Body::UserMessage { .. } => {}
            Body::TurnStart {} => {
                self.turn_start.entry(turn).or_insert(e.ts);
            }
            Body::TurnEnd { .. } => {
                if let Some(&i) = self.turn_at.get(&turn) {
                    self.turns[i].ended_at = self.turns[i].ended_at.max(e.ts);
                }
            }
            Body::Reasoning { text } => {
                let chars = text.chars().count() as u64;
                self.add(e, TrajectoryItem::Reasoning { event: e.id.clone(), ts: e.ts, chars });
            }
            Body::AssistantMessage { text, .. } => {
                let item = TrajectoryItem::Text {
                    event: e.id.clone(),
                    ts: e.ts,
                    chars: text.chars().count() as u64,
                    preview: clip(text, PREVIEW_CHARS),
                };
                self.add(e, item);
            }
            Body::ToolCall { call_id, name, input } => {
                let args = match input {
                    serde_json::Value::String(s) => clip(s, PREVIEW_CHARS),
                    serde_json::Value::Null => String::new(),
                    other => clip(&other.to_string(), PREVIEW_CHARS),
                };
                let item = TrajectoryItem::Tool {
                    event: e.id.clone(),
                    ts: e.ts,
                    name: name.clone(),
                    call_id: call_id.clone(),
                    input_preview: args,
                    ended_at: None,
                    is_error: None,
                    output_preview: None,
                    running: true,
                    children: Vec::new(),
                };
                self.add(e, item);
            }
            Body::ToolResult { call_id, output, is_error, .. } => {
                if let Some(&i) = self.calls.get(call_id)
                    && let TrajectoryItem::Tool { ended_at, is_error: err, output_preview, running, .. } =
                        &mut self.arena[i]
                {
                    *ended_at = Some(e.ts);
                    *err = Some(*is_error);
                    *output_preview = Some(clip(output, PREVIEW_CHARS));
                    *running = false;
                }
            }
            Body::System { subtype, .. } => {
                self.add(e, TrajectoryItem::System { event: e.id.clone(), ts: e.ts, subtype: subtype.clone() });
            }
            Body::Usage(_) => {
                let pending = std::mem::take(&mut self.pending);
                let si = match self.step_at.get(&e.id) {
                    Some(&i) => i,
                    None => {
                        let first = pending.first().map_or(e.ts, |&i| item_ts(&self.arena[i]));
                        self.steps.push(StepAcc { event: e.id.clone(), items: Vec::new() });
                        let si = self.steps.len() - 1;
                        self.step_at.insert(e.id.clone(), si);
                        self.turn_mut(turn, first).steps.push(si);
                        si
                    }
                };
                self.steps[si].items.extend(pending);
            }
        }
    }

    /// The trajectory without children (see [`link_children`]).
    pub fn finish(mut self) -> SessionTrajectory {
        self.close_turn();
        let detail = self.ledger.detail(&self.key, self.p);
        let by_event: HashMap<&str, (usize, &StepUsage)> =
            detail.steps.iter().enumerate().map(|(i, s)| (s.event.as_str(), (i, s))).collect();
        let mut arena: Vec<Option<TrajectoryItem>> = self.arena.into_iter().map(Some).collect();
        let mut take = |ids: &[usize]| -> Vec<TrajectoryItem> { ids.iter().filter_map(|&i| arena[i].take()).collect() };
        let mut accs = self.turns;
        accs.sort_by_key(|t| t.turn);
        let mut turns = Vec::with_capacity(accs.len());
        for t in accs {
            let mut ended = t.ended_at.max(t.started_at);
            let mut prev_end = t.started_at;
            let mut steps = Vec::with_capacity(t.steps.len());
            for &si in &t.steps {
                let acc = &self.steps[si];
                let Some(&(i, u)) = by_event.get(acc.event.as_str()) else { continue };
                let items = take(&acc.items);
                let end = items.iter().map(item_end).fold(u.ts, i64::max);
                let started_at = if prev_end > 0 { prev_end.min(u.ts) } else { u.ts };
                prev_end = end;
                ended = ended.max(end);
                steps.push(TrajectoryStep {
                    index: i as u32 + 1,
                    event: u.event.clone(),
                    started_at,
                    ts: u.ts,
                    model: u.model.clone(),
                    input: u.input,
                    output: u.output,
                    cache_read: u.cache_read,
                    cache_write: u.cache_write,
                    reasoning: u.reasoning,
                    cost_usd: u.cost_usd,
                    cost_source: u.cost_source,
                    cost_parts: u.cost_parts.clone(),
                    price: u.price.clone(),
                    context_tokens: u.context_tokens,
                    context_pct: u.context_pct,
                    items,
                });
            }
            let items = take(&t.items);
            ended = items.iter().map(item_end).fold(ended, i64::max);
            turns.push(TrajectoryTurn {
                turn: t.turn,
                event: t.event,
                started_at: t.started_at,
                ended_at: ended,
                prompt: t.prompt,
                steps,
                items,
            });
        }
        SessionTrajectory { session: self.key, turns, children: Vec::new() }
    }
}

/// Whether a tool name looks like one that starts a sub-agent.
fn spawns_agents(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["agent", "task", "spawn"].iter().any(|k| n.contains(k))
}

struct ToolRef {
    turn: usize,
    /// Position in `turns[turn].steps`; `None` for a call in the turn's own items.
    step: Option<usize>,
    item: usize,
    ts: i64,
    agentish: bool,
}

/// Fill `children` of `t` from `kids`, the direct sub-sessions, and mark the `tool` item each
/// one hangs from. A child whose `parent_call` names a call goes to exactly that call; the
/// others to the latest not-yet-claimed sub-agent-like call that began before them; failing
/// that they hang from the step they started in.
pub fn link_children(t: &mut SessionTrajectory, kids: &[Session]) {
    let mut refs: Vec<ToolRef> = Vec::new();
    let mut by_call: HashMap<String, usize> = HashMap::new();
    for (ti, turn) in t.turns.iter().enumerate() {
        let lists = turn.steps.iter().enumerate().map(|(si, s)| (Some(si), &s.items)).chain([(None, &turn.items)]);
        for (step, items) in lists {
            for (ii, it) in items.iter().enumerate() {
                if let TrajectoryItem::Tool { call_id, name, ts, .. } = it {
                    by_call.entry(call_id.clone()).or_insert(refs.len());
                    refs.push(ToolRef { turn: ti, step, item: ii, ts: *ts, agentish: spawns_agents(name) });
                }
            }
        }
    }
    let start = |s: &Session| s.started_at.unwrap_or(s.updated_at);
    let mut order: Vec<&Session> = kids.iter().collect();
    order.sort_by(|a, b| start(a).cmp(&start(b)).then_with(|| a.key.cmp(&b.key)));

    let mut claimed = vec![false; refs.len()];
    let mut hung: HashMap<&str, usize> = HashMap::new();
    for k in &order {
        if let Some(&r) = k.parent_call.as_ref().and_then(|c| by_call.get(c)) {
            claimed[r] = true;
            hung.insert(&k.key, r);
        }
    }
    for k in &order {
        // A `parent_call` that is not among these calls started it somewhere else (a sub-agent
        // of a sub-agent, a call the transcript no longer holds): guessing by time would mislead.
        if hung.contains_key(k.key.as_str()) || k.parent_call.is_some() {
            continue;
        }
        let t0 = start(k);
        let near = refs
            .iter()
            .enumerate()
            .filter(|(i, r)| r.agentish && !claimed[*i] && r.ts <= t0)
            .max_by_key(|(i, r)| (r.ts, *i))
            .map(|(i, _)| i);
        if let Some(r) = near {
            claimed[r] = true;
            hung.insert(&k.key, r);
        }
    }

    let mut children = Vec::with_capacity(order.len());
    for k in order {
        let (call_id, step) = match hung.get(k.key.as_str()) {
            Some(&r) => {
                let tr = &refs[r];
                let turn = &mut t.turns[tr.turn];
                let step = tr.step.map(|si| turn.steps[si].index);
                let items = match tr.step {
                    Some(si) => &mut turn.steps[si].items,
                    None => &mut turn.items,
                };
                let mut call_id = None;
                if let TrajectoryItem::Tool { call_id: c, children: ch, .. } = &mut items[tr.item] {
                    ch.push(k.key.clone());
                    call_id = Some(c.clone());
                }
                (call_id, step)
            }
            None => {
                let t0 = start(k);
                let mut steps = t.turns.iter().flat_map(|turn| &turn.steps);
                let step = steps
                    .clone()
                    .find(|s| s.started_at <= t0 && t0 <= s.ts)
                    .or_else(|| steps.rfind(|s| s.started_at <= t0))
                    .map(|s| s.index);
                (None, step)
            }
        };
        children.push(TrajectoryChild {
            key: k.key.clone(),
            // What the session list shows: the title, else the first prompt.
            title: k.title.clone().or_else(|| k.preview.clone()).filter(|t| !t.is_empty()),
            model: k.model.clone(),
            status: k.status,
            started_at: k.started_at,
            updated_at: k.updated_at,
            usage: k.usage.as_deref().cloned(),
            family: k.family.as_deref().cloned(),
            call_id,
            step,
        });
    }
    t.children = children;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Pricing;
    use serde_json::json;
    use uniflo_schema::{Status, Usage};

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

    fn user(id: &str, ts: i64, text: &str) -> Record {
        ev(id, ts, Body::UserMessage { text: text.into(), synthetic: false })
    }

    fn usage(id: &str, ts: i64, input: u64) -> Record {
        ev(id, ts, Body::Usage(Usage { input, output: 10, model: Some("m".into()), ..Default::default() }))
    }

    fn call(id: &str, ts: i64, call_id: &str, name: &str) -> Record {
        ev(id, ts, Body::ToolCall { call_id: call_id.into(), name: name.into(), input: json!({"k": "v"}) })
    }

    fn result(id: &str, ts: i64, call_id: &str, is_error: bool) -> Record {
        ev(id, ts, Body::ToolResult { call_id: call_id.into(), name: None, output: "out".into(), is_error })
    }

    fn text(id: &str, ts: i64, t: &str) -> Record {
        ev(id, ts, Body::AssistantMessage { text: t.into(), model: None })
    }

    fn build(recs: &[Record]) -> SessionTrajectory {
        let p = Pricing::new(None).current();
        let mut b = Builder::new("t:1", &p);
        for r in recs {
            b.push(r);
        }
        b.finish()
    }

    fn kinds(items: &[TrajectoryItem]) -> Vec<&'static str> {
        items
            .iter()
            .map(|i| match i {
                TrajectoryItem::Reasoning { .. } => "reasoning",
                TrajectoryItem::Text { .. } => "text",
                TrajectoryItem::Tool { .. } => "tool",
                TrajectoryItem::System { .. } => "system",
            })
            .collect()
    }

    #[test]
    fn records_before_a_usage_belong_to_it_and_a_repeated_usage_extends_its_step() {
        let t = build(&[
            user("u1", 100, "go"),
            ev("r", 110, Body::Reasoning { text: "hm".into() }),
            usage("s1", 111, 10),
            text("a", 120, "hello"),
            // The same API message repeats its usage on the next line (Claude).
            usage("s1", 121, 10),
            call("c", 130, "call1", "Bash"),
            usage("s1", 131, 10),
            result("cr", 140, "call1", false),
            text("b", 150, "done"),
            usage("s2", 151, 20),
            // Never followed by a usage: stays on the turn.
            text("tail", 160, "bye"),
            ev("end", 170, Body::TurnEnd { reason: None }),
        ]);
        assert_eq!(t.turns.len(), 1);
        let turn = &t.turns[0];
        assert_eq!((turn.turn, turn.event.as_deref(), turn.started_at, turn.ended_at), (1, Some("u1"), 100, 170));
        assert_eq!(turn.steps.len(), 2);
        assert_eq!(kinds(&turn.steps[0].items), ["reasoning", "text", "tool"]);
        assert_eq!(kinds(&turn.steps[1].items), ["text"]);
        assert_eq!(kinds(&turn.items), ["text"]);
        let (s1, s2) = (&turn.steps[0], &turn.steps[1]);
        assert_eq!((s1.index, s2.index), (1, 2));
        assert_eq!((s1.started_at, s1.ts), (100, 131), "first step starts with the turn");
        assert_eq!((s2.started_at, s2.ts), (140, 151), "second starts when the first's last record ended");
        assert_eq!(s1.output, 10);
        match &s1.items[2] {
            TrajectoryItem::Tool { ended_at, is_error, output_preview, running, .. } => {
                assert_eq!(
                    (*ended_at, *is_error, output_preview.as_deref(), *running),
                    (Some(140), Some(false), Some("out"), false)
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn injected_messages_open_no_turn_and_a_turn_start_is_merged_with_its_prompt() {
        let t = build(&[
            ev("ts1", 90, Body::TurnStart {}),
            user("u1", 100, "first"),
            usage("s1", 110, 1),
            ev("inj", 115, Body::UserMessage { text: "<reminder>".into(), synthetic: true }),
            ev("sys", 116, Body::System { subtype: "compact".into(), text: "x".into() }),
            usage("s2", 120, 2),
            user("u2", 200, "second"),
            call("c", 210, "call2", "Bash"),
            usage("s3", 211, 3),
        ]);
        assert_eq!(t.turns.iter().map(|t| t.turn).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(t.turns[0].started_at, 90, "turn start precedes its prompt");
        assert_eq!(t.turns[0].prompt.as_deref(), Some("first"));
        assert_eq!(kinds(&t.turns[0].steps[1].items), ["system"]);
        let idx: Vec<u32> = t.turns.iter().flat_map(|t| &t.steps).map(|s| s.index).collect();
        assert_eq!(idx, [1, 2, 3]);
        match &t.turns[1].steps[0].items[0] {
            TrajectoryItem::Tool { running, ended_at, is_error, .. } => {
                assert_eq!((*running, *ended_at, *is_error), (true, None, None))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn previews_cut_by_characters_not_bytes() {
        let long = "é世".repeat(200);
        let t = build(&[
            user("u1", 1, &long),
            text("a", 2, &long),
            ev("c", 3, Body::ToolCall { call_id: "k".into(), name: "Write".into(), input: json!(long) }),
            ev("r", 4, Body::ToolResult { call_id: "k".into(), name: None, output: long.clone(), is_error: false }),
            usage("s", 5, 1),
        ]);
        assert_eq!(t.turns[0].prompt.as_ref().unwrap().chars().count(), 300);
        match &t.turns[0].steps[0].items[..] {
            [
                TrajectoryItem::Text { chars, preview, .. },
                TrajectoryItem::Tool { input_preview, output_preview, .. },
            ] => {
                assert_eq!((*chars, preview.chars().count()), (400, 200));
                assert_eq!(input_preview.chars().count(), 200);
                assert_eq!(output_preview.as_ref().unwrap().chars().count(), 200);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn records_with_no_usage_at_all_sit_on_the_turn() {
        let t = build(&[user("u1", 1, "q"), text("a", 2, "thinking aloud"), call("c", 3, "k", "Bash")]);
        assert_eq!(t.turns.len(), 1);
        assert!(t.turns[0].steps.is_empty());
        assert_eq!(kinds(&t.turns[0].items), ["text", "tool"]);
        assert_eq!(t.turns[0].ended_at, 3);
        assert!(build(&[]).turns.is_empty());
    }

    fn child(key: &str, started: i64, parent_call: Option<&str>) -> Session {
        Session {
            key: key.into(),
            harness: "t".into(),
            id: key.into(),
            parent: Some("t:1".into()),
            parent_call: parent_call.map(Into::into),
            title: None,
            cwd: None,
            model: None,
            preview: None,
            source: String::new(),
            started_at: Some(started),
            updated_at: started + 5,
            status: Status::Idle,
            status_since: 0,
            status_reason: None,
            pid: None,
            usage: None,
            family: None,
            archived: false,
        }
    }

    #[test]
    fn a_child_without_a_title_is_named_by_its_first_prompt() {
        let mut t = build(&[user("u1", 100, "go"), usage("s1", 120, 1)]);
        let titled =
            Session { title: Some("Review".into()), preview: Some("first prompt".into()), ..child("t:a", 110, None) };
        let bare = Session { preview: Some("only a prompt".into()), ..child("t:b", 111, None) };
        let nameless = child("t:c", 112, None);
        link_children(&mut t, &[titled, bare, nameless]);
        let title = |k: &str| t.children.iter().find(|c| c.key == k).unwrap().title.clone();
        assert_eq!(title("t:a").as_deref(), Some("Review"));
        assert_eq!(title("t:b").as_deref(), Some("only a prompt"));
        assert_eq!(title("t:c"), None);
    }

    #[test]
    fn children_hang_on_their_call_else_on_the_nearest_unclaimed_agent_call_else_on_a_step() {
        let mut t = build(&[
            user("u1", 100, "go"),
            call("c1", 110, "spawn1", "spawn_agent"),
            call("c2", 112, "bash", "Bash"),
            call("c3", 114, "spawn2", "Task"),
            usage("s1", 120, 1),
            result("r1", 130, "spawn1", false),
            user("u2", 200, "next"),
            call("c4", 210, "spawn3", "Agent"),
            usage("s2", 220, 2),
        ]);
        link_children(
            &mut t,
            &[
                child("t:exact", 300, Some("spawn3")),
                child("t:near", 125, None),
                child("t:next", 126, None),
                child("t:late", 400, None),
                child("t:lost", 50, None),
                child("t:elsewhere", 125, Some("not-in-this-transcript")),
            ],
        );
        let at = |k: &str| t.children.iter().find(|c| c.key == k).map(|c| (c.call_id.as_deref(), c.step)).unwrap();
        // By parent_call, whatever the times say.
        assert_eq!(at("t:exact"), (Some("spawn3"), Some(2)));
        // The latest unclaimed agent-like call before the start: spawn2 (not the Bash call), then spawn1.
        assert_eq!(at("t:near"), (Some("spawn2"), Some(1)));
        assert_eq!(at("t:next"), (Some("spawn1"), Some(1)));
        // spawn3 was claimed exactly, nothing is left: hangs from the step it started in (the last one).
        assert_eq!(at("t:late"), (None, Some(2)));
        // Older than every step.
        assert_eq!(at("t:lost"), (None, None));
        // Its call is elsewhere: no guess by time, only the step it started in.
        assert_eq!(at("t:elsewhere"), (None, Some(1)));
        let order: Vec<&str> = t.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(order, ["t:lost", "t:elsewhere", "t:near", "t:next", "t:exact", "t:late"], "by start, then key");
        let tool_children = |step: usize, i: usize| match &t.turns[step].steps[0].items[i] {
            TrajectoryItem::Tool { children, .. } => children.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(tool_children(0, 0), ["t:next"]);
        assert!(tool_children(0, 1).is_empty());
        assert_eq!(tool_children(0, 2), ["t:near"]);
        assert_eq!(tool_children(1, 0), ["t:exact"]);
    }
}
