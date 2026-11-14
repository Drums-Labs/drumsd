//! Running host programs with a deadline, output captured.

use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Out {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub seconds: f64,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).replace("\r\n", "\n")
    }
}

/// Run `prog args` with an optional stdin, killed after `timeout`. A timeout is code -2, a spawn failure -1.
pub async fn run(prog: &str, args: &[&str], stdin: Option<&[u8]>, timeout: Duration) -> Out {
    let t0 = Instant::now();
    let mut c = Command::new(prog);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => return Out { code: -1, stdout: vec![], stderr: format!("spawn {prog}: {e}"), seconds: 0.0 },
    };
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(data).await;
        drop(si);
    }
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => Out {
            code: o.status.code().unwrap_or(-3),
            stdout: o.stdout,
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            seconds: t0.elapsed().as_secs_f64(),
        },
        Ok(Err(e)) => Out { code: -1, stdout: vec![], stderr: e.to_string(), seconds: t0.elapsed().as_secs_f64() },
        Err(_) => {
            Out { code: -2, stdout: vec![], stderr: format!("{prog} timed out after {:.0} s", timeout.as_secs_f64()), seconds: t0.elapsed().as_secs_f64() }
        }
    }
}

/// Kill a whole process group (desktop processes are started in their own group: an emulator's QEMU child, a VM).
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        let _ = std::process::Command::new("kill").args(["-9", "--", &format!("-{pgid}")]).status();
    }
}

/// Record a desktop's process group so a restarted daemon can kill what an earlier one left running.
pub fn write_pgid(dir: &std::path::Path, pgid: Option<u32>) {
    if let Some(p) = pgid {
        let _ = std::fs::write(dir.join("pgid"), p.to_string());
    }
}

/// Kill the process groups recorded under `root/drums-os-*/pgid` (daemon start: no desktop is adopted).
pub fn kill_recorded_groups(root: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("drums-os-") {
                continue;
            }
            if let Some(p) = std::fs::read_to_string(e.path().join("pgid")).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                kill_group(p);
                n += 1;
            }
        }
    }
    n
}

pub fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.001))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn runs_and_times_out() {
        let o = run("sh", &["-c", "echo hi; exit 3"], None, secs(5.0)).await;
        assert_eq!(o.code, 3);
        assert_eq!(o.text(), "hi\n");
        let o = run("cat", &[], Some(b"abc"), secs(5.0)).await;
        assert_eq!(o.text(), "abc");
        let o = run("sleep", &["5"], None, secs(0.2)).await;
        assert_eq!(o.code, -2);
        let o = run("/nonexistent/x", &[], None, secs(1.0)).await;
        assert_eq!(o.code, -1);
    }
}
