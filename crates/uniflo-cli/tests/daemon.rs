//! `uniflo daemon --restart` run by a caller that captures its output (a script, CI, a harness
//! hook): the command returns and the caller's pipes close while the background daemon keeps
//! serving. On Windows the daemon used to inherit those pipes, so the caller waited on it forever.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_uniflo");

fn uniflo(dir: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("UNIFLO_HOME", dir.join("home"))
        .env("UNIFLO_CACHE_DIR", dir.join("cache"))
        .env("UNIFLO_DATA_DIR", dir.join("data"))
        .env("UNIFLO_CONFIG_DIR", dir.join("config"))
        .stdin(Stdio::null());
    c
}

fn read_to_end_later(mut r: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx
}

#[test]
fn restart_returns_and_releases_the_callers_pipes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = uniflo(dir.path())
        .args([
            "daemon",
            "--restart",
            "--bind",
            &bind,
            "--no-cache",
            "--no-update-check",
            "--no-price-sync",
            "--no-fts",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = read_to_end_later(child.stdout.take().unwrap());
    let err = read_to_end_later(child.stderr.take().unwrap());
    let status = child.wait().unwrap();
    let out = out.recv_timeout(Duration::from_secs(10));
    let err = err.recv_timeout(Duration::from_secs(10));
    // stop the background daemon before asserting, so a failure leaves nothing running
    let stop = uniflo(dir.path()).args(["daemon", "--stop"]).output().unwrap();

    assert!(status.success(), "{status}");
    assert!(out.is_ok(), "stdout stayed open after `daemon --restart` exited");
    let err = err.expect("stderr stayed open after `daemon --restart` exited");
    assert!(err.contains("daemon started in background"), "{err}");
    assert!(stop.status.success(), "{}", String::from_utf8_lossy(&stop.stderr));
}
