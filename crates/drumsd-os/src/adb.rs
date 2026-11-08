//! adb client for one Android device (a redroid container over TCP, or an emulator console port).

use crate::exec::{run, secs, Out};

#[derive(Clone, Debug)]
pub struct Adb {
    pub bin: String,
    pub serial: String,
    /// TCP devices (redroid) must be `adb connect`ed; emulators register themselves.
    pub tcp: bool,
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
    pub async fn shell(&self, cmd: &str, timeout_s: f64) -> Out {
        self.raw(&["shell", cmd], timeout_s).await
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
