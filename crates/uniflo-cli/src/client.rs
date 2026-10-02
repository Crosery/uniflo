//! Minimal HTTP/1.1 client for the local daemon (GET only, Content-Length or chunked).

use anyhow::{Context, Result, anyhow, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Client {
    host: String,
    port: u16,
    token: Option<String>,
}

pub struct Resp<R> {
    pub status: u16,
    chunked: bool,
    length: Option<usize>,
    reader: BufReader<R>,
}

impl Client {
    pub fn new(url: &str, token: Option<String>) -> Result<Client> {
        let rest = url.strip_prefix("http://").ok_or_else(|| anyhow!("only http:// URLs are supported: {url}"))?;
        let hostport = rest.split('/').next().unwrap_or(rest);
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().context("port")?),
            None => (hostport.to_owned(), 80),
        };
        Ok(Client { host, port, token })
    }

    pub fn alive(&self) -> bool {
        self.get("/v1/health").map(|r| r.status == 200).unwrap_or(false)
    }

    pub fn get(&self, path: &str) -> Result<Resp<TcpStream>> {
        let addr = format!("{}:{}", self.host, self.port);
        let sock = std::net::ToSocketAddrs::to_socket_addrs(&addr)?.next().ok_or_else(|| anyhow!("resolve {addr}"))?;
        let mut s = TcpStream::connect_timeout(&sock, Duration::from_millis(300))?;
        s.set_nodelay(true)?;
        let auth = self.token.as_ref().map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: {}:{}\r\n{auth}Accept: */*\r\nConnection: close\r\n\r\n",
            self.host, self.port
        )?;
        let mut reader = BufReader::new(s);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status: u16 =
            line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow!("bad status line"))?;
        let (mut chunked, mut length) = (false, None);
        loop {
            line.clear();
            reader.read_line(&mut line)?;
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
        Ok(Resp { status, chunked, length, reader })
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut r = self.get(path)?;
        let body = r.body()?;
        if r.status != 200 {
            bail!("HTTP {}: {}", r.status, String::from_utf8_lossy(&body));
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

impl<R: Read> Resp<R> {
    pub fn body(&mut self) -> Result<Vec<u8>> {
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

    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let mut size = String::new();
        if self.reader.read_line(&mut size)? == 0 {
            return Ok(None);
        }
        let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or(""), 16).context("chunk size")?;
        if n == 0 {
            return Ok(None);
        }
        let mut buf = vec![0; n + 2];
        self.reader.read_exact(&mut buf)?;
        buf.truncate(n);
        Ok(Some(buf))
    }

    /// Feed complete lines of a streaming (chunked) body to `f` until EOF or `f` returns false.
    pub fn lines(&mut self, mut f: impl FnMut(&[u8]) -> bool) -> Result<()> {
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let chunk = if self.chunked {
                match self.next_chunk()? {
                    Some(c) => c,
                    None => break,
                }
            } else {
                let mut buf = vec![0; 64 * 1024];
                let n = self.reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                buf.truncate(n);
                buf
            };
            pending.extend_from_slice(&chunk);
            while let Some(i) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=i).collect();
                if !f(&line[..line.len() - 1]) {
                    return Ok(());
                }
            }
        }
        if !pending.is_empty() {
            f(&pending);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(raw: &'static str, chunked: bool) -> Resp<&'static [u8]> {
        Resp { status: 200, chunked, length: None, reader: BufReader::new(raw.as_bytes()) }
    }

    #[test]
    fn chunked_bodies_and_lines() {
        let mut r = resp("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n", true);
        assert_eq!(r.body().unwrap(), b"hello world");
        let mut r = resp("3\r\na\nb\r\n3\r\nc\nd\r\n0\r\n\r\n", true);
        let mut got = Vec::new();
        r.lines(|l| {
            got.push(String::from_utf8(l.to_vec()).unwrap());
            true
        })
        .unwrap();
        assert_eq!(got, vec!["a", "bc", "d"]);
    }

    #[test]
    fn url_parsing() {
        let c = Client::new("http://127.0.0.1:7311", None).unwrap();
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", 7311));
        assert!(Client::new("https://x", None).is_err());
    }
}
