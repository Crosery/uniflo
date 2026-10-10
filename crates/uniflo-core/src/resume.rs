//! Resume commands: how to continue a session in its own harness CLI.
//!
//! A table keyed by harness id (the CLI's flags are the only harness knowledge here). Nothing is
//! executed in this module: [`resume`] builds argv and shell lines, [`terminal_script`] /
//! [`terminal_argv`] / [`ghostty_argv`] the `open` invocations that open a macOS terminal on one. The session id is validated before
//! it is put into any argv, and every value reaches a shell only through [`sh_quote`] /
//! [`ps_quote`], so no part of a session can become shell syntax.

use uniflo_schema::{ResumeInfo, Session};

/// `harness id → (program, flag, flag=id joined, needs the session's cwd, sub-sessions resumable)`.
const RECIPES: &[(&str, &str, &str, bool, bool, bool)] = &[
    ("claude", "claude", "--resume", false, true, false),
    ("codex", "codex", "resume", false, false, true),
    ("qoder", "qodercli", "--resume", false, true, false),
    ("cursor", "cursor-agent", "--resume", false, false, true),
    ("opencode", "opencode", "--session", false, false, true),
    ("kilo", "kilo", "--session", false, true, true),
    ("pi", "pi", "--session", false, false, true),
    ("omp", "omp", "--resume", false, false, true),
    ("grok", "grok", "--resume", false, false, true),
    ("kimi", "kimi", "--session", false, false, true),
    ("copilot", "copilot", "--resume", true, false, true),
    ("codebuddy", "codebuddy", "--resume", false, true, true),
    ("devin", "devin", "--resume", false, true, true),
    ("hermes", "hermes", "--resume", false, false, true),
    ("antigravity", "agy", "--conversation", true, false, true),
];

pub const INVALID_ID: &str = "会话 id 不合法";

/// Harness ids [`resume`] has a command for.
pub fn supported_harnesses() -> impl Iterator<Item = &'static str> {
    RECIPES.iter().map(|r| r.0)
}

/// At most 200 bytes, an ASCII letter or digit first, then only letters, digits, `_ . - :`.
/// Rejects anything an argv parser could read as an option (`--dangerously-skip-permissions`).
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 200
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"_.-:".contains(c))
}

/// Program and arguments that resume session `id` of `harness`, and whether they must run in
/// the session's cwd. `None`: the harness has no resume command. The id is not validated here.
pub fn argv(harness: &str, id: &str) -> Option<(Vec<String>, bool)> {
    let &(_, program, flag, joined, cwd, _) = RECIPES.iter().find(|r| r.0 == harness)?;
    let args = if joined { vec![format!("{flag}={id}")] } else { vec![flag.to_owned(), id.to_owned()] };
    Some((std::iter::once(program.to_owned()).chain(args).collect(), cwd))
}

pub fn resume(s: &Session) -> ResumeInfo {
    let mut info = ResumeInfo {
        key: s.key.clone(),
        harness: s.harness.clone(),
        supported: false,
        argv: Vec::new(),
        cwd: s.cwd.clone(),
        command: None,
        command_powershell: None,
        reason: None,
    };
    let Some(recipe) = RECIPES.iter().find(|r| r.0 == s.harness) else {
        info.reason = Some(format!("{} 没有从命令行恢复会话的方式", s.harness));
        return info;
    };
    if !valid_id(&s.id) {
        info.reason = Some(INVALID_ID.to_owned());
        return info;
    }
    if !recipe.5
        && let Some(parent) = &s.parent
    {
        info.reason = Some(format!("子代理会话不能单独恢复，请恢复父会话 {parent}"));
        return info;
    }
    let Some((argv, needs_cwd)) = argv(&s.harness, &s.id) else { return info };
    let cwd = match (needs_cwd, s.cwd.as_deref()) {
        (true, None | Some("")) => {
            info.reason = Some(format!("{} 需要在会话的工作目录中恢复，但该会话没有记录 cwd", s.harness));
            return info;
        }
        (true, Some(c)) => Some(c),
        (false, _) => None,
    };
    info.command = Some(shell_line(&argv, cwd));
    info.command_powershell = Some(powershell_line(&argv, cwd));
    info.argv = argv;
    info.supported = true;
    info
}

/// `cd <cwd> && <argv>` (or just `<argv>`), every word POSIX-quoted.
pub fn shell_line(argv: &[String], cwd: Option<&str>) -> String {
    let cmd = argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
    match cwd {
        Some(c) => format!("cd {} && {cmd}", sh_quote(c)),
        None => cmd,
    }
}

pub fn powershell_line(argv: &[String], cwd: Option<&str>) -> String {
    let cmd = format!("& {}", argv.iter().map(|a| ps_quote(a)).collect::<Vec<_>>().join(" "));
    match cwd {
        Some(c) => format!("Set-Location -LiteralPath {} -ErrorAction Stop; {cmd}", ps_quote(c)),
        None => cmd,
    }
}

/// One POSIX shell word: bare when only safe characters, else single-quoted (`'` → `'"'"'`).
/// A leading `=` is quoted too (zsh expands `=cmd`).
pub fn sh_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./:@%+,=-".contains(c);
    if !s.is_empty() && !s.starts_with('=') && s.chars().all(safe) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// One PowerShell single-quoted string. PowerShell also closes such strings on the typographic
/// single quotes, so every one of them is doubled.
pub fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// macOS terminal applications `POST /v1/sessions/{key}/open-terminal` can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Terminal,
    Iterm,
    Ghostty,
}

impl Terminal {
    pub fn parse(s: &str) -> Option<Terminal> {
        match s.to_ascii_lowercase().as_str() {
            "" | "terminal" | "terminal.app" => Some(Terminal::Terminal),
            "iterm" | "iterm2" => Some(Terminal::Iterm),
            "ghostty" => Some(Terminal::Ghostty),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Terminal::Terminal => "terminal",
            Terminal::Iterm => "iterm",
            Terminal::Ghostty => "ghostty",
        }
    }

    /// Bundle names looked up in `/Applications` and `~/Applications`.
    pub fn bundles(self) -> &'static [&'static str] {
        match self {
            Terminal::Terminal => &["Utilities/Terminal.app", "Terminal.app"],
            Terminal::Iterm => &["iTerm.app"],
            Terminal::Ghostty => &["Ghostty.app"],
        }
    }
}

/// Text of the `.command` script Terminal / iTerm run: delete itself and its private directory,
/// run `line`, then stay in an interactive login shell. `line` is already POSIX-quoted
/// ([`shell_line`]) and is the only variable part, so the script needs no Apple events.
pub fn terminal_script(line: &str) -> String {
    format!(
        "#!/bin/sh\nrm -f -- \"$0\"\nrmdir -- \"$(dirname -- \"$0\")\" 2>/dev/null\n{line}\nexec \"${{SHELL:-/bin/zsh}}\" -l\n"
    )
}

/// `open` argv that hands the script at `script` to Terminal or iTerm; `None` for Ghostty, which
/// takes its command through [`ghostty_argv`].
pub fn terminal_argv(t: Terminal, script: &str) -> Option<Vec<String>> {
    let app = match t {
        Terminal::Terminal => "Terminal",
        Terminal::Iterm => "iTerm",
        Terminal::Ghostty => return None,
    };
    Some(["open", "-a", app, script].map(str::to_owned).to_vec())
}

/// `open` argv for a new Ghostty instance running `line` and then a login shell. Every Ghostty
/// flag precedes `-e` (everything after it is the command); `line` is one argv element.
pub fn ghostty_argv(cwd: Option<&str>, line: &str) -> Vec<String> {
    let mut argv: Vec<String> = ["open", "-na", "Ghostty", "--args"].map(str::to_owned).to_vec();
    if let Some(c) = cwd {
        argv.push(format!("--working-directory={c}"));
    }
    argv.extend(["-e", "/bin/sh", "-c"].map(str::to_owned));
    argv.push(format!("{line}\nexec \"${{SHELL:-/bin/zsh}}\" -l"));
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniflo_schema::Status;

    fn session(harness: &str, id: &str, cwd: Option<&str>) -> Session {
        Session {
            key: format!("{harness}:{id}"),
            harness: harness.into(),
            id: id.into(),
            parent: None,
            title: None,
            cwd: cwd.map(str::to_owned),
            model: None,
            preview: None,
            source: String::new(),
            started_at: None,
            updated_at: 0,
            status: Status::Idle,
            status_since: 0,
            status_reason: None,
            pid: None,
            usage: None,
            archived: false,
        }
    }

    #[test]
    fn every_table_row() {
        let want: &[(&str, &[&str], bool)] = &[
            ("claude", &["claude", "--resume", "s1"], true),
            ("codex", &["codex", "resume", "s1"], false),
            ("qoder", &["qodercli", "--resume", "s1"], true),
            ("cursor", &["cursor-agent", "--resume", "s1"], false),
            ("opencode", &["opencode", "--session", "s1"], false),
            ("kilo", &["kilo", "--session", "s1"], true),
            ("pi", &["pi", "--session", "s1"], false),
            ("omp", &["omp", "--resume", "s1"], false),
            ("grok", &["grok", "--resume", "s1"], false),
            ("kimi", &["kimi", "--session", "s1"], false),
            ("copilot", &["copilot", "--resume=s1"], false),
            ("codebuddy", &["codebuddy", "--resume", "s1"], true),
            ("devin", &["devin", "--resume", "s1"], true),
            ("hermes", &["hermes", "--resume", "s1"], false),
            ("antigravity", &["agy", "--conversation=s1"], false),
        ];
        assert_eq!(supported_harnesses().count(), want.len());
        for (h, argv_want, cwd) in want {
            let (a, c) = argv(h, "s1").unwrap();
            assert_eq!(
                (a.as_slice(), c),
                (argv_want.iter().map(|s| s.to_string()).collect::<Vec<_>>().as_slice(), *cwd)
            );
            let r = resume(&session(h, "s1", Some("/w")));
            assert!(r.supported, "{h}: {r:?}");
            assert_eq!(r.argv, a);
        }
        for h in ["dsh", "gemini", "workbuddy", "minimax", "factory", "cline", "craft", "openclaw", "kiro"] {
            let r = resume(&session(h, "s1", Some("/w")));
            assert!(!r.supported && r.argv.is_empty() && r.command.is_none(), "{h}");
            assert!(r.reason.unwrap().starts_with(h));
        }
    }

    #[test]
    fn id_validation() {
        for ok in ["a", "4f1c-9", "01a0f852.SpecAxis", "x:y_z", &"a".repeat(200)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in
            ["", "-x", "--dangerously-skip-permissions", "_a", "a b", "a;b", "a/b", "a'b", "a$b", &"a".repeat(201)]
        {
            assert!(!valid_id(bad), "{bad}");
        }
        let r = resume(&session("claude", "--dangerously-skip-permissions", Some("/w")));
        assert_eq!((r.supported, r.reason.as_deref()), (false, Some(INVALID_ID)));
        assert!(r.argv.is_empty());
    }

    #[test]
    fn cwd_rules_and_quoting() {
        let r = resume(&session("claude", "s1", Some("/tmp/it's a dir")));
        assert_eq!(r.command.as_deref(), Some(r#"cd '/tmp/it'"'"'s a dir' && claude --resume s1"#));
        assert_eq!(
            r.command_powershell.as_deref(),
            Some("Set-Location -LiteralPath '/tmp/it''s a dir' -ErrorAction Stop; & 'claude' '--resume' 's1'")
        );
        let r = resume(&session("codex", "s1", Some("/tmp/x y")));
        assert_eq!((r.command.as_deref(), r.cwd.as_deref()), (Some("codex resume s1"), Some("/tmp/x y")));
        let r = resume(&session("claude", "s1", None));
        assert!(!r.supported && r.reason.unwrap().contains("cwd"));
        let mut sub = session("claude", "agent-1", Some("/w"));
        sub.parent = Some("claude:root".into());
        assert!(!resume(&sub).supported);
        sub.harness = "pi".into();
        assert!(resume(&sub).supported, "pi forks are sessions of their own");

        assert_eq!(sh_quote("abc-1.2:x"), "abc-1.2:x");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("=x"), "'=x'");
        assert_eq!(sh_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(ps_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
    }

    #[test]
    fn terminal_launch_needs_no_apple_events() {
        let line = shell_line(&["claude".into(), "--resume".into(), "s1".into()], Some("/tmp/it's $HOME \"x\""));
        let script = terminal_script(&line);
        assert_eq!(
            script,
            format!(
                "#!/bin/sh\nrm -f -- \"$0\"\nrmdir -- \"$(dirname -- \"$0\")\" 2>/dev/null\n{line}\nexec \"${{SHELL:-/bin/zsh}}\" -l\n"
            )
        );
        assert!(script.contains(r#"cd '/tmp/it'"'"'s $HOME "x"' && claude --resume s1"#), "{script}");
        assert_eq!(
            terminal_argv(Terminal::Terminal, "/t/a.command").unwrap(),
            ["open", "-a", "Terminal", "/t/a.command"]
        );
        assert_eq!(terminal_argv(Terminal::Iterm, "/t/a.command").unwrap(), ["open", "-a", "iTerm", "/t/a.command"]);
        assert_eq!(terminal_argv(Terminal::Ghostty, "/t/a.command"), None);
        let g = ghostty_argv(Some("/w d"), &line);
        assert_eq!(g[..5], ["open", "-na", "Ghostty", "--args", "--working-directory=/w d"]);
        assert_eq!(g[5..8], ["-e", "/bin/sh", "-c"]);
        assert_eq!(g.len(), 9, "the command is one argv element");
        assert_eq!(g[8], format!("{line}\nexec \"${{SHELL:-/bin/zsh}}\" -l"));
        assert_eq!(ghostty_argv(None, "x")[4], "-e");
        assert_eq!(Terminal::parse("iTerm2"), Some(Terminal::Iterm));
        assert_eq!(Terminal::parse("xterm"), None);
    }
}
