//! Agent-access routes: resume commands, `open-terminal` (a write route, macOS only) and agent
//! memory / instruction files. Also [`get_in_process`]: one request answered without a socket,
//! which `uniflo mcp` uses when no daemon is running.

use crate::{ApiError, AppState, with_seq};
use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uniflo_core::memory::{self, ReadError};
use uniflo_core::resume::{self, Terminal};
use uniflo_core::util::home;
use uniflo_schema::Session;

pub(crate) async fn resume(State(s): State<AppState>, Path(key): Path<String>) -> Response {
    match s.engine.session(&key) {
        Some(sess) => with_seq(&s.engine, Json(resume::resume(&sess)).into_response()),
        None => ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response(),
    }
}

/// Runs one argv (`open …`) to completion. Replaceable with [`with_launcher`] (tests).
pub type Launcher = Arc<dyn Fn(&[String]) -> std::io::Result<()> + Send + Sync>;

/// `router` whose `open-terminal` runs `launcher` instead of the system `open`.
pub fn with_launcher(router: Router, launcher: Launcher) -> Router {
    router.layer(Extension(launcher))
}

#[derive(Deserialize)]
pub(crate) struct TerminalParams {
    terminal: Option<String>,
}

pub(crate) async fn open_terminal(
    State(s): State<AppState>,
    Path(key): Path<String>,
    Query(p): Query<TerminalParams>,
    launcher: Option<Extension<Launcher>>,
) -> Response {
    let Some(sess) = s.engine.session(&key) else {
        return ApiError(StatusCode::NOT_FOUND, format!("unknown session {key}")).into_response();
    };
    let launch = launcher.map_or_else(system_launcher, |Extension(l)| l);
    let res = tokio::task::spawn_blocking(move || {
        let tmp = std::env::temp_dir();
        let env = Env { macos: cfg!(target_os = "macos"), installed: &installed, launch: &*launch, tmp: &tmp };
        open(&sess, p.terminal.as_deref(), &env)
    })
    .await;
    match res {
        Ok((code, body)) => (code, Json(body)).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

/// What `open-terminal` needs from the machine, injectable so tests never open a window.
struct Env<'a> {
    macos: bool,
    installed: &'a dyn Fn(Terminal) -> bool,
    launch: &'a dyn Fn(&[String]) -> std::io::Result<()>,
    /// Where the private directory holding the `.command` script is created.
    tmp: &'a std::path::Path,
}

/// Open a new terminal window that changes into the session's cwd (when it still exists) and runs
/// its resume command. Terminal and iTerm get a self-deleting `.command` script through `open -a`;
/// Ghostty gets the command as argv. No Apple events, so a launchd daemon needs no Automation
/// permission.
fn open(sess: &Session, terminal: Option<&str>, env: &Env) -> (StatusCode, Value) {
    let info = resume::resume(sess);
    if !info.supported {
        let reason = info.reason.clone().unwrap_or_default();
        return (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": reason, "resume": info }));
    }
    if !env.macos {
        return (
            StatusCode::NOT_IMPLEMENTED,
            json!({
                "error": "opening a terminal is only implemented on macOS; run `command` in a terminal yourself",
                "command": info.command,
                "command_powershell": info.command_powershell,
                "cwd": info.cwd,
            }),
        );
    }
    let Some(t) = Terminal::parse(terminal.unwrap_or("")) else {
        return (StatusCode::BAD_REQUEST, json!({ "error": "terminal must be terminal, iterm or ghostty" }));
    };
    let cwd = info.cwd.as_deref().filter(|c| std::path::Path::new(c).is_dir());
    let needs_cwd = resume::argv(&sess.harness, &sess.id).is_some_and(|(_, c)| c);
    if needs_cwd && cwd.is_none() {
        let msg = format!("the session's working directory no longer exists: {}", info.cwd.as_deref().unwrap_or(""));
        return (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": msg, "command": info.command }));
    }
    let line = resume::shell_line(&info.argv, cwd);
    let fail = |msg: String| {
        (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": msg, "reason": msg, "command": line, "cwd": cwd }))
    };
    if !(env.installed)(t) {
        let msg = format!("没有安装 {}，请复制恢复命令到终端里运行", terminal_name(t));
        return (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": msg, "reason": msg, "command": line }));
    }
    let (argv, script_dir) = match t {
        Terminal::Ghostty => (resume::ghostty_argv(cwd, &line), None),
        _ => match write_script(env.tmp, &resume::terminal_script(&line)) {
            Ok((dir, path)) => (resume::terminal_argv(t, &path.to_string_lossy()).unwrap_or_default(), Some(dir)),
            Err(e) => return fail(format!("无法写入临时启动脚本：{e}；请复制恢复命令到终端里运行")),
        },
    };
    match (env.launch)(&argv) {
        Ok(()) => (StatusCode::OK, json!({ "opened": true, "terminal": t.as_str(), "command": line, "cwd": cwd })),
        Err(e) => {
            if let Some(dir) = script_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            fail(format!("无法打开 {}：{e}；请复制恢复命令到终端里运行", terminal_name(t)))
        }
    }
}

fn terminal_name(t: Terminal) -> &'static str {
    match t {
        Terminal::Terminal => "Terminal",
        Terminal::Iterm => "iTerm2",
        Terminal::Ghostty => "Ghostty",
    }
}

/// `<fresh 0700 dir>/resume.command` (0700) holding `script`; returns the directory and the file.
/// The directory name is random, so the path cannot be guessed or pre-created by another user.
fn write_script(root: &std::path::Path, script: &str) -> std::io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    use std::io::Write;
    let dir = private_dir(root)?;
    let path = dir.join("resume.command");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o700);
    let res = opts.open(&path).and_then(|mut f| f.write_all(script.as_bytes()));
    if let Err(e) = res {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok((dir, path))
}

/// Exclusively creates a 0700 directory with an unguessable name under `root`.
fn private_dir(root: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::hash::{BuildHasher, Hasher};
    let mut last = None;
    for _ in 0..16 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        let dir = root.join(format!("uniflo-open-{:016x}", h.finish()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        match b.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("无法创建临时目录")))
}

fn system_launcher() -> Launcher {
    Arc::new(|argv: &[String]| {
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()))
        }
    })
}

/// Terminal.app always exists; others are looked up in `/Applications` and `~/Applications`.
fn installed(t: Terminal) -> bool {
    t == Terminal::Terminal
        || t.bundles().iter().any(|b| {
            std::path::Path::new("/Applications").join(b).exists() || home().join("Applications").join(b).exists()
        })
}

#[derive(Deserialize)]
pub(crate) struct MemoryParams {
    cwd: Option<String>,
    path: Option<String>,
}

pub(crate) async fn memory(Query(p): Query<MemoryParams>) -> Response {
    let home = home();
    let cwd = p.cwd.as_deref().filter(|c| !c.is_empty()).map(|c| tilde(c, &home));
    if cwd.as_ref().is_some_and(|c| !c.is_absolute()) {
        return ApiError(StatusCode::BAD_REQUEST, "cwd must be an absolute path".into()).into_response();
    }
    match tokio::task::spawn_blocking(move || memory::list(&home, cwd.as_deref())).await {
        Ok(files) => Json(files).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

pub(crate) async fn memory_file(Query(p): Query<MemoryParams>) -> Response {
    let home = home();
    let Some(path) = p.path.as_deref().filter(|c| !c.is_empty()).map(|c| tilde(c, &home)) else {
        return ApiError(StatusCode::BAD_REQUEST, "missing path".into()).into_response();
    };
    match tokio::task::spawn_blocking(move || memory::read(&home, &path)).await {
        Ok(Ok(f)) => Json(f).into_response(),
        Ok(Err(ReadError::Forbidden)) => {
            ApiError(StatusCode::FORBIDDEN, "not an agent memory or instruction file listed by /v1/memory".into())
                .into_response()
        }
        Ok(Err(ReadError::NotFound)) => ApiError(StatusCode::NOT_FOUND, "no such file".into()).into_response(),
        Ok(Err(ReadError::Io(e))) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(err) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

fn tilde(p: &str, home: &std::path::Path) -> std::path::PathBuf {
    match p.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => p.into(),
    }
}

/// Answer `GET <path_and_query>` (already percent-encoded) in-process: status and body bytes,
/// exactly what a client of the daemon would read. Streaming routes never end; do not call them.
pub async fn get_in_process(router: Router, path_and_query: &str) -> (u16, Vec<u8>) {
    let req = Request::get(path_and_query).header(header::HOST, "localhost").body(Body::empty());
    let Ok(req) = req else { return (400, br#"{"error":"bad request path"}"#.to_vec()) };
    let resp = match router.oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let status = resp.status().as_u16();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.map(|b| b.to_vec()).unwrap_or_default();
    (status, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use uniflo_schema::Status;

    fn session(harness: &str, id: &str, cwd: &str) -> Session {
        Session {
            key: format!("{harness}:{id}"),
            harness: harness.into(),
            id: id.into(),
            parent: None,
            title: None,
            cwd: Some(cwd.into()),
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

    fn run(sess: &Session, terminal: Option<&str>, macos: bool, iterm: bool) -> (StatusCode, Value, Vec<Vec<String>>) {
        let tmp = tempfile::tempdir().unwrap();
        run_in(sess, terminal, macos, iterm, tmp.path(), Ok(()))
    }

    /// Like `run` with the script root and the launch outcome chosen by the test; the generated
    /// script is left in place (nothing ever executes it) for the caller to inspect.
    fn run_in(
        sess: &Session,
        terminal: Option<&str>,
        macos: bool,
        iterm: bool,
        tmp: &std::path::Path,
        outcome: std::io::Result<()>,
    ) -> (StatusCode, Value, Vec<Vec<String>>) {
        let calls = Mutex::new(Vec::new());
        let outcome = Mutex::new(Some(outcome));
        let launch = |argv: &[String]| {
            calls.lock().unwrap().push(argv.to_vec());
            outcome.lock().unwrap().take().unwrap_or(Ok(()))
        };
        let installed = |t: Terminal| t != Terminal::Iterm || iterm;
        let (code, body) = open(sess, terminal, &Env { macos, installed: &installed, launch: &launch, tmp });
        (code, body, calls.into_inner().unwrap())
    }

    #[test]
    fn launches_open_with_a_private_script_or_ghostty_argv() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("it's $here");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_str().unwrap();
        let s = session("claude", "s1", cwd);
        let line = format!("cd {} && claude --resume s1", resume::sh_quote(cwd));
        let tmp = tempfile::tempdir().unwrap();
        for (t, want, app) in [(None, Terminal::Terminal, "Terminal"), (Some("iterm"), Terminal::Iterm, "iTerm")] {
            let (code, body, calls) = run_in(&s, t, true, true, tmp.path(), Ok(()));
            assert_eq!(code, StatusCode::OK, "{body}");
            assert_eq!(
                (body["terminal"].as_str(), body["command"].as_str()),
                (Some(want.as_str()), Some(line.as_str()))
            );
            let [argv] = &calls[..] else { panic!("{calls:?}") };
            assert_eq!(argv[..3], ["open", "-a", app]);
            assert_eq!(argv.len(), 4);
            let script = std::path::Path::new(&argv[3]);
            assert!(script.starts_with(tmp.path()) && script.extension().is_some_and(|e| e == "command"), "{argv:?}");
            assert_eq!(std::fs::read_to_string(script).unwrap(), resume::terminal_script(&line));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!((mode(script), mode(script.parent().unwrap())), (0o700, 0o700));
            }
        }
        let (code, body, calls) = run(&s, Some("ghostty"), true, true);
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(calls, vec![resume::ghostty_argv(Some(cwd), &line)]);
        let (code, body, calls) = run(&s, Some("iterm"), true, false);
        assert_eq!((code, calls.len()), (StatusCode::UNPROCESSABLE_ENTITY, 0), "iTerm missing: nothing launched");
        assert_eq!(body["command"].as_str(), Some(line.as_str()));
        assert_eq!(run(&s, Some("xterm"), true, true).0, StatusCode::BAD_REQUEST);
        // Not required by codex: a missing cwd only drops the `cd`.
        let (code, body, _) = run(&session("codex", "s1", "/no/such/dir"), None, true, true);
        assert_eq!((code, body["command"].as_str()), (StatusCode::OK, Some("codex resume s1")));
        let (code, _, calls) = run(&session("claude", "s1", "/no/such/dir"), None, true, true);
        assert_eq!((code, calls.len()), (StatusCode::UNPROCESSABLE_ENTITY, 0));
    }

    #[test]
    fn failures_carry_a_reason_and_the_command_and_leave_no_script() {
        let cwd = tempfile::tempdir().unwrap();
        let s = session("claude", "s1", cwd.path().to_str().unwrap());
        let line = format!("cd {} && claude --resume s1", resume::sh_quote(cwd.path().to_str().unwrap()));
        let tmp = tempfile::tempdir().unwrap();
        for t in [Some("terminal"), Some("ghostty")] {
            let err = std::io::Error::other("Unable to find application named 'Terminal'");
            let (code, body, calls) = run_in(&s, t, true, true, tmp.path(), Err(err));
            assert_eq!((code, calls.len()), (StatusCode::INTERNAL_SERVER_ERROR, 1));
            assert_eq!(body["command"].as_str(), Some(line.as_str()));
            let reason = body["reason"].as_str().unwrap();
            assert!(reason.starts_with("无法打开") && reason.contains("Unable to find application"), "{reason}");
            assert_eq!(body["error"], body["reason"]);
        }
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "failed launches remove their script");
        let (code, body, calls) = run_in(&s, None, true, true, &tmp.path().join("missing"), Ok(()));
        assert_eq!((code, calls.len()), (StatusCode::INTERNAL_SERVER_ERROR, 0));
        assert!(body["reason"].as_str().unwrap().contains("临时启动脚本"));
        assert_eq!(body["command"].as_str(), Some(line.as_str()));
    }

    #[test]
    fn other_platforms_answer_501_with_the_command() {
        let s = session("claude", "s1", "/w d");
        let (code, body, calls) = run(&s, None, false, true);
        assert_eq!(code, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["command"], "cd '/w d' && claude --resume s1");
        assert!(body["command_powershell"].as_str().unwrap().starts_with("Set-Location"));
        assert!(calls.is_empty());
        let (code, body, calls) = run(&session("dsh", "s1", "/w"), None, true, true);
        assert_eq!((code, calls.len()), (StatusCode::UNPROCESSABLE_ENTITY, 0));
        assert_eq!(body["resume"]["supported"], false);
    }
}
