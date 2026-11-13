//! Tiny async HTTP/1.1 client for in-guest servers on 127.0.0.1 (Connection: close, read to EOF).

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
}

fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = vec![];
    loop {
        let i = b.windows(2).position(|w| w == b"\r\n").ok_or("bad chunk")?;
        let n =
            usize::from_str_radix(std::str::from_utf8(&b[..i]).map_err(|e| e.to_string())?.split(';').next().unwrap().trim(), 16).map_err(|e| e.to_string())?;
        b = &b[i + 2..];
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&b[..n.min(b.len())]);
        b = &b[(n + 2).min(b.len())..];
    }
}

pub fn parse(raw: &[u8]) -> Result<Resp, String> {
    let he = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("no HTTP header end")?;
    let head = String::from_utf8_lossy(&raw[..he]).to_string();
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or(format!("bad status line {head:?}"))?;
    let mut body = raw[he + 4..].to_vec();
    let lower = head.to_ascii_lowercase();
    if lower.contains("transfer-encoding: chunked") {
        body = dechunk(&body)?;
    } else if let Some(cl) = lower.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()) {
        body.truncate(cl);
    }
    Ok(Resp { status, body })
}

pub async fn request(port: u16, method: &str, path: &str, body: Option<&Value>, timeout_s: f64) -> Result<Resp, String> {
    let fut = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| format!("connect :{port}: {e}"))?;
        let b = body.map(|v| serde_json::to_vec(v).unwrap()).unwrap_or_default();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n", b.len()).into_bytes();
        if body.is_some() {
            req.extend_from_slice(b"Content-Type: application/json\r\n");
        }
        req.extend_from_slice(b"\r\n");
        req.extend_from_slice(&b);
        s.write_all(&req).await.map_err(|e| format!("write: {e}"))?;
        let mut raw = vec![];
        s.read_to_end(&mut raw).await.map_err(|e| format!("read: {e}"))?;
        parse(&raw)
    };
    match tokio::time::timeout(std::time::Duration::from_secs_f64(timeout_s), fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("guest did not answer {method} {path} within {timeout_s:.1} s")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses() {
        let r = super::parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhixx").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hi"[..]));
        let r = super::parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body, b"hi");
    }
}
