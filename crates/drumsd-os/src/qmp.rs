//! Minimal QMP client (QEMU Machine Protocol over a unix socket): one command at a time, events skipped.

use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct Qmp {
    rd: BufReader<tokio::net::unix::OwnedReadHalf>,
    wr: tokio::net::unix::OwnedWriteHalf,
}

impl Qmp {
    pub async fn connect(sock: &Path, timeout_s: f64) -> Result<Qmp, String> {
        let t0 = std::time::Instant::now();
        let s = loop {
            match UnixStream::connect(sock).await {
                Ok(s) => break s,
                Err(e) if t0.elapsed().as_secs_f64() > timeout_s => {
                    return Err(format!("qmp {}: {e}", sock.display()))
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        };
        let (r, w) = s.into_split();
        let mut q = Qmp {
            rd: BufReader::new(r),
            wr: w,
        };
        let greet = q.line().await?;
        if greet.get("QMP").is_none() {
            return Err(format!("qmp: unexpected greeting {greet}"));
        }
        q.cmd("qmp_capabilities", json!({})).await?;
        Ok(q)
    }
    async fn line(&mut self) -> Result<Value, String> {
        let mut s = String::new();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            self.rd.read_line(&mut s),
        )
        .await
        .map_err(|_| "qmp: no reply in 600 s".to_string())?
        .map_err(|e| format!("qmp read: {e}"))?;
        if n == 0 {
            return Err("qmp: socket closed".into());
        }
        serde_json::from_str(&s).map_err(|e| format!("qmp: bad json {e}: {s}"))
    }
    /// Execute a command; returns its `return` value, or the QMP error as Err.
    pub async fn cmd(&mut self, execute: &str, arguments: Value) -> Result<Value, String> {
        let mut req = json!({"execute": execute});
        if arguments.as_object().is_some_and(|o| !o.is_empty()) {
            req["arguments"] = arguments;
        }
        let mut b = serde_json::to_vec(&req).unwrap();
        b.push(b'\n');
        self.wr
            .write_all(&b)
            .await
            .map_err(|e| format!("qmp write: {e}"))?;
        loop {
            let v = self.line().await?;
            if v.get("event").is_some() {
                continue;
            }
            if let Some(r) = v.get("return") {
                return Ok(r.clone());
            }
            if let Some(e) = v.get("error") {
                return Err(format!(
                    "qmp {execute}: {}",
                    e["desc"].as_str().unwrap_or(&e.to_string())
                ));
            }
        }
    }
    pub async fn status(&mut self) -> Result<String, String> {
        Ok(self.cmd("query-status", json!({})).await?["status"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }
    /// Wait for an incoming migration to finish (status leaves "inmigrate").
    pub async fn wait_not_inmigrate(&mut self, timeout_s: f64) -> Result<String, String> {
        let t0 = std::time::Instant::now();
        loop {
            let s = self.status().await?;
            if s != "inmigrate" {
                return Ok(s);
            }
            if t0.elapsed().as_secs_f64() > timeout_s {
                return Err(format!(
                    "incoming migration not finished after {timeout_s:.0} s"
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    /// Wait for an outgoing migration to complete; returns the final query-migrate.
    pub async fn wait_migrated(&mut self, timeout_s: f64) -> Result<Value, String> {
        let t0 = std::time::Instant::now();
        loop {
            let m = self.cmd("query-migrate", json!({})).await?;
            match m["status"].as_str().unwrap_or("") {
                "completed" => return Ok(m),
                "failed" | "cancelled" => {
                    return Err(format!("migration {}: {}", m["status"], m["error-desc"]))
                }
                _ => {}
            }
            if t0.elapsed().as_secs_f64() > timeout_s {
                return Err(format!("migration not complete after {timeout_s:.0} s"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
