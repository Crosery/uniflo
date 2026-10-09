//! Event windows centred on one event id (jumping from a search hit into a transcript).

use crate::adapter::HistoryQuery;
use crate::engine::Engine;
use anyhow::Result;
use uniflo_schema::Event;

const PAGE: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Chronological; `events[anchor]` is the requested event.
    pub events: Vec<Event>,
    pub anchor: usize,
}

/// Page backwards through `fetch(before, limit)` (the [`crate::Adapter::history`] contract)
/// until `id` shows up, then return up to `limit` events around it, the anchor in the middle
/// unless the transcript ends first. `None` when the session has no such event.
pub fn window(
    mut fetch: impl FnMut(Option<u64>, usize) -> Result<Vec<Event>>,
    id: &str,
    limit: usize,
) -> Result<Option<Window>> {
    let limit = limit.max(1);
    let page_size = PAGE.max(limit);
    // Events newer than the current page, chronological; only the oldest `limit` can matter.
    let mut newer: Vec<Event> = Vec::new();
    let mut before = None;
    loop {
        let mut page = fetch(before, page_size)?;
        if page.is_empty() {
            return Ok(None);
        }
        if let Some(i) = page.iter().rposition(|e| e.id == id) {
            let mut after = page.split_off(i + 1);
            after.append(&mut newer);
            after.truncate(limit - 1);
            let anchor = page.pop().expect("anchor is in the page");
            let mut older = page;
            if older.len() < limit - 1
                && let Some(pos) = older.first().or(Some(&anchor)).and_then(|e| e.pos)
            {
                let mut more = fetch(Some(pos), limit - 1 - older.len())?;
                more.retain(|e| e.pos.is_some_and(|p| p < pos));
                more.append(&mut older);
                older = more;
            }
            // Balanced split; a side that runs short gives its share to the other.
            let half = (limit - 1) / 2;
            let n_after = after.len().min(limit - 1 - half.min(older.len()));
            let n_before = older.len().min(limit - 1 - n_after);
            let mut events = older.split_off(older.len() - n_before);
            let anchor_at = events.len();
            events.push(anchor);
            events.extend(after.into_iter().take(n_after));
            return Ok(Some(Window { events, anchor: anchor_at }));
        }
        let first = page.first().and_then(|e| e.pos);
        page.append(&mut newer);
        page.truncate(limit);
        newer = page;
        match first {
            Some(p) if before.is_none_or(|b| p < b) => before = Some(p),
            _ => return Ok(None),
        }
    }
}

impl Engine {
    /// Blocking: up to `limit` events of `key` centred on event `id`.
    pub fn around(&self, key: &str, id: &str, limit: usize) -> Result<Option<Window>> {
        window(|before, limit| self.history(key, &HistoryQuery { before, limit }), id, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniflo_schema::Body;

    fn ev(i: u64) -> Event {
        Event {
            id: format!("e{i}"),
            session: "t:s".into(),
            ts: i as i64,
            pos: Some(i * 10),
            partial: false,
            truncated: false,
            body: Body::Reasoning { text: String::new() },
        }
    }

    /// `n` events, pos 0,10,20…; history semantics: newest `limit` with pos < before.
    fn source(n: u64) -> impl FnMut(Option<u64>, usize) -> Result<Vec<Event>> {
        let all: Vec<Event> = (0..n).map(ev).collect();
        move |before, limit| {
            let mut v: Vec<Event> = all.iter().filter(|e| before.is_none_or(|b| e.pos.unwrap() < b)).cloned().collect();
            let skip = v.len().saturating_sub(limit);
            Ok(v.split_off(skip))
        }
    }

    fn ids(w: &Window) -> Vec<String> {
        w.events.iter().map(|e| e.id.clone()).collect()
    }

    #[test]
    fn centred_across_pages() {
        let w = window(source(3000), "e1200", 21).unwrap().unwrap();
        assert_eq!(w.events.len(), 21);
        assert_eq!(w.events[w.anchor].id, "e1200");
        assert_eq!(w.anchor, 10);
        assert_eq!(w.events.first().unwrap().id, "e1190");
        assert_eq!(w.events.last().unwrap().id, "e1210");
    }

    #[test]
    fn anchor_on_a_page_edge_fetches_older() {
        // First page holds e2500..e2999: e2500 is its oldest event.
        let w = window(source(3000), "e2500", 20).unwrap().unwrap();
        assert_eq!(w.events[w.anchor].id, "e2500");
        assert_eq!(w.anchor, 9);
        assert_eq!(w.events.len(), 20);
        let pos: Vec<u64> = w.events.iter().map(|e| e.pos.unwrap()).collect();
        assert!(pos.windows(2).all(|p| p[0] < p[1]), "chronological, no duplicates");
    }

    #[test]
    fn ends_shift_the_window() {
        let w = window(source(100), "e98", 20).unwrap().unwrap();
        assert_eq!((w.events.len(), w.anchor), (20, 18));
        assert_eq!(ids(&w).last().unwrap(), "e99");
        let w = window(source(100), "e1", 20).unwrap().unwrap();
        assert_eq!((w.events.len(), w.anchor), (20, 1));
        assert_eq!(ids(&w).first().unwrap(), "e0");
        let w = window(source(5), "e2", 20).unwrap().unwrap();
        assert_eq!(ids(&w), vec!["e0", "e1", "e2", "e3", "e4"]);
        assert_eq!(w.anchor, 2);
    }

    #[test]
    fn missing_event() {
        assert!(window(source(1200), "nope", 20).unwrap().is_none());
        assert!(window(source(0), "e0", 20).unwrap().is_none());
    }
}
