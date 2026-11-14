//! adb client for one Android device (a redroid container over TCP, or an emulator console port).

use crate::exec::{run, secs, Out};

#[derive(Clone, Debug)]
pub struct Adb {
    pub bin: String,
    pub serial: String,
    /// TCP devices (redroid) must be `adb connect`ed; emulators register themselves.
    pub tcp: bool,
    /// Run shell commands as root through `su 0` (userdebug images; AndroidWorld runs `adb root` for the same reason:
    /// checkers read app databases).
    pub su: bool,
}

impl Adb {
    pub async fn raw(&self, args: &[&str], timeout_s: f64) -> Out {
        let mut a = vec!["-s", self.serial.as_str()];
        a.extend_from_slice(args);
        run(&self.bin, &a, None, secs(timeout_s)).await
    }
    pub async fn connect(&self) -> bool {
        if !self.tcp {
            return true;
        }
        let o = run(&self.bin, &["connect", &self.serial], None, secs(5.0)).await;
        o.ok() && o.text().contains("connected")
    }
    pub async fn disconnect(&self) {
        if self.tcp {
            let _ = run(&self.bin, &["disconnect", &self.serial], None, secs(5.0)).await;
        }
    }
    /// `adb shell CMD` (one string, run by the device's sh). Exit status is the command's (adb v2 protocol).
    async fn shell_once(&self, cmd: &str, timeout_s: f64) -> Out {
        if self.su {
            let c = format!("su 0 sh -c {}", sh_quote(cmd));
            return self.raw(&["shell", &c], timeout_s).await;
        }
        self.raw(&["shell", cmd], timeout_s).await
    }

    /// A command the device never ran: adb could not reach it. Safe to retry for any command.
    pub fn not_run(o: &Out) -> bool {
        let e = o.stderr.to_ascii_lowercase();
        o.code != 0 && ["device offline", "not found", "no devices", "connection refused", "failed to connect", "unauthorized"].iter().any(|p| e.contains(p))
    }
    /// The transport broke while the command may have run (closed / protocol fault). Retry only idempotent commands.
    pub fn broken(o: &Out) -> bool {
        let e = o.stderr.to_ascii_lowercase();
        Self::not_run(o) || (o.code != 0 && ["closed", "protocol fault", "connection reset", "broken pipe"].iter().any(|p| e.contains(p)))
    }

    /// Drop the host-side TCP transport and open a new one; true when a round trip works again.
    pub async fn reconnect(&self, timeout_s: f64) -> bool {
        if !self.tcp {
            return false;
        }
        self.disconnect().await;
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_secs_f64() < timeout_s {
            self.connect().await;
            if self.shell_once("true", 5.0).await.ok() {
                return true;
            }
            tokio::time::sleep(secs(0.1)).await;
        }
        false
    }

    /// `adb shell CMD` (one string, run by the device's sh; exit status is the command's). If adb could not reach
    /// the device (the command did not run), reconnect once and retry: emulators restored from a snapshot drop their
    /// TCP transport now and then (T5: 1.5-2% of clone-mode episodes before this retry).
    pub async fn shell(&self, cmd: &str, timeout_s: f64) -> Out {
        let o = self.shell_once(cmd, timeout_s).await;
        if Self::not_run(&o) && self.reconnect(10.0).await {
            return self.shell_once(cmd, timeout_s).await;
        }
        o
    }
    /// As `shell`, but also retries when the transport broke mid-command: only for idempotent commands (probes,
    /// listings, checkers, task setup), never for actions.
    pub async fn shell_idempotent(&self, cmd: &str, timeout_s: f64) -> Out {
        let o = self.shell_once(cmd, timeout_s).await;
        if Self::broken(&o) && self.reconnect(10.0).await {
            return self.shell_once(cmd, timeout_s).await;
        }
        o
    }

    /// n consecutive successful round trips, gap_s apart (a transport that just came back can drop again).
    pub async fn steady(&self, n: u32, gap_s: f64, timeout_s: f64) -> bool {
        let t0 = std::time::Instant::now();
        let mut ok = 0;
        while t0.elapsed().as_secs_f64() < timeout_s {
            if self.shell_once("true", 5.0).await.ok() {
                ok += 1;
                if ok >= n {
                    return true;
                }
            } else {
                ok = 0;
                self.disconnect().await;
                self.connect().await;
            }
            tokio::time::sleep(secs(gap_s)).await;
        }
        false
    }
    pub async fn screencap(&self) -> Result<Vec<u8>, String> {
        let o = self.raw(&["exec-out", "screencap", "-p"], 20.0).await;
        if o.ok() && o.stdout.starts_with(b"\x89PNG") {
            Ok(o.stdout)
        } else {
            Err(format!("screencap rc {}: {}", o.code, o.stderr.trim()))
        }
    }
    pub async fn booted(&self) -> bool {
        let o = self.shell("getprop sys.boot_completed", 5.0).await;
        o.ok() && o.text().trim() == "1"
    }
    /// Wait until the device reports boot completed (reconnecting TCP devices), polling every `poll_s`.
    pub async fn wait_boot(&self, timeout_s: f64, poll_s: f64) -> Result<f64, String> {
        let t0 = std::time::Instant::now();
        loop {
            if self.tcp {
                self.connect().await;
            }
            if self.booted().await {
                return Ok(t0.elapsed().as_secs_f64());
            }
            if t0.elapsed().as_secs_f64() > timeout_s {
                return Err(format!("{} not booted after {timeout_s:.0} s", self.serial));
            }
            tokio::time::sleep(secs(poll_s)).await;
        }
    }
    /// Emulator console command through adb (`adb emu ...`).
    pub async fn emu(&self, args: &[&str], timeout_s: f64) -> Out {
        let mut a = vec!["emu"];
        a.extend_from_slice(args);
        self.raw(&a, timeout_s).await
    }
}

/// Quote one argument for the device's sh.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `input text` needs spaces as %s and shell metacharacters escaped.
pub fn input_text_arg(s: &str) -> String {
    let mut o = String::new();
    for ch in s.chars() {
        match ch {
            ' ' => o.push_str("%s"),
            '\'' | '"' | '\\' | '(' | ')' | '&' | '<' | '>' | ';' | '|' | '*' | '~' | '`' | '$' | '#' | '?' | '[' | ']' | '{' | '}' | '!' => {
                o.push('\\');
                o.push(ch);
            }
            _ => o.push(ch),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoting() {
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(input_text_arg("hi there (x)"), "hi%sthere%s\\(x\\)");
    }
}
