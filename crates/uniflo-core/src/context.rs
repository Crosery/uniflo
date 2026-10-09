//! Hand-off context: the most recent sessions of one project (git root), as data and as the
//! short Markdown `uniflo context` prints into a new agent session.

use crate::usage::report::project_of;
use crate::util::preview;
use uniflo_schema::{ContextReport, ContextSession, Session};

/// Characters of the first prompt shown per session.
pub const PREVIEW_CHARS: usize = 80;

/// Top-level sessions whose cwd belongs to `project` and that were active at or after `since`,
/// newest first, at most `limit`.
pub fn report(sessions: &[Session], project: &str, since: i64, limit: usize) -> ContextReport {
    let mut picked: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.parent.is_none() && s.updated_at >= since)
        .filter(|s| s.cwd.as_deref().is_some_and(|c| !c.is_empty() && project_of(c) == project))
        .collect();
    picked.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.key.cmp(&b.key)));
    picked.truncate(limit);
    ContextReport {
        project: project.to_owned(),
        since,
        sessions: picked
            .into_iter()
            .map(|s| ContextSession {
                key: s.key.clone(),
                harness: s.harness.clone(),
                title: s.title.clone(),
                cwd: s.cwd.clone(),
                updated_at: s.updated_at,
                preview: s.preview.as_deref().map(short).filter(|p| !p.is_empty()),
                cost_usd: s.usage.as_ref().and_then(|u| u.cost_usd),
            })
            .collect(),
    }
}

/// One line per session plus how to read them; empty when there are none, so a SessionStart
/// hook adds nothing.
pub fn markdown(r: &ContextReport) -> String {
    if r.sessions.is_empty() {
        return String::new();
    }
    let mut out = format!("## Recent agent sessions in {}\n\n", r.project);
    for s in &r.sessions {
        let title = s.title.as_deref().map_or_else(|| "(untitled)".to_owned(), short);
        let mut line = format!("- {title} · {} · {} · `{}`", s.harness, local_time(s.updated_at), s.key);
        if let Some(p) = &s.preview {
            line.push_str(&format!(" · \"{}\"", p.replace('"', "'")));
        }
        line.push_str(&format!(" · {}", money(s.cost_usd)));
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(
        "\nRead one in full with the uniflo MCP tool `uniflo_session` (pass the key), or `uniflo show <key>` / \
         `uniflo tail <key> -n 100`; search all transcripts with `uniflo_search` or `uniflo grep <terms>`.\n",
    );
    out
}

/// At most [`PREVIEW_CHARS`] characters, the ellipsis included.
fn short(s: &str) -> String {
    let p = preview(s, PREVIEW_CHARS);
    if p.chars().count() > PREVIEW_CHARS { preview(s, PREVIEW_CHARS - 1) } else { p }
}

fn local_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".into())
}

fn money(c: Option<f64>) -> String {
    match c {
        None => "cost unknown".into(),
        Some(c) if c != 0.0 && c.abs() < 0.01 => format!("${c:.4}"),
        Some(c) => format!("${c:.2}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniflo_schema::{SessionUsage, Status};

    fn s(key: &str, cwd: &str, updated_at: i64) -> Session {
        Session {
            key: key.into(),
            harness: "claude".into(),
            id: key.into(),
            parent: None,
            title: Some(format!("t-{key}")),
            cwd: Some(cwd.into()),
            model: None,
            preview: Some("x".repeat(200)),
            source: String::new(),
            started_at: None,
            updated_at,
            status: Status::Idle,
            status_since: 0,
            status_reason: None,
            pid: None,
            usage: None,
        }
    }

    #[test]
    fn picks_the_project_newest_first() {
        let t = tempfile::tempdir().unwrap();
        let proj = t.path().join("p");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        let p = proj.display().to_string();
        let mut a = s("a", &format!("{p}/sub"), 30);
        a.usage = Some(Box::new(SessionUsage { cost_usd: Some(1.5), ..Default::default() }));
        let mut child = s("child", &p, 40);
        child.parent = Some("a".into());
        let list = vec![s("old", &p, 5), a, s("b", &p, 20), child, s("elsewhere", "/nowhere", 50)];
        let r = report(&list, &p, 10, 5);
        assert_eq!(r.sessions.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(r.sessions[0].cost_usd, Some(1.5));
        assert_eq!(r.sessions[0].preview.as_ref().unwrap().chars().count(), PREVIEW_CHARS, "79 chars + …");
        let md = markdown(&r);
        assert_eq!(md.lines().filter(|l| l.starts_with("- ")).count(), 2);
        assert!(md.contains("`a`") && md.contains("$1.50") && md.contains("uniflo_session"));
        assert_eq!(report(&list, &p, 10, 1).sessions.len(), 1);
        assert_eq!(markdown(&report(&list, "/none", 0, 5)), "");
    }
}
