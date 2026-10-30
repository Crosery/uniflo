//! Session families: a session plus every descendant reached through the `parent` chain
//! (a sub-agent's sub-agents included), the data behind `Session.family`.
//!
//! The engine builds a [`Forest`] (one pass over the sessions) per update and asks it for
//! the families of the few sessions an update can have changed.

use std::collections::{HashMap, HashSet};
use uniflo_schema::{SessionFamily, SessionUsage};

/// Parent → children index.
pub struct Forest<'a> {
    children: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Forest<'a> {
    /// From `(session, parent)` links. Children are kept sorted so a family is always summed
    /// in the same order: the floating-point total of an unchanged family never differs.
    pub fn new(links: impl IntoIterator<Item = (&'a str, &'a str)>) -> Forest<'a> {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for (child, parent) in links {
            children.entry(parent).or_default().push(child);
        }
        for v in children.values_mut() {
            v.sort_unstable();
        }
        Forest { children }
    }

    /// Totals of `root` and its descendants; `None` when it has no descendant. A session
    /// without `usage` counts as a descendant and adds nothing.
    pub fn family<'u>(&self, root: &str, usage: impl Fn(&str) -> Option<&'u SessionUsage>) -> Option<SessionFamily> {
        self.children.get(root)?;
        let mut f = SessionFamily::default();
        let mut seen: HashSet<&str> = HashSet::from([root]);
        let mut stack: Vec<&str> = vec![root];
        while let Some(k) = stack.pop() {
            if let Some(u) = usage(k) {
                f.steps += u.steps;
                f.input += u.input;
                f.output += u.output;
                f.cache_read += u.cache_read;
                f.cache_write += u.cache_write;
                f.reasoning += u.reasoning;
                f.unpriced_steps += u.unpriced_steps;
                if let Some(c) = u.cost_usd {
                    f.cost_usd = Some(f.cost_usd.unwrap_or(0.0) + c);
                }
            }
            for c in self.children.get(k).into_iter().flatten().rev() {
                if seen.insert(c) {
                    f.sessions += 1;
                    stack.push(c);
                }
            }
        }
        // A session parented to itself has no descendant of its own.
        (f.sessions > 0).then_some(f)
    }
}

/// The sessions above `start`, nearest first: `parent_of` maps a session to its parent, `None`
/// where the chain ends (unknown parent). Stops at a cycle.
pub fn ancestors<'a>(start: &'a str, parent_of: impl Fn(&str) -> Option<&'a str>) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut seen: HashSet<&str> = HashSet::from([start]);
    let mut cur = parent_of(start);
    while let Some(p) = cur {
        if !seen.insert(p) {
            break;
        }
        out.push(p);
        cur = parent_of(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(steps: u64, cost: Option<f64>) -> SessionUsage {
        SessionUsage {
            steps,
            input: steps * 10,
            cost_usd: cost,
            unpriced_steps: u64::from(cost.is_none()),
            ..Default::default()
        }
    }

    #[test]
    fn sums_self_and_all_levels() {
        let forest = Forest::new([("c1", "p"), ("c2", "p"), ("c11", "c1")]);
        let us: HashMap<&str, SessionUsage> = HashMap::from([
            ("p", usage(1, Some(1.0))),
            ("c1", usage(2, Some(2.0))),
            ("c2", usage(3, None)),
            ("c11", usage(4, Some(4.0))),
        ]);
        let get = |k: &str| us.get(k);
        let p = forest.family("p", get).unwrap();
        assert_eq!((p.sessions, p.steps, p.input, p.unpriced_steps), (3, 10, 100, 1));
        assert_eq!(p.cost_usd, Some(7.0), "unpriced sessions add no amount");
        let c1 = forest.family("c1", get).unwrap();
        assert_eq!((c1.sessions, c1.steps, c1.cost_usd), (1, 6, Some(6.0)));
        assert!(forest.family("c2", get).is_none() && forest.family("c11", get).is_none());
        assert!(forest.family("unknown", get).is_none());
    }

    #[test]
    fn missing_usage_counts_as_zero_and_cost_stays_null() {
        let forest = Forest::new([("c", "p")]);
        let f = forest.family("p", |_| None).unwrap();
        assert_eq!((f.sessions, f.steps, f.cost_usd), (1, 0, None));
        let only_unpriced = usage(2, None);
        let f = forest.family("p", |_| Some(&only_unpriced)).unwrap();
        assert_eq!((f.steps, f.unpriced_steps, f.cost_usd), (4, 2, None));
    }

    #[test]
    fn cycles_terminate() {
        let forest = Forest::new([("a", "b"), ("b", "a")]);
        let f = forest.family("a", |_| None).unwrap();
        assert_eq!(f.sessions, 1, "b only; the root is never its own descendant");
        let parents: HashMap<&str, &str> = HashMap::from([("a", "b"), ("b", "a")]);
        assert_eq!(ancestors("a", |k| parents.get(k).copied()), ["b"]);
    }

    #[test]
    fn every_family_of_a_large_forest_is_one_pass_not_quadratic() {
        // 100 roots × 50 children × 10 grandchildren = 55 100 sessions, depth 3.
        let names: Vec<String> = (0..55_100).map(|i| format!("s{i}")).collect();
        let mut links: Vec<(&str, &str)> = Vec::new();
        for r in 0..100 {
            for c in 0..50 {
                let child = 100 + r * 50 + c;
                links.push((&names[child], &names[r]));
                for g in 0..10 {
                    links.push((&names[5_100 + (r * 50 + c) * 10 + g], &names[child]));
                }
            }
        }
        let one = SessionUsage { steps: 1, cost_usd: Some(0.5), ..Default::default() };
        let t0 = std::time::Instant::now();
        let forest = Forest::new(links);
        let fams: Vec<Option<SessionFamily>> = names.iter().map(|n| forest.family(n, |_| Some(&one))).collect();
        let took = t0.elapsed();
        assert_eq!(fams.iter().filter(|f| f.is_some()).count(), 100 + 5_000);
        let root = fams[0].as_ref().unwrap();
        assert_eq!((root.sessions, root.steps, root.cost_usd), (550, 551, Some(275.5)));
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
        eprintln!("55 100 sessions, every family: {took:?}");
    }

    #[test]
    fn ancestors_nearest_first() {
        let parents: HashMap<&str, &str> = HashMap::from([("c11", "c1"), ("c1", "p")]);
        assert_eq!(ancestors("c11", |k| parents.get(k).copied()), ["c1", "p"]);
        assert!(ancestors("p", |k| parents.get(k).copied()).is_empty());
    }
}
