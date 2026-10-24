//! [`EnvelopeStream`]: turns the long-lived `GET /v1/stream.ndjson` connection into
//! `Iterator<Item = Result<Envelope, Error>>`.

use crate::error::Error;
use crate::http::Response;
use std::net::TcpStream;
use uniflo_schema::Envelope;

/// Live [`Envelope`]s from `GET /v1/stream.ndjson`, returned by [`crate::Client::stream`].
///
/// Iterates forever (as long as the daemon keeps the connection open): `next()` blocks until
/// another envelope arrives. Drop the iterator to close the connection. The underlying socket has
/// no read timeout, since an idle daemon can go a long time between events.
pub struct EnvelopeStream {
    resp: Response<TcpStream>,
    /// Bytes read from the socket that have not formed a complete line yet.
    pending: Vec<u8>,
    eof: bool,
}

impl EnvelopeStream {
    pub(crate) fn new(resp: Response<TcpStream>) -> Self {
        EnvelopeStream { resp, pending: Vec::new(), eof: false }
    }
}

impl std::fmt::Debug for EnvelopeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvelopeStream").field("pending_bytes", &self.pending.len()).field("eof", &self.eof).finish()
    }
}

/// Strip one trailing `\n` and an optional preceding `\r`.
fn strip_eol(mut line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\n') {
        line = &line[..line.len() - 1];
    }
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    line
}

impl Iterator for EnvelopeStream {
    type Item = Result<Envelope, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=pos).collect();
                let line = strip_eol(&line);
                // The gateway's NDJSON transport has no blank keep-alive lines today, but
                // tolerate them (and any other whitespace-only line) rather than fail a decode.
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                return Some(
                    serde_json::from_slice(line)
                        .map_err(|e| Error::Decode { path: "/v1/stream.ndjson".to_owned(), source: e }),
                );
            }
            if self.eof {
                if self.pending.iter().all(u8::is_ascii_whitespace) {
                    return None;
                }
                let rest = std::mem::take(&mut self.pending);
                return Some(
                    serde_json::from_slice(&rest)
                        .map_err(|e| Error::Decode { path: "/v1/stream.ndjson".to_owned(), source: e }),
                );
            }
            match self.resp.next_piece() {
                Ok(Some(bytes)) => self.pending.extend_from_slice(&bytes),
                Ok(None) => self.eof = true,
                Err(e) => return Some(Err(Error::Io(e))),
            }
        }
    }
}
