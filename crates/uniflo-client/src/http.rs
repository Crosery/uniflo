//! Minimal blocking HTTP/1.1 client (GET only): connect, send a request line, decode a
//! `Content-Length` or chunked response. Adapted from `uniflo-cli/src/client.rs`, but returns
//! this crate's own [`Error`] instead of `anyhow::Error` (this crate depends on nothing but
//! `uniflo-schema`, `serde`, `serde_json`). Also holds the percent-encoding and query-string
//! helpers every [`crate::Client`] method needs.

use crate::error::Error;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A connected response: status line parsed, headers consumed, body unread.
pub(crate) struct Response<R> {
    pub status: u16,
    chunked: bool,
    length: Option<usize>,
    reader: BufReader<R>,
}

/// Open a TCP connection and send `GET {path} HTTP/1.1`, returning the parsed status and headers.
/// `read_timeout: None` leaves the socket blocking forever on reads, which the live stream needs
/// (an idle daemon may not send another envelope for a long time).
pub(crate) fn get(
    host: &str,
    port: u16,
    path: &str,
    token: Option<&str>,
    connect_timeout: Duration,
    read_timeout: Option<Duration>,
) -> Result<Response<TcpStream>, Error> {
    let addr = format!("{host}:{port}");
    let sock =
        addr.to_socket_addrs().map_err(Error::Connect)?.next().ok_or_else(|| {
            Error::Connect(std::io::Error::new(std::io::ErrorKind::NotFound, format!("resolve {addr}")))
        })?;
    let mut stream = TcpStream::connect_timeout(&sock, connect_timeout).map_err(Error::Connect)?;
    stream.set_nodelay(true).map_err(Error::Io)?;
    stream.set_read_timeout(read_timeout).map_err(Error::Io)?;
    stream.set_write_timeout(Some(connect_timeout)).map_err(Error::Io)?;
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Accept: */*\r\nConnection: close\r\n\r\n")
        .map_err(Error::Io)?;
    read_head(stream)
}

fn read_head<R: Read>(s: R) -> Result<Response<R>, Error> {
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(Error::Io)?;
    let status: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| {
        Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad status line: {line:?}")))
    })?;
    let (mut chunked, mut length) = (false, None);
    loop {
        line.clear();
        reader.read_line(&mut line).map_err(Error::Io)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let (k, v) = l.split_once(':').unwrap_or((l, ""));
        match k.to_ascii_lowercase().as_str() {
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "content-length" => length = v.trim().parse().ok(),
            _ => {}
        }
    }
    Ok(Response { status, chunked, length, reader })
}

impl<R: Read> Response<R> {
    /// Read the whole body (bounded by `Content-Length`, or chunked, or EOF).
    pub(crate) fn body(&mut self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        if self.chunked {
            while let Some(chunk) = self.next_chunk()? {
                out.extend_from_slice(&chunk);
            }
        } else if let Some(n) = self.length {
            out.resize(n, 0);
            self.reader.read_exact(&mut out)?;
        } else {
            self.reader.read_to_end(&mut out)?;
        }
        Ok(out)
    }

    fn next_chunk(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut size = String::new();
        if self.reader.read_line(&mut size)? == 0 {
            return Ok(None);
        }
        let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad chunk size: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        let mut buf = vec![0; n + 2]; // chunk data + trailing CRLF
        self.reader.read_exact(&mut buf)?;
        buf.truncate(n);
        Ok(Some(buf))
    }

    /// One more piece of a streaming body: the next chunk (chunked encoding) or up to 64 KiB
    /// (anything else). `Ok(None)` is EOF. Used by [`crate::stream::EnvelopeStream`] to turn a
    /// long-lived connection into newline-delimited pieces without buffering the whole body.
    pub(crate) fn next_piece(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if self.chunked {
            self.next_chunk()
        } else {
            let mut buf = vec![0u8; 64 * 1024];
            let n = self.reader.read(&mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            buf.truncate(n);
            Ok(Some(buf))
        }
    }
}

/// Percent-encode every byte that is not in the URL "unreserved" set (`A-Za-z0-9-_.~`). Used for
/// both path segments (session keys may contain `:` and `/`, e.g. `claude:abc` -> `claude%3Aabc`)
/// and query values (arbitrary text, including non-ASCII such as Chinese search terms). Each byte
/// of a multi-byte UTF-8 sequence is encoded independently, which is the correct and standard way
/// to percent-encode non-ASCII text.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Builds a `?k=v&k2=v2` query string, percent-encoding every value.
#[derive(Default)]
pub(crate) struct QueryString(String);

impl QueryString {
    pub(crate) fn new() -> Self {
        QueryString(String::new())
    }

    pub(crate) fn push(&mut self, key: &str, value: &str) {
        self.0.push(if self.0.is_empty() { '?' } else { '&' });
        self.0.push_str(key);
        self.0.push('=');
        self.0.push_str(&percent_encode(value));
    }

    pub(crate) fn push_opt(&mut self, key: &str, value: Option<&str>) {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            self.push(key, v);
        }
    }

    pub(crate) fn push_u64(&mut self, key: &str, value: Option<u64>) {
        if let Some(v) = value {
            self.push(key, &v.to_string());
        }
    }

    pub(crate) fn push_usize(&mut self, key: &str, value: Option<usize>) {
        if let Some(v) = value {
            self.push(key, &v.to_string());
        }
    }

    pub(crate) fn finish(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encodes_reserved_and_non_ascii_bytes() {
        assert_eq!(percent_encode("claude:abc"), "claude%3Aabc");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
        assert_eq!(percent_encode("abc-_.~XYZ019"), "abc-_.~XYZ019");
        assert_eq!(percent_encode("缓存击穿"), "%E7%BC%93%E5%AD%98%E5%87%BB%E7%A9%BF");
    }

    #[test]
    fn query_string_builds_and_skips_empty() {
        let mut qs = QueryString::new();
        qs.push_opt("q", Some("s:work"));
        qs.push_opt("filter", None);
        qs.push_opt("filter", Some(""));
        qs.push_usize("limit", Some(20));
        assert_eq!(qs.finish(), "?q=s%3Awork&limit=20");
    }
}
