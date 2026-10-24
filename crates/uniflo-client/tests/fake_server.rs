//! Fake HTTP server tests: a `std::net::TcpListener` on `127.0.0.1:0` serving scripted bytes, no
//! real network and no uniflo daemon (synthetic data only, per `AGENTS.md`). Exercises
//! status/header parsing, `Content-Length` and chunked bodies, non-2xx error decoding, the
//! `Authorization` header, percent-encoding of session keys and non-ASCII queries, and the
//! NDJSON stream parser across chunk boundaries with a blank keep-alive line.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use uniflo_client::{Client, Envelope, Error, StreamOptions};

/// Accept exactly one connection, hand the raw request head back over the channel, then write
/// `response` verbatim and close.
fn serve_one(response: Vec<u8>) -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let req = read_request_head(&mut stream);
        let _ = tx.send(req);
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    });
    (addr, rx)
}

fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte).unwrap_or(0) == 1 {
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    buf
}

fn client(addr: SocketAddr) -> Client {
    Client::new(&format!("http://{addr}")).unwrap().with_timeout(Duration::from_secs(2))
}

fn content_length_response(status_line: &str, body: &[u8]) -> Vec<u8> {
    let mut resp = format!("{status_line}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn push_chunk(resp: &mut Vec<u8>, data: &[u8]) {
    resp.extend(format!("{:x}\r\n", data.len()).into_bytes());
    resp.extend_from_slice(data);
    resp.extend_from_slice(b"\r\n");
}

const HEALTH_OK: &[u8] =
    br#"{"ok":true,"version":"v","schema":1,"seq":0,"sessions":0,"working":0,"uptime_ms":0,"update_available":false,"read_only":false}"#;

#[test]
fn content_length_body_decodes() {
    let body = br#"{"ok":true,"version":"0.2.0-test","schema":1,"seq":5,"sessions":1,"working":0,"uptime_ms":10,"update_available":false,"read_only":false}"#;
    let (addr, _rx) = serve_one(content_length_response("HTTP/1.1 200 OK", body));
    let health = client(addr).health().unwrap();
    assert_eq!((health.ok, health.seq, health.sessions), (true, 5, 1));
}

#[test]
fn chunked_body_decodes() {
    let body = br#"{"ok":true,"version":"v","schema":1,"seq":9,"sessions":2,"working":1,"uptime_ms":1,"update_available":false,"read_only":false}"#;
    let (a, b) = body.split_at(body.len() / 2);
    let mut resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    push_chunk(&mut resp, a);
    push_chunk(&mut resp, b);
    resp.extend_from_slice(b"0\r\n\r\n");
    let (addr, _rx) = serve_one(resp);
    let health = client(addr).health().unwrap();
    assert_eq!((health.seq, health.sessions, health.working), (9, 2, 1));
}

#[test]
fn non_2xx_surfaces_gateway_error_message() {
    let body = br#"{"error":"unknown session claude:x"}"#;
    let (addr, _rx) = serve_one(content_length_response("HTTP/1.1 404 Not Found", body));
    let err = client(addr).session("claude:x").unwrap_err();
    match err {
        Error::Http { status, message } => {
            assert_eq!(status, 404);
            assert_eq!(message, "unknown session claude:x");
        }
        other => panic!("expected Error::Http, got {other:?}"),
    }
}

#[test]
fn non_2xx_without_json_error_falls_back_to_raw_body() {
    let body = b"plain text failure";
    let (addr, _rx) = serve_one(content_length_response("HTTP/1.1 500 Internal Server Error", body));
    let err = client(addr).health().unwrap_err();
    match err {
        Error::Http { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "plain text failure");
        }
        other => panic!("expected Error::Http, got {other:?}"),
    }
}

#[test]
fn authorization_header_sent_when_token_set() {
    let (addr, rx) = serve_one(content_length_response("HTTP/1.1 200 OK", HEALTH_OK));
    client(addr).with_token("s3cr3t").health().unwrap();
    let req = String::from_utf8(rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    assert!(req.contains("Authorization: Bearer s3cr3t\r\n"), "{req}");
}

#[test]
fn no_authorization_header_when_no_token() {
    let (addr, rx) = serve_one(content_length_response("HTTP/1.1 200 OK", HEALTH_OK));
    client(addr).health().unwrap();
    let req = String::from_utf8(rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    assert!(!req.to_ascii_lowercase().contains("authorization:"), "{req}");
}

#[test]
fn session_key_is_percent_encoded_in_path() {
    let body = br#"{"key":"claude:a/b","harness":"claude","id":"a/b","source":"/s","updated_at":1,"status":"idle","status_since":0}"#;
    let (addr, rx) = serve_one(content_length_response("HTTP/1.1 200 OK", body));
    let session = client(addr).session("claude:a/b").unwrap();
    assert_eq!(session.key, "claude:a/b");
    let req = String::from_utf8(rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    let line = req.lines().next().unwrap();
    assert_eq!(line, "GET /v1/sessions/claude%3Aa%2Fb HTTP/1.1");
}

#[test]
fn chinese_query_is_percent_encoded() {
    let (addr, rx) = serve_one(content_length_response("HTTP/1.1 200 OK", b"[]"));
    client(addr).sessions("缓存击穿", 5).unwrap();
    let req = String::from_utf8(rx.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    let line = req.lines().next().unwrap();
    assert_eq!(line, "GET /v1/sessions?q=%E7%BC%93%E5%AD%98%E5%87%BB%E7%A9%BF&limit=5 HTTP/1.1");
}

#[test]
fn ndjson_stream_handles_split_envelopes_and_keep_alive_blank_lines() {
    let hello = r#"{"type":"hello","seq":1,"version":1,"server":"test"}"#;
    let removed = r#"{"type":"removed","seq":2,"key":"claude:x"}"#;
    let (h1, h2) = hello.split_at(hello.len() / 2);
    let mut resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    // `hello` arrives split across two chunks, neither of which contains the terminating `\n`.
    push_chunk(&mut resp, h1.as_bytes());
    push_chunk(&mut resp, h2.as_bytes());
    // this chunk closes `hello`'s line, then is itself a blank keep-alive line.
    push_chunk(&mut resp, b"\n\n");
    // `removed` has no trailing `\n` at all: EOF must still flush it as a complete line.
    push_chunk(&mut resp, removed.as_bytes());
    resp.extend_from_slice(b"0\r\n\r\n");
    let (addr, _rx) = serve_one(resp);
    let envs: Vec<Envelope> =
        client(addr).stream(&StreamOptions::default()).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(envs.len(), 2, "{envs:?}");
    match &envs[0] {
        Envelope::Hello { seq, .. } => assert_eq!(*seq, 1),
        other => panic!("{other:?}"),
    }
    match &envs[1] {
        Envelope::Removed { seq, key } => {
            assert_eq!(*seq, 2);
            assert_eq!(key, "claude:x");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn stream_non_2xx_surfaces_error_without_hanging() {
    let body = br#"{"error":"token required"}"#;
    let (addr, _rx) = serve_one(content_length_response("HTTP/1.1 403 Forbidden", body));
    let err = client(addr).stream(&StreamOptions::default()).unwrap_err();
    match err {
        Error::Http { status, message } => {
            assert_eq!(status, 403);
            assert_eq!(message, "token required");
        }
        other => panic!("{other:?}"),
    }
}
