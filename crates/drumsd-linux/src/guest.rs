//! Minimal blocking HTTP/1.1 client for the OSWorld guest server (`desktop_env/server/main.py` in the image), enough
//! for `/platform`, `/execute`, `/screenshot`. Every request uses `Connection: close` and reads to EOF.

use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Guest {
    pub addr: SocketAddr,
    pub timeout: Duration,
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

fn dechunk(mut b: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = vec![];
    loop {
        let i = b.windows(2).position(|w| w == b"\r\n").ok_or_else(|| io::Error::other("bad chunk"))?;
        let n = usize::from_str_radix(std::str::from_utf8(&b[..i]).map_err(io::Error::other)?.split(';').next().unwrap().trim(), 16).map_err(io::Error::other)?;
        b = &b[i + 2..];
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&b[..n.min(b.len())]);
        b = &b[(n + 2).min(b.len())..];
    }
}

pub fn parse_response(raw: &[u8]) -> io::Result<Response> {
    let he = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| io::Error::other("no HTTP header end"))?;
    let head = String::from_utf8_lossy(&raw[..he]).to_string();
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| io::Error::other(format!("bad status line {head:?}")))?;
    let mut body = raw[he + 4..].to_vec();
    let lower = head.to_ascii_lowercase();
    if lower.contains("transfer-encoding: chunked") {
        body = dechunk(&body)?;
    } else if let Some(cl) = lower.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()) {
        body.truncate(cl);
    }
    Ok(Response { status, body })
}

impl Guest {
    pub fn new(port: u16, timeout: Duration) -> Guest {
        Guest { addr: SocketAddr::from(([127, 0, 0, 1], port)), timeout }
    }

    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> io::Result<Response> {
        let mut s = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        s.set_nodelay(true)?;
        let payload = body.map(|b| serde_json::to_vec(b).unwrap()).unwrap_or_default();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAccept: */*\r\n");
        if body.is_some() {
            req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", payload.len()));
        }
        req.push_str("\r\n");
        let mut buf = req.into_bytes();
        buf.extend_from_slice(&payload);
        s.write_all(&buf)?;
        let mut raw = vec![];
        s.read_to_end(&mut raw)?;
        parse_response(&raw)
    }

    pub fn platform(&self) -> io::Result<String> {
        let r = self.request("GET", "/platform", None)?;
        Ok(String::from_utf8_lossy(&r.body).trim().to_string())
    }

    /// POST /execute; fails unless status == success and returncode == 0 (as iarun.guest.Guest.execute).
    pub fn execute(&self, command: Value, shell: bool) -> io::Result<Value> {
        let r = self.request("POST", "/execute", Some(&json!({"command": command, "shell": shell})))?;
        let v: Value = serde_json::from_slice(&r.body).map_err(|e| io::Error::other(format!("/execute HTTP {}: {e}", r.status)))?;
        if v["status"] != "success" {
            return Err(io::Error::other(format!("/execute failed: {}", v.to_string().chars().take(400).collect::<String>())));
        }
        Ok(v)
    }

    pub fn sh(&self, cmd: &str) -> io::Result<String> {
        let v = self.execute(json!(cmd), true)?;
        if v["returncode"] != 0 {
            return Err(io::Error::other(format!("guest command rc={} err={}", v["returncode"], v["error"])));
        }
        Ok(v["output"].as_str().unwrap_or("").to_string())
    }

    pub fn screenshot(&self) -> io::Result<Vec<u8>> {
        let r = self.request("GET", "/screenshot", None)?;
        if !r.body.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(io::Error::other(format!("/screenshot did not return a PNG (HTTP {})", r.status)));
        }
        Ok(r.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn serve_once(reply: &'static [u8]) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            // read the whole request (headers + Content-Length body) so closing never resets the connection
            let mut got = vec![];
            let mut buf = [0u8; 4096];
            loop {
                let n = c.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                if let Some(he) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..he]).to_ascii_lowercase();
                    let cl = head.lines().find_map(|l| l.strip_prefix("content-length:")).map(|v| v.trim().parse::<usize>().unwrap()).unwrap_or(0);
                    if got.len() >= he + 4 + cl {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            c.write_all(reply).unwrap();
        });
        port
    }

    #[test]
    fn content_length_and_chunked_bodies() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nLinux").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"Linux"[..]));
        let c = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nLin\r\n2\r\nux\r\n0\r\n\r\n").unwrap();
        assert_eq!(c.body, b"Linux");
    }

    #[test]
    fn platform_and_execute_against_a_fake_server() {
        let p = serve_once(b"HTTP/1.0 200 OK\r\nContent-Type: text/html\r\n\r\nLinux");
        assert_eq!(Guest::new(p, Duration::from_secs(2)).platform().unwrap(), "Linux");
        let p = serve_once(b"HTTP/1.0 200 OK\r\n\r\n{\"status\": \"success\", \"output\": \"a\\nb\\n\", \"error\": \"\", \"returncode\": 0}");
        assert_eq!(Guest::new(p, Duration::from_secs(2)).sh("ls").unwrap(), "a\nb\n");
        let p = serve_once(b"HTTP/1.0 200 OK\r\n\r\n{\"status\": \"success\", \"output\": \"\", \"error\": \"no\", \"returncode\": 2}");
        assert!(Guest::new(p, Duration::from_secs(2)).sh("false").is_err());
    }
}
