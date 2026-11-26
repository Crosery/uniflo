//! `uniflo message`: optional cross-agent messaging CLI & silent harness hook (ADR-0012).

use crate::client::Client;
use crate::i18n::tr;
use crate::{Cli, enc};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use uniflo_schema::{AckMessagesResponse, AgentMessage, InboxResponse, SendMessageResponse};

#[derive(Args)]
pub struct MessageArgs {
    #[command(subcommand)]
    pub action: MessageAction,
}

#[derive(Subcommand)]
pub enum MessageAction {
    /// Send a message to another local agent session (by key, unique prefix, or `cwd:<dir>`).
    Send {
        /// Recipient session key (`claude:<id>`), unique prefix, or `cwd:<dir>` / path.
        to: String,
        /// Message text.
        #[arg(required = true)]
        text: Vec<String>,
        /// Sender session key or label.
        #[arg(long)]
        from: Option<String>,
        /// Optional message kind tag.
        #[arg(long)]
        kind: Option<String>,
        /// Optional message id this replies to.
        #[arg(long)]
        reply_to: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read messages in an inbox (defaults to unread messages).
    Inbox {
        /// Filter by recipient session key, prefix, or `cwd:<dir>`.
        #[arg(long)]
        to: Option<String>,
        /// Filter by recipient working directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Filter by sender.
        #[arg(long)]
        from: Option<String>,
        /// Include already acknowledged messages.
        #[arg(long)]
        all: bool,
        /// Only messages not yet delivered to a hook.
        #[arg(long)]
        undelivered: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Mark returned messages as acknowledged.
        #[arg(long)]
        ack: bool,
        #[arg(long)]
        json: bool,
    },
    /// Mark messages as acknowledged (or `--delivered`).
    Ack {
        /// Message ids (`m-1`, `m-2`, …).
        ids: Vec<String>,
        /// Mark all pending messages for this recipient session.
        #[arg(long)]
        to: Option<String>,
        /// Mark as delivered instead of acknowledged.
        #[arg(long)]
        delivered: bool,
        #[arg(long)]
        json: bool,
    },
    /// Stream incoming messages in real time over SSE (`/v1/messages/stream`, zero polling).
    Watch {
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Silent harness hook entrypoint: pulls undelivered messages and prints `<uniflo-messages>` only if non-empty.
    Hook {
        /// Harness id used to form `<harness>:<session_id>` from hook stdin JSON (e.g. `claude`).
        #[arg(long)]
        harness: Option<String>,
        /// Explicit recipient session key.
        #[arg(long)]
        session: Option<String>,
        /// Explicit working directory fallback.
        #[arg(long)]
        cwd: Option<String>,
    },
}

pub fn run(cli: &Cli, a: &MessageArgs) -> Result<()> {
    match &a.action {
        MessageAction::Send { to, text, from, kind, reply_to, json } => {
            let c = Client::new(&cli.url, cli.token.clone())?;
            let body = json!({
                "to": to,
                "from": from,
                "text": text.join(" "),
                "kind": kind,
                "reply_to": reply_to,
            });
            let resp: SendMessageResponse = c.write_json("POST", "/v1/messages", Some(&body))?;
            if *json {
                println!("{}", serde_json::to_string(&resp)?);
            } else {
                println!(
                    "{} -> {} ({}, delivery={})",
                    resp.message.id,
                    resp.target.key,
                    resp.target.status.as_str(),
                    resp.target.delivery
                );
                if resp.target.delivery == "resume"
                    && let Some(cmd) = &resp.target.resume_command
                {
                    println!("{}{cmd}", tr!("恢复唤醒命令：", "resume command: "));
                }
            }
            Ok(())
        }
        MessageAction::Inbox { to, cwd, from, all, undelivered, limit, ack, json } => {
            let c = Client::new(&cli.url, cli.token.clone())?;
            let path = inbox_path(to.as_deref(), cwd.as_deref(), from.as_deref(), !*all, *undelivered, *limit);
            let resp: InboxResponse = c.get_json(&path)?;
            if *ack && !resp.messages.is_empty() {
                let ids: Vec<String> = resp.messages.iter().map(|m| m.id.clone()).collect();
                let _: AckMessagesResponse =
                    c.write_json("POST", "/v1/messages/ack", Some(&json!({ "ids": ids, "state": "acked" })))?;
            }
            if *json {
                println!("{}", serde_json::to_string(&resp)?);
            } else if resp.messages.is_empty() {
                println!("{}", tr!("收件箱为空", "inbox empty"));
            } else {
                for m in &resp.messages {
                    println!("{}", format_line(m));
                }
            }
            Ok(())
        }
        MessageAction::Ack { ids, to, delivered, json } => {
            if ids.is_empty() && to.is_none() {
                bail!(tr!("请提供消息 id 或 --to <会话>", "provide message ids or --to <session>"));
            }
            let c = Client::new(&cli.url, cli.token.clone())?;
            let state = if *delivered { "delivered" } else { "acked" };
            let resp: AckMessagesResponse =
                c.write_json("POST", "/v1/messages/ack", Some(&json!({ "ids": ids, "to": to, "state": state })))?;
            if *json {
                println!("{}", serde_json::to_string(&resp)?);
            } else {
                println!("{}", tr!("已更新 {} 条消息（{state}）", "updated {} messages ({state})", resp.updated));
            }
            Ok(())
        }
        MessageAction::Watch { to, cwd, json } => watch(cli, to.as_deref(), cwd.as_deref(), *json),
        MessageAction::Hook { harness, session, cwd } => {
            // Hooks must never fail or block the host agent when the daemon is offline.
            let _ = run_hook(cli, harness.as_deref(), session.as_deref(), cwd.as_deref());
            Ok(())
        }
    }
}

fn inbox_path(
    to: Option<&str>,
    cwd: Option<&str>,
    from: Option<&str>,
    unread: bool,
    undelivered: bool,
    limit: usize,
) -> String {
    let mut qs = vec![format!("limit={limit}")];
    if let Some(t) = to {
        qs.push(format!("to={}", enc(t)));
    }
    if let Some(c) = cwd {
        qs.push(format!("cwd={}", enc(c)));
    }
    if let Some(f) = from {
        qs.push(format!("from={}", enc(f)));
    }
    if unread {
        qs.push("unread=true".into());
    }
    if undelivered {
        qs.push("undelivered=true".into());
    }
    format!("/v1/messages?{}", qs.join("&"))
}

fn format_line(m: &AgentMessage) -> String {
    let tag = m.kind.as_deref().map(|k| format!("[{k}] ")).unwrap_or_default();
    let state = if m.acked_at.is_some() {
        "acked"
    } else if m.delivered_at.is_some() {
        "delivered"
    } else {
        "new"
    };
    format!("{} ({state}) {} -> {}: {tag}{}", m.id, m.from, m.to, m.text)
}

fn run_hook(cli: &Cli, harness: Option<&str>, session: Option<&str>, cwd: Option<&str>) -> Result<()> {
    let stdin_json = read_hook_stdin();
    let session_key = session.map(str::to_owned).or_else(|| {
        let sid = stdin_json.as_ref().and_then(|v| v.get("session_id")).and_then(Value::as_str)?;
        if sid.contains(':') { Some(sid.to_owned()) } else { harness.map(|h| format!("{h}:{sid}")) }
    });
    let dir = cwd
        .map(str::to_owned)
        .or_else(|| stdin_json.as_ref().and_then(|v| v.get("cwd")).and_then(Value::as_str).map(str::to_owned))
        .or_else(|| std::env::var("CLAUDE_PROJECT_DIR").ok().filter(|s| !s.is_empty()));

    if session_key.is_none() && dir.is_none() {
        return Ok(());
    }

    let c = Client::new(&cli.url, cli.token.clone())?;
    let path = if let Some(ref key) = session_key {
        inbox_path(Some(key), None, None, false, true, 20)
    } else {
        inbox_path(None, dir.as_deref(), None, false, true, 20)
    };
    let resp: InboxResponse = c.get_json(&path)?;
    if resp.messages.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = resp.messages.iter().map(|m| m.id.clone()).collect();
    let _ = c.write_json::<AckMessagesResponse>(
        "POST",
        "/v1/messages/ack",
        Some(&json!({ "ids": ids, "state": "delivered" })),
    );

    println!("<uniflo-messages>");
    for m in &resp.messages {
        let reply = m.reply_to.as_deref().map(|r| format!(" reply_to={r}")).unwrap_or_default();
        println!("[{} from={}{reply}] {}", m.id, m.from, m.text);
    }
    println!("</uniflo-messages>");
    Ok(())
}

fn read_hook_stdin() -> Option<Value> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(64 * 1024).read_to_end(&mut buf);
    if buf.is_empty() {
        return None;
    }
    serde_json::from_slice(&buf).ok()
}

fn watch(cli: &Cli, to: Option<&str>, cwd: Option<&str>, as_json: bool) -> Result<()> {
    let rest = cli
        .url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!(tr!("只支持 http:// 地址", "only http:// URLs are supported")))?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse::<u16>()?),
        None => (hostport.to_owned(), 80),
    };
    let mut qs = Vec::new();
    if let Some(t) = to {
        qs.push(format!("to={}", enc(t)));
    }
    if let Some(c) = cwd {
        qs.push(format!("cwd={}", enc(c)));
    }
    let path =
        if qs.is_empty() { "/v1/messages/stream".to_owned() } else { format!("/v1/messages/stream?{}", qs.join("&")) };
    let addr = format!("{host}:{port}");
    let sock = std::net::ToSocketAddrs::to_socket_addrs(&addr)?
        .next()
        .ok_or_else(|| anyhow::anyhow!(tr!("无法解析 {addr}", "resolve {addr}")))?;
    let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(2))?;
    s.set_nodelay(true)?;
    let auth = cli.token.as_ref().map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Accept: text/event-stream\r\n\r\n")?;
    s.flush()?;
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if !line.contains(" 200 ") {
        bail!("{}", line.trim());
    }
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut out = std::io::stdout().lock();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if let Some(data) = trimmed.strip_prefix("data:") {
            let data = data.trim_start();
            if as_json {
                writeln!(out, "{data}")?;
            } else if let Ok(m) = serde_json::from_str::<AgentMessage>(data) {
                writeln!(out, "{}", format_line(&m))?;
            }
            out.flush()?;
        }
    }
    Ok(())
}
