//! Edits of harness configuration files: only the `uniflo` entry (or Uniflo's own SessionStart
//! hook) changes; key order, other entries and the indentation style are kept. A file that does
//! not parse is reported and left alone. Writes back the file up first and replace it atomically.

use super::harness::Shape;
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const NAME: &str = "uniflo";

/// What an edit would do to one file.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Unchanged,
    /// New full text; `created` lists the containers (or `"file"`) this edit adds.
    Write {
        text: String,
        created: Vec<String>,
    },
    /// The `uniflo` name holds an entry Uniflo did not write.
    Occupied,
    /// The file does not parse (comments, trailing commas…) or has an unexpected shape.
    Invalid(String),
}

/// Uniflo's stdio entry for `bin` in a harness's JSON shape.
pub fn entry(shape: Shape, bin: &str) -> Value {
    match shape {
        Shape::Plain => json!({ "command": bin, "args": ["mcp"] }),
        Shape::Stdio => json!({ "type": "stdio", "command": bin, "args": ["mcp"] }),
        Shape::Copilot => json!({ "type": "local", "command": bin, "args": ["mcp"], "tools": ["*"] }),
        Shape::OpenCode => json!({ "type": "local", "command": [bin, "mcp"], "enabled": true }),
    }
}

/// An entry that runs `<…/uniflo> mcp`, in any of the shapes.
pub fn is_ours(v: &Value) -> bool {
    let words: Vec<&str> = match (&v["command"], &v["args"]) {
        (Value::String(c), Value::Array(a)) => {
            std::iter::once(c.as_str()).chain(a.iter().filter_map(Value::as_str)).collect()
        }
        (Value::Array(c), Value::Null) => c.iter().filter_map(Value::as_str).collect(),
        _ => return false,
    };
    matches!(words.as_slice(), [program, "mcp"] if is_uniflo(program))
}

pub fn is_uniflo(program: &str) -> bool {
    let p = program.trim_matches(['\'', '"']);
    let name = Path::new(p).file_name().and_then(|n| n.to_str()).unwrap_or(p);
    name == "uniflo" || name == "uniflo.exe"
}

/// The program of an entry [`is_ours`].
pub fn program(v: &Value) -> Option<&str> {
    match &v["command"] {
        Value::String(c) => Some(c),
        Value::Array(c) => c.first().and_then(Value::as_str),
        _ => None,
    }
}

fn parse_json(text: Option<&str>) -> Result<Map<String, Value>, String> {
    let Some(t) = text.filter(|t| !t.trim().is_empty()) else { return Ok(Map::new()) };
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("top level is not a JSON object".into()),
        Err(e) => Err(format!("parse failed: {e}")),
    }
}

/// The file's indentation (first indented line), two spaces when there is none.
fn indent_of(text: Option<&str>) -> Vec<u8> {
    let Some(t) = text else { return b"  ".to_vec() };
    for line in t.lines().skip(1) {
        let ws: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if !ws.is_empty() {
            return if ws.starts_with('\t') { b"\t".to_vec() } else { ws.into_bytes() };
        }
    }
    b"  ".to_vec()
}

fn render_json(m: Map<String, Value>, before: Option<&str>) -> String {
    use serde::Serialize;
    let indent = indent_of(before);
    let mut out = Vec::new();
    let mut ser =
        serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(&indent));
    let _ = Value::Object(m).serialize(&mut ser);
    let mut s = String::from_utf8(out).unwrap_or_default();
    if before.is_none_or(|b| b.ends_with('\n')) {
        s.push('\n');
    }
    s
}

/// Add or refresh `<key>.uniflo` = `want`.
pub fn json_set(text: Option<&str>, key: &str, want: &Value) -> Outcome {
    let mut root = match parse_json(text) {
        Ok(m) => m,
        Err(e) => return Outcome::Invalid(e),
    };
    let mut created = Vec::new();
    if text.is_none() {
        created.push("file".to_owned());
    }
    if !root.contains_key(key) {
        root.insert(key.to_owned(), Value::Object(Map::new()));
        created.push(key.to_owned());
    }
    let Some(servers) = root.get_mut(key).and_then(Value::as_object_mut) else {
        return Outcome::Invalid(format!("`{key}` is not an object"));
    };
    match servers.get(NAME) {
        Some(cur) if cur == want => return Outcome::Unchanged,
        Some(cur) if !is_ours(cur) => return Outcome::Occupied,
        _ => {}
    }
    servers.insert(NAME.to_owned(), want.clone());
    Outcome::Write { text: render_json(root, text), created }
}

/// Remove `<key>.uniflo` when Uniflo wrote it; drop containers in `created` left empty.
pub fn json_remove(text: Option<&str>, key: &str, created: &[String]) -> Outcome {
    let Some(t) = text else { return Outcome::Unchanged };
    let mut root = match parse_json(Some(t)) {
        Ok(m) => m,
        Err(e) => return Outcome::Invalid(e),
    };
    let Some(servers) = root.get_mut(key).and_then(Value::as_object_mut) else { return Outcome::Unchanged };
    match servers.get(NAME) {
        None => return Outcome::Unchanged,
        Some(v) if !is_ours(v) => return Outcome::Occupied,
        _ => {}
    }
    servers.shift_remove(NAME);
    if servers.is_empty() && created.iter().any(|c| c == key) {
        root.shift_remove(key);
    }
    let text =
        if root.is_empty() && created.iter().any(|c| c == "file") { String::new() } else { render_json(root, text) };
    Outcome::Write { text, created: Vec::new() }
}

/// `[mcp_servers.uniflo]` with `command = bin`, `args = ["mcp"]` (Codex without its CLI).
pub fn toml_set(text: Option<&str>, bin: &str) -> Outcome {
    let mut doc: toml_edit::DocumentMut = match text.unwrap_or("").parse() {
        Ok(d) => d,
        Err(e) => return Outcome::Invalid(format!("parse failed: {e}")),
    };
    let mut created = Vec::new();
    if text.is_none() {
        created.push("file".to_owned());
    }
    if !doc.contains_key("mcp_servers") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc.insert("mcp_servers", toml_edit::Item::Table(t));
        created.push("mcp_servers".to_owned());
    }
    let Some(servers) = doc["mcp_servers"].as_table_like_mut() else {
        return Outcome::Invalid("`mcp_servers` is not a table".into());
    };
    if let Some(cur) = servers.get(NAME) {
        match toml_entry(cur) {
            Some(p) if p == bin => return Outcome::Unchanged,
            Some(_) => {}
            None => return Outcome::Occupied,
        }
    }
    let mut t = toml_edit::Table::new();
    t.insert("command", toml_edit::value(bin));
    t.insert("args", toml_edit::value(toml_edit::Array::from_iter(["mcp"])));
    servers.insert(NAME, toml_edit::Item::Table(t));
    Outcome::Write { text: doc.to_string(), created }
}

pub fn toml_remove(text: Option<&str>, created: &[String]) -> Outcome {
    let Some(t) = text else { return Outcome::Unchanged };
    let mut doc: toml_edit::DocumentMut = match t.parse() {
        Ok(d) => d,
        Err(e) => return Outcome::Invalid(format!("parse failed: {e}")),
    };
    let Some(servers) = doc.get_mut("mcp_servers").and_then(toml_edit::Item::as_table_like_mut) else {
        return Outcome::Unchanged;
    };
    match servers.get(NAME) {
        None => return Outcome::Unchanged,
        Some(cur) if toml_entry(cur).is_none() => return Outcome::Occupied,
        _ => {}
    }
    servers.remove(NAME);
    if servers.is_empty() && created.iter().any(|c| c == "mcp_servers") {
        doc.remove("mcp_servers");
    }
    let text = doc.to_string();
    let text = if text.trim().is_empty() && created.iter().any(|c| c == "file") { String::new() } else { text };
    Outcome::Write { text, created: Vec::new() }
}

/// Program of a TOML `uniflo` entry Uniflo wrote.
pub fn toml_entry(item: &toml_edit::Item) -> Option<String> {
    let t = item.as_table_like()?;
    let program = t.get("command")?.as_str()?;
    let args: Vec<&str> = t.get("args")?.as_array()?.iter().filter_map(|v| v.as_str()).collect();
    (args == ["mcp"] && is_uniflo(program)).then(|| program.to_owned())
}

/// Program of the `uniflo` entry in a probe file: `Ok(None)` absent, `Err(())` present but not
/// Uniflo's, `Ok(Some(program))` Uniflo's.
pub fn probe_json(text: &str) -> Option<Result<Option<String>, ()>> {
    let root = parse_json(Some(text)).ok()?;
    Some(match root.get("mcpServers").and_then(|s| s.get(NAME)) {
        None => Ok(None),
        Some(v) if is_ours(v) => Ok(program(v).map(str::to_owned)),
        Some(_) => Err(()),
    })
}

pub fn probe_toml(text: &str) -> Option<Result<Option<String>, ()>> {
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    Some(match doc.get("mcp_servers").and_then(|s| s.get(NAME)) {
        None => Ok(None),
        Some(item) => toml_entry(item).map(Some).ok_or(()),
    })
}

/// The SessionStart hook command Uniflo adds to Claude Code.
pub fn hook_command(bin: &str) -> String {
    format!(
        "{} context --cwd \"$CLAUDE_PROJECT_DIR\" --limit 5 --since 14d 2>/dev/null || true",
        uniflo_core::resume::sh_quote(bin)
    )
}

fn is_our_hook(h: &Value) -> bool {
    h["type"] == "command"
        && h["command"].as_str().is_some_and(|c| {
            c.contains(" context --cwd \"$CLAUDE_PROJECT_DIR\"") && c.split(" context ").next().is_some_and(is_uniflo)
        })
}

/// Add or refresh Uniflo's command in `hooks.SessionStart` of Claude's `settings.json`.
pub fn hook_set(text: Option<&str>, command: &str) -> Outcome {
    let mut root = match parse_json(text) {
        Ok(m) => m,
        Err(e) => return Outcome::Invalid(e),
    };
    let mut created = Vec::new();
    if text.is_none() {
        created.push("file".to_owned());
    }
    if !root.contains_key("hooks") {
        root.insert("hooks".into(), json!({}));
        created.push("hooks".into());
    }
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Outcome::Invalid("`hooks` is not an object".into());
    };
    if !hooks.contains_key("SessionStart") {
        hooks.insert("SessionStart".into(), json!([]));
        created.push("hooks.SessionStart".into());
    }
    let Some(groups) = hooks.get_mut("SessionStart").and_then(Value::as_array_mut) else {
        return Outcome::Invalid("`hooks.SessionStart` is not an array".into());
    };
    let ours = groups
        .iter_mut()
        .filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut))
        .flat_map(|hs| hs.iter_mut())
        .find(|h| is_our_hook(h));
    match ours {
        Some(h) if h["command"] == command => return Outcome::Unchanged,
        Some(h) => h["command"] = json!(command),
        None => groups.push(json!({ "hooks": [{ "type": "command", "command": command }] })),
    }
    Outcome::Write { text: render_json(root, text), created }
}

pub fn hook_remove(text: Option<&str>, created: &[String]) -> Outcome {
    let Some(t) = text else { return Outcome::Unchanged };
    let mut root = match parse_json(Some(t)) {
        Ok(m) => m,
        Err(e) => return Outcome::Invalid(e),
    };
    let Some(groups) = root.get_mut("hooks").and_then(|h| h.get_mut("SessionStart")).and_then(Value::as_array_mut)
    else {
        return Outcome::Unchanged;
    };
    let mut removed = false;
    groups.retain_mut(|g| {
        let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else { return true };
        let n = hs.len();
        hs.retain(|h| !is_our_hook(h));
        removed |= hs.len() < n;
        !(hs.is_empty() && n > 0)
    });
    if !removed {
        return Outcome::Unchanged;
    }
    let had = |k: &str| created.iter().any(|c| c == k);
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        if had("hooks.SessionStart") && hooks["SessionStart"].as_array().is_some_and(Vec::is_empty) {
            hooks.shift_remove("SessionStart");
        }
        if had("hooks") && hooks.is_empty() {
            root.shift_remove("hooks");
        }
    }
    let text = if root.is_empty() && had("file") { String::new() } else { render_json(root, Some(t)) };
    Outcome::Write { text, created: Vec::new() }
}

/// Replace `path` with `text` atomically (temp file + rename, permissions kept). A symlinked
/// config is written through to its target. Returns the backup made of the old content.
pub fn write(path: &Path, text: &str, stamp: &str) -> Result<Option<PathBuf>> {
    let real = if path.is_symlink() { std::fs::canonicalize(path)? } else { path.to_path_buf() };
    let old = std::fs::metadata(&real).ok();
    let backup = match &old {
        Some(_) => {
            let b = backup_path(&real, stamp);
            std::fs::copy(&real, &b).with_context(|| format!("back up {}", real.display()))?;
            Some(b)
        }
        None => None,
    };
    let dir = real.parent().context("no parent directory")?;
    std::fs::create_dir_all(dir)?;
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("config");
    let tmp = dir.join(format!(".{name}.uniflo-tmp-{}", std::process::id()));
    let res = (|| -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        if let Some(md) = &old {
            std::fs::set_permissions(&tmp, md.permissions())?;
        }
        std::fs::rename(&tmp, &real)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.with_context(|| format!("write {}", real.display()))?;
    Ok(backup)
}

/// Delete a file Uniflo created and emptied again, after backing it up like any other change.
pub fn delete(path: &Path, stamp: &str) -> Result<Option<PathBuf>> {
    let b = backup_path(path, stamp);
    std::fs::copy(path, &b)?;
    std::fs::remove_file(path)?;
    Ok(Some(b))
}

/// `<file>.bak-uniflo-<stamp>`, numbered when it already exists.
fn backup_path(p: &Path, stamp: &str) -> PathBuf {
    let base = format!("{}.bak-uniflo-{stamp}", p.display());
    let mut cand = PathBuf::from(&base);
    let mut n = 2;
    while cand.exists() {
        cand = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    cand
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/opt/bin/uniflo";

    #[test]
    fn json_keeps_order_and_indent_and_other_entries() {
        let before = "{\n    \"zeta\": 1,\n    \"mcpServers\": {\n        \"b\": {\"command\": \"x\"},\n        \"a\": {\"command\": \"y\"}\n    },\n    \"alpha\": true\n}\n";
        let Outcome::Write { text, created } = json_set(Some(before), "mcpServers", &entry(Shape::Plain, BIN)) else {
            panic!()
        };
        assert!(created.is_empty());
        let keys: Vec<String> = serde_json::from_str::<Map<String, Value>>(&text).unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "mcpServers", "alpha"]);
        let v: Value = serde_json::from_str(&text).unwrap();
        let servers: Vec<&String> = v["mcpServers"].as_object().unwrap().keys().collect();
        assert_eq!(servers, ["b", "a", "uniflo"]);
        assert!(text.contains("\n    \"zeta\"") && text.ends_with("}\n"));
        assert_eq!(json_set(Some(&text), "mcpServers", &entry(Shape::Plain, BIN)), Outcome::Unchanged);
        let Outcome::Write { text: refreshed, .. } =
            json_set(Some(&text), "mcpServers", &entry(Shape::Plain, "/new/uniflo"))
        else {
            panic!()
        };
        assert!(refreshed.contains("/new/uniflo") && !refreshed.contains(BIN));
        let Outcome::Write { text: back, .. } = json_remove(Some(&text), "mcpServers", &[]) else { panic!() };
        assert_eq!(serde_json::from_str::<Value>(&back).unwrap(), serde_json::from_str::<Value>(before).unwrap());
    }

    #[test]
    fn foreign_entries_comments_and_shapes() {
        let taken = r#"{"mcpServers":{"uniflo":{"command":"npx","args":["other-uniflo"]}}}"#;
        assert_eq!(json_set(Some(taken), "mcpServers", &entry(Shape::Plain, BIN)), Outcome::Occupied);
        assert_eq!(json_remove(Some(taken), "mcpServers", &[]), Outcome::Occupied);
        let commented = "{\n  // mine\n  \"mcpServers\": {}\n}";
        assert!(
            matches!(json_set(Some(commented), "mcpServers", &entry(Shape::Plain, BIN)), Outcome::Invalid(e) if e.starts_with("parse failed"))
        );
        assert!(matches!(json_set(Some(r#"{"mcp":[]}"#), "mcp", &entry(Shape::OpenCode, BIN)), Outcome::Invalid(_)));
        let oc = entry(Shape::OpenCode, BIN);
        assert_eq!(oc["command"], json!([BIN, "mcp"]));
        for s in [Shape::Plain, Shape::Stdio, Shape::Copilot, Shape::OpenCode] {
            assert!(is_ours(&entry(s, BIN)));
        }
        assert!(!is_ours(&json!({"command": "/x/uniflo", "args": ["daemon"]})));
    }

    #[test]
    fn created_containers_and_files_go_away_again() {
        let Outcome::Write { text, created } = json_set(None, "mcpServers", &entry(Shape::Plain, BIN)) else {
            panic!()
        };
        assert_eq!(created, ["file", "mcpServers"]);
        assert_eq!(
            json_remove(Some(&text), "mcpServers", &created),
            Outcome::Write { text: String::new(), created: vec![] }
        );
        let Outcome::Write { text, created } = json_set(Some("{\"x\":1}\n"), "mcpServers", &entry(Shape::Plain, BIN))
        else {
            panic!()
        };
        let Outcome::Write { text: back, .. } = json_remove(Some(&text), "mcpServers", &created) else { panic!() };
        assert_eq!(back, "{\n  \"x\": 1\n}\n");
    }

    #[test]
    fn toml_keeps_formatting() {
        let before = "# my config\nmodel = \"gpt-5\"   # pinned\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let Outcome::Write { text, created } = toml_set(Some(before), BIN) else { panic!() };
        assert!(created.is_empty());
        assert!(text.starts_with(before), "{text}");
        assert!(text.contains("[mcp_servers.uniflo]\ncommand = \"/opt/bin/uniflo\"\nargs = [\"mcp\"]"), "{text}");
        assert_eq!(toml_set(Some(&text), BIN), Outcome::Unchanged);
        assert_eq!(probe_toml(&text), Some(Ok(Some(BIN.to_owned()))));
        let Outcome::Write { text: back, .. } = toml_remove(Some(&text), &[]) else { panic!() };
        assert_eq!(back, before);
        let taken = "[mcp_servers.uniflo]\ncommand = \"npx\"\n";
        assert_eq!(toml_set(Some(taken), BIN), Outcome::Occupied);
        assert_eq!(probe_toml(taken), Some(Err(())));
        let Outcome::Write { text, created } = toml_set(None, BIN) else { panic!() };
        assert!(!text.contains("[mcp_servers]\n"), "implicit parent table: {text}");
        assert_eq!(toml_remove(Some(&text), &created), Outcome::Write { text: String::new(), created: vec![] });
    }

    #[test]
    fn session_start_hook() {
        let before = r#"{"hooks":{"SessionStart":[{"matcher":"startup","hooks":[{"type":"command","command":"echo hi"}]}]},"model":"x"}"#;
        let cmd = hook_command("/opt/my bin/uniflo");
        assert_eq!(
            cmd,
            "'/opt/my bin/uniflo' context --cwd \"$CLAUDE_PROJECT_DIR\" --limit 5 --since 14d 2>/dev/null || true"
        );
        let Outcome::Write { text, created } = hook_set(Some(before), &cmd) else { panic!() };
        assert!(created.is_empty());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(hook_set(Some(&text), &cmd), Outcome::Unchanged);
        let Outcome::Write { text: back, .. } = hook_remove(Some(&text), &created) else { panic!() };
        assert_eq!(serde_json::from_str::<Value>(&back).unwrap(), serde_json::from_str::<Value>(before).unwrap());
        let Outcome::Write { text, created } = hook_set(None, &cmd) else { panic!() };
        assert_eq!(created, ["file", "hooks", "hooks.SessionStart"]);
        assert_eq!(hook_remove(Some(&text), &created), Outcome::Write { text: String::new(), created: vec![] });
    }

    #[test]
    fn atomic_write_with_backup_through_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("dotfiles/mcp.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::os::unix::fs::symlink(&real, t.path().join("link.json")).unwrap();
            let b = write(&t.path().join("link.json"), "{\"a\":1}", "20261009-120000").unwrap().unwrap();
            assert!(t.path().join("link.json").is_symlink(), "the user's symlink survives");
            assert_eq!(std::fs::read_to_string(&real).unwrap(), "{\"a\":1}");
            assert_eq!(std::fs::read_to_string(&b).unwrap(), "{}");
            assert_eq!(std::fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o600);
            let b2 = write(&real, "{}", "20261009-120000").unwrap().unwrap();
            assert!(b2.display().to_string().ends_with("-2"));
        }
        assert_eq!(write(&t.path().join("new/x.json"), "{}", "s").unwrap(), None);
        let leftovers: Vec<_> = std::fs::read_dir(real.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("uniflo-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
