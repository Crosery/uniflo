//! Terminal look and feel: colors, the banner and key-driven pickers.
//!
//! Everything degrades to plain text. Colors need a terminal on stdout and no `NO_COLOR`; the
//! pickers need a real unix terminal on stdin and stdout and otherwise leave the caller's
//! line-based prompts in charge. Unit tests always see plain text.

use crate::i18n::tr;
use crate::update::width;
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

/// Whether to emit ANSI colors.
pub fn color() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !cfg!(test) && io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
    })
}

/// Whether the arrow-key pickers can run: a real unix terminal on both ends, not a dumb one.
pub fn available() -> bool {
    cfg!(unix)
        && !cfg!(test)
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var("TERM").is_ok_and(|t| t != "dumb")
}

fn paint(on: bool, code: &str, s: &str) -> String {
    if on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_owned() }
}

/// Palette on the 256-color cube: it works in Terminal.app, iTerm, Windows Terminal and over ssh.
macro_rules! style {
    ($name:ident, $code:expr) => {
        pub fn $name(s: &str) -> String {
            paint(color(), $code, s)
        }
    };
}
style!(accent, "1;38;5;81");
style!(good, "38;5;78");
style!(warn, "38;5;221");
style!(bad, "38;5;203");
style!(dim, "2");
style!(bold, "1");
style!(pink, "1;38;5;213");

/// Cyan → blue → violet → pink across the letters.
const GRADIENT: [u8; 6] = [51, 45, 75, 105, 141, 213];

/// The wordmark with a version and a one-line tagline, for the commands people sit in front of.
pub fn banner(version: &str) -> String {
    let word: String = "uniflo"
        .chars()
        .enumerate()
        .map(|(i, c)| paint(color(), &format!("1;38;5;{}", GRADIENT[i % GRADIENT.len()]), &c.to_string()))
        .collect();
    let tagline = tr!("所有 agent 的会话，本机统一读取", "every agent session, one local gateway");
    format!("\n  {} {}  {}  {}\n", pink("◆"), word, dim(&format!("v{version}")), dim(&tagline))
}

/// Display columns of `s` without its ANSI color sequences.
fn visible_width(s: &str) -> usize {
    let mut plain = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    width(&plain)
}

/// A two-line box for notices (update available, …) on stderr.
pub fn note(icon: &str, headline: &str, detail: &str) -> String {
    let (l1, l2) = (format!("{icon} {headline}"), format!("  {detail}"));
    let inner = visible_width(&l1).max(visible_width(&l2));
    let row = |s: &str| format!("  {} {s}{} {}", dim("│"), " ".repeat(inner - visible_width(s)), dim("│"));
    format!(
        "  {}\n{}\n{}\n  {}",
        dim(&format!("╭{}╮", "─".repeat(inner + 2))),
        row(&l1),
        row(&l2),
        dim(&format!("╰{}╯", "─".repeat(inner + 2))),
    )
}

// ---------------------------------------------------------------- pickers

/// Only the unix terminal reader produces keys; elsewhere the pickers are never offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Key {
    Up,
    Down,
    Space,
    Enter,
    Esc,
    Char(char),
}

/// Keys in one `read()` chunk: arrows arrive as `ESC [ A`, a lone `ESC` is the Escape key.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn parse_keys(buf: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x1b if matches!(buf.get(i + 1), Some(b'[' | b'O')) => {
                match buf.get(i + 2) {
                    Some(b'A') => keys.push(Key::Up),
                    Some(b'B') => keys.push(Key::Down),
                    _ => {}
                }
                i += 3;
                continue;
            }
            0x1b | 0x03 | 0x04 => keys.push(Key::Esc),
            b'\r' | b'\n' => keys.push(Key::Enter),
            b' ' => keys.push(Key::Space),
            b => keys.push(Key::Char(b as char)),
        }
        i += 1;
    }
    keys
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub hint: String,
}

impl Item {
    pub fn new(label: impl Into<String>, hint: impl Into<String>) -> Item {
        Item { label: label.into(), hint: hint.into() }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Continue,
    Done,
    Cancel,
}

/// A list with a cursor; `checks` makes it a multi-select.
pub struct Picker {
    pub items: Vec<Item>,
    pub cur: usize,
    pub checks: Option<Vec<bool>>,
}

impl Picker {
    pub fn select(items: Vec<Item>, cur: usize) -> Picker {
        let cur = cur.min(items.len().saturating_sub(1));
        Picker { items, cur, checks: None }
    }

    pub fn multi(items: Vec<Item>, checked: Vec<bool>) -> Picker {
        Picker { checks: Some(checked), items, cur: 0 }
    }

    pub fn handle(&mut self, key: Key) -> Step {
        let n = self.items.len();
        match key {
            Key::Up | Key::Char('k') if n > 0 => self.cur = (self.cur + n - 1) % n,
            Key::Down | Key::Char('j') if n > 0 => self.cur = (self.cur + 1) % n,
            Key::Space => {
                if let Some(c) = self.checks.as_mut() {
                    c[self.cur] = !c[self.cur];
                }
            }
            Key::Char('a') => {
                if let Some(c) = self.checks.as_mut() {
                    let all = c.iter().all(|x| *x);
                    c.iter_mut().for_each(|x| *x = !all);
                }
            }
            Key::Char(d @ '1'..='9') if self.checks.is_none() => {
                let i = d as usize - '1' as usize;
                if i < n {
                    self.cur = i;
                    return Step::Done;
                }
            }
            Key::Enter => return Step::Done,
            Key::Esc | Key::Char('q') => return Step::Cancel,
            _ => {}
        }
        Step::Continue
    }

    /// Lines to draw (no trailing newline), clipped to `cols` display columns.
    pub fn render(&self, title: &str, cols: usize) -> Vec<String> {
        let mut out = vec![format!("{} {}", accent("?"), bold(title))];
        let label_w = self.items.iter().map(|i| width(&i.label)).max().unwrap_or(0);
        for (n, it) in self.items.iter().enumerate() {
            let here = n == self.cur;
            let mark = match &self.checks {
                Some(c) if c[n] => good("◉"),
                Some(_) => dim("○"),
                None => String::new(),
            };
            let pointer = if here { accent("❯") } else { " ".into() };
            let label = if here { accent(&it.label) } else { it.label.clone() };
            let used = 4 + if self.checks.is_some() { 2 } else { 0 } + label_w + 2;
            let hint = clip(&it.hint, cols.saturating_sub(used));
            let pad = " ".repeat(label_w.saturating_sub(width(&it.label)) + 2);
            let mark = if mark.is_empty() { mark } else { format!("{mark} ") };
            out.push(format!("  {pointer} {mark}{label}{pad}{}", dim(&hint)));
        }
        let help = if self.checks.is_some() {
            tr!(
                "↑↓ 移动 · 空格勾选 · a 全选/清空 · 回车确认 · Esc 取消",
                "↑↓ move · space toggle · a all/none · enter confirm · esc cancel"
            )
        } else {
            tr!("↑↓ 移动 · 回车确认 · Esc 取消", "↑↓ move · enter confirm · esc cancel")
        };
        out.push(format!("  {}", dim(&clip(&help, cols.saturating_sub(2)))));
        out
    }

    /// What the finished prompt collapses to.
    pub fn summary(&self, title: &str) -> String {
        let chosen = match &self.checks {
            Some(c) => {
                let names: Vec<&str> =
                    self.items.iter().zip(c).filter(|(_, on)| **on).map(|(i, _)| i.label.as_str()).collect();
                if names.is_empty() { tr!("（无）", "(none)") } else { names.join(", ") }
            }
            None => self.items.get(self.cur).map(|i| i.label.clone()).unwrap_or_default(),
        };
        format!("{} {} {}", good("✔"), bold(title), accent(&chosen))
    }
}

/// `text` cut to `max` display columns, with `…` when something was dropped.
pub fn clip(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = width(&c.to_string());
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// Pick one: `Ok(None)` when cancelled, `Err` when the terminal cannot be driven.
pub fn select(title: &str, items: Vec<Item>, cur: usize) -> io::Result<Option<usize>> {
    let mut p = Picker::select(items, cur);
    Ok(drive(&mut p, title)?.then_some(p.cur))
}

/// Pick any number; `Ok(None)` when cancelled.
pub fn multiselect(title: &str, items: Vec<Item>, checked: Vec<bool>) -> io::Result<Option<Vec<bool>>> {
    let mut p = Picker::multi(items, checked);
    Ok(drive(&mut p, title)?.then(|| p.checks.unwrap_or_default()))
}

/// Yes/no on one line, answered with a single key; Enter takes the default. `Ok(None)` = cancelled.
pub fn confirm(question: &str, default: bool) -> io::Result<Option<bool>> {
    let _raw = raw::Raw::enter()?;
    let mut out = io::stdout().lock();
    let hint = if default { "Y/n" } else { "y/N" };
    write!(out, "\x1b[?25l{} {} {}", accent("?"), bold(question), dim(&format!("({hint})")))?;
    out.flush()?;
    let answer = loop {
        let Some(keys) = raw::read_keys() else { break None };
        let hit = keys.into_iter().find_map(|k| match k {
            Key::Enter => Some(Some(default)),
            Key::Char('y' | 'Y') => Some(Some(true)),
            Key::Char('n' | 'N') => Some(Some(false)),
            Key::Esc => Some(None),
            _ => None,
        });
        if let Some(a) = hit {
            break a;
        }
    };
    let shown = match answer {
        Some(true) => good(&tr!("是", "yes")),
        Some(false) => bad(&tr!("否", "no")),
        None => dim(&tr!("已取消", "cancelled")),
    };
    write!(
        out,
        "\r\x1b[2K{} {} {shown}\n\x1b[?25h",
        if answer.is_some() { good("✔") } else { dim("·") },
        bold(question)
    )?;
    out.flush()?;
    Ok(answer)
}

/// Run the picker until Enter (`true`) or cancel (`false`). The prompt collapses to a summary line.
fn drive(p: &mut Picker, title: &str) -> io::Result<bool> {
    let _raw = raw::Raw::enter()?;
    let mut out = io::stdout().lock();
    write!(out, "\x1b[?25l")?;
    let mut drawn = 0usize;
    let done = loop {
        let lines = p.render(title, raw::columns());
        if drawn > 0 {
            write!(out, "\x1b[{drawn}A")?;
        }
        write!(out, "\r\x1b[J")?;
        for l in &lines {
            writeln!(out, "{l}")?;
        }
        out.flush()?;
        drawn = lines.len();
        let Some(keys) = raw::read_keys() else { break false };
        let mut finished = None;
        for k in keys {
            match p.handle(k) {
                Step::Continue => {}
                Step::Done => finished = Some(true),
                Step::Cancel => finished = Some(false),
            }
            if finished.is_some() {
                break;
            }
        }
        if let Some(f) = finished {
            break f;
        }
    };
    write!(out, "\x1b[{drawn}A\r\x1b[J")?;
    let line = if done {
        p.summary(title)
    } else {
        format!("{} {} {}", dim("·"), bold(title), dim(&tr!("已取消", "cancelled")))
    };
    writeln!(out, "{line}\x1b[?25h")?;
    out.flush()?;
    Ok(done)
}

#[cfg(unix)]
mod raw {
    use super::{Key, parse_keys};
    use std::io;

    /// The terminal in raw-ish mode (no line buffering, no echo, no signals) until dropped.
    pub struct Raw {
        saved: libc::termios,
    }

    impl Raw {
        pub fn enter() -> io::Result<Raw> {
            // SAFETY: plain termios calls on fd 0 with a zeroed struct that tcgetattr fills in.
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let saved = t;
                t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
                t.c_iflag &= !(libc::ICRNL | libc::IXON);
                t.c_cc[libc::VMIN] = 1;
                t.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(0, libc::TCSADRAIN, &t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Raw { saved })
            }
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            // SAFETY: restores the attributes read in `enter`.
            unsafe { libc::tcsetattr(0, libc::TCSADRAIN, &self.saved) };
        }
    }

    /// Next chunk of keys; `None` at end of input.
    pub fn read_keys() -> Option<Vec<Key>> {
        let mut buf = [0u8; 16];
        // SAFETY: reads at most `buf.len()` bytes into `buf`.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        (n > 0).then(|| parse_keys(&buf[..n as usize]))
    }

    pub fn columns() -> usize {
        // SAFETY: TIOCGWINSZ fills a winsize; a failure leaves the zeroed struct.
        unsafe {
            let mut w: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut w) == 0 && w.ws_col > 0 { w.ws_col as usize } else { 80 }
        }
    }
}

#[cfg(not(unix))]
mod raw {
    use super::Key;
    use std::io;

    pub struct Raw;

    impl Raw {
        pub fn enter() -> io::Result<Raw> {
            Err(io::Error::other("no raw terminal"))
        }
    }

    pub fn read_keys() -> Option<Vec<Key>> {
        None
    }

    pub fn columns() -> usize {
        80
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Item> {
        vec![Item::new("claude", "Claude Code"), Item::new("codex", "Codex"), Item::new("cursor", "Cursor")]
    }

    #[test]
    fn keys_parse_arrows_escape_and_letters() {
        assert_eq!(parse_keys(b"\x1b[A\x1b[B"), [Key::Up, Key::Down]);
        assert_eq!(parse_keys(b"\x1b"), [Key::Esc]);
        assert_eq!(parse_keys(b"\x03"), [Key::Esc]);
        assert_eq!(parse_keys(b" \r"), [Key::Space, Key::Enter]);
        assert_eq!(parse_keys(b"j"), [Key::Char('j')]);
        assert_eq!(parse_keys(b"\x1bOB"), [Key::Down]);
    }

    #[test]
    fn select_moves_wraps_and_confirms() {
        let mut p = Picker::select(items(), 0);
        assert_eq!(p.handle(Key::Up), Step::Continue);
        assert_eq!(p.cur, 2, "wraps to the last");
        p.handle(Key::Down);
        assert_eq!(p.cur, 0);
        assert_eq!(p.handle(Key::Char('2')), Step::Done, "a digit picks directly");
        assert_eq!(p.cur, 1);
        assert_eq!(p.handle(Key::Esc), Step::Cancel);
    }

    #[test]
    fn multiselect_toggles_and_flips_all() {
        let mut p = Picker::multi(items(), vec![true, true, false]);
        p.handle(Key::Down);
        p.handle(Key::Space);
        assert_eq!(p.checks, Some(vec![true, false, false]));
        p.handle(Key::Char('a'));
        assert_eq!(p.checks, Some(vec![true, true, true]));
        p.handle(Key::Char('a'));
        assert_eq!(p.checks, Some(vec![false, false, false]), "all on flips to all off");
        assert_eq!(p.handle(Key::Char('2')), Step::Continue, "digits do not jump in a multi-select");
    }

    #[test]
    fn rendering_is_plain_text_without_a_terminal() {
        let mut p = Picker::multi(items(), vec![true, false, true]);
        p.cur = 1;
        let lines = p.render("harnesses", 80);
        assert_eq!(lines[0], "? harnesses");
        assert_eq!(lines[1], "    ◉ claude  Claude Code");
        assert_eq!(lines[2], "  ❯ ○ codex   Codex");
        assert!(lines[4].contains("↑↓") && !lines.iter().any(|l| l.contains('\x1b')));
        assert_eq!(p.summary("harnesses"), "✔ harnesses claude, cursor");
        let s = Picker::select(items(), 1);
        assert_eq!(s.summary("how"), "✔ how codex");
    }

    #[test]
    fn hints_are_clipped_to_the_terminal_width() {
        assert_eq!(clip("abcdef", 6), "abcdef");
        assert_eq!(clip("abcdefgh", 5), "abcd…");
        assert_eq!(clip("中文提示文字", 7), "中文提…", "wide characters count two columns");
        let p = Picker::select(vec![Item::new("a", "x".repeat(200))], 0);
        assert!(p.render("t", 40).iter().all(|l| width(l) <= 40));
    }

    #[test]
    fn banner_and_note_carry_the_text() {
        assert!(banner("0.2.1").contains("uniflo") && banner("0.2.1").contains("v0.2.1"));
        let n = note("✦", "uniflo 0.3.0 可用", "运行 uniflo update");
        assert!(n.contains("uniflo 0.3.0 可用") && n.contains("╭") && n.contains("╯"));
        let widths: Vec<usize> = n.lines().map(width).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "box edges line up: {widths:?}");
        // a colored icon must not widen the first row
        let n = note("\x1b[1;38;5;213m✦\x1b[0m", "uniflo 0.3.0 可用", "运行 uniflo update");
        let widths: Vec<usize> = n.lines().map(visible_width).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "colored icon: {widths:?}");
    }
}
