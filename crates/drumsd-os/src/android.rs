//! Android desktops: redroid containers (Android in a container on the host kernel, binder from binder_linux) or
//! Android emulators on KVM (copy-on-write over one AVD with `-read-only`, reset = load the clean snapshot).
//!
//! redroid: each desktop is a privileged container whose /data is an overlay (lower = the golden /data made by
//! osvm/hosts/android-golden.sh, upper = the desktop's own). Reset = remove the container, drop the upper layer,
//! start a fresh container on a fresh upper (Android boots from scratch; no memory snapshot).
//! emulator: each desktop is `emulator @AVD -read-only -snapshot clean` (writes go to a temporary overlay).
//! Reset = `adb emu avd snapshot load clean` (memory and disk back to the snapshot); a failed load falls back to a
//! cold restart of the emulator process.

use crate::adb::{input_text_arg, sh_quote, Adb};
use crate::exec::{run, secs, Out};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidMode {
    Redroid,
    Emulator,
}

fn d_adb() -> String {
    "adb".into()
}
fn d_docker() -> String {
    "docker".into()
}
fn d_boot() -> f64 {
    180.0
}
fn d_true() -> bool {
    true
}
fn d_snap() -> String {
    "clean".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AndroidCfg {
    pub mode: AndroidMode,
    #[serde(default = "d_adb")]
    pub adb: String,
    #[serde(default = "d_docker")]
    pub docker: String,
    /// redroid: image reference (pin a digest).
    #[serde(default)]
    pub image: String,
    /// redroid: the golden /data tree (read-only lower layer).
    #[serde(default)]
    pub golden_data: PathBuf,
    /// redroid: boot args (androidboot.* properties).
    #[serde(default)]
    pub boot_args: Vec<String>,
    /// redroid: CPU and memory limits per container (0 = none).
    #[serde(default)]
    pub cpus: f64,
    #[serde(default)]
    pub memory_mib: u64,
    /// emulator: the emulator binary and AVD name; ANDROID_SDK_ROOT / ANDROID_AVD_HOME for it.
    #[serde(default)]
    pub emulator: String,
    #[serde(default)]
    pub avd: String,
    #[serde(default)]
    pub sdk_root: String,
    #[serde(default)]
    pub avd_home: String,
    #[serde(default = "d_snap")]
    pub snapshot: String,
    /// emulator clone mode: each desktop gets its own reflink copy of `template_avd` (an .avd directory, a copy of
    /// <avd_home>/<avd>.avd on the reflink filesystem) bind-mounted over <avd_home>/<avd>.avd in a private mount
    /// namespace, under
    /// `clone_root` (a reflink-capable filesystem, e.g. XFS), so it runs without -read-only and reset is an in-place
    /// `snapshot load` (memory + disk). Unset: all desktops share the AVD with -read-only (reset = process restart).
    #[serde(default)]
    pub clone_root: Option<PathBuf>,
    #[serde(default)]
    pub template_avd: Option<PathBuf>,
    /// emulator: extra arguments (e.g. ["-memory","2048","-cores","2","-gpu","swiftshader_indirect"]).
    #[serde(default)]
    pub emulator_args: Vec<String>,
    #[serde(default = "d_boot")]
    pub boot_timeout_s: f64,
    /// Host-global sysctls (paths under /proc/sys) a privileged redroid container must not change, with their values.
    /// Android's boot raises vm/watermark_scale_factor by one on every boot; it is not namespaced, so each redroid
    /// reset raised the HOST's free-memory watermarks (T5: 10 -> 60 in 200 boots; MemAvailable lost hundreds of GiB
    /// over a ladder). drumsd restores these after every boot and before every reset, and reports each restore.
    #[serde(default)]
    pub pin_host_sysctl: std::collections::BTreeMap<String, String>,
    /// Run desktop shell commands as root via `su 0` (default true).
    #[serde(default = "d_true")]
    pub su: bool,
    /// Where per-desktop state lives (redroid overlays, emulator logs).
    pub work_dir: PathBuf,
}

pub struct AndroidDesk {
    pub cfg: AndroidCfg,
    pub name: String,
    /// redroid: host port forwarded to the container's adbd. emulator: console port (adb port = +1).
    pub port: u16,
    pub adb: Adb,
    child: Option<tokio::process::Child>,
}

impl AndroidDesk {
    pub fn new(cfg: AndroidCfg, name: String, port: u16) -> AndroidDesk {
        // emulators are addressed over TCP (adb port = console + 1): adb's automatic emulator discovery stops at 16
        let (serial, tcp) = match cfg.mode {
            AndroidMode::Redroid => (format!("127.0.0.1:{port}"), true),
            AndroidMode::Emulator => (format!("127.0.0.1:{}", port + 1), true),
        };
        let adb = Adb {
            bin: cfg.adb.clone(),
            serial,
            tcp,
            su: cfg.su,
        };
        AndroidDesk {
            cfg,
            name,
            port,
            adb,
            child: None,
        }
    }
    fn dir(&self) -> PathBuf {
        self.cfg.work_dir.join(&self.name)
    }

    /// Start from the clean image. Returns timings (seconds) by phase.
    pub async fn start(&mut self) -> Result<Value, String> {
        match self.cfg.mode {
            AndroidMode::Redroid => self.redroid_up().await,
            AndroidMode::Emulator => self.emulator_up().await,
        }
    }

    async fn redroid_mount(&self) -> Result<(), String> {
        let d = self.dir();
        let (upper, work, merged) = (d.join("upper"), d.join("work"), d.join("data"));
        // never reuse an upper layer (a desktop of the same name from an earlier daemon may have left one)
        if upper.exists() || work.exists() {
            self.redroid_unmount().await?;
        }
        for p in [&upper, &work, &merged] {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            self.cfg.golden_data.display(),
            upper.display(),
            work.display()
        );
        let o = run(
            "mount",
            &[
                "-t",
                "overlay",
                "overlay",
                "-o",
                &opts,
                &merged.to_string_lossy(),
            ],
            None,
            secs(30.0),
        )
        .await;
        if !o.ok() {
            return Err(format!("overlay mount: {}", o.stderr.trim()));
        }
        Ok(())
    }
    async fn redroid_unmount(&self) -> Result<(), String> {
        let d = self.dir();
        let merged = d.join("data");
        let _ = run(
            "umount",
            &["-l", &merged.to_string_lossy()],
            None,
            secs(30.0),
        )
        .await;
        // drop the upper layer: rename now (O(1)), delete in the background
        let trash = d.join(format!(
            "trash-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&trash).map_err(|e| e.to_string())?;
        for n in ["upper", "work"] {
            let _ = std::fs::rename(d.join(n), trash.join(n));
        }
        tokio::spawn(async move {
            let _ = run("rm", &["-rf", &trash.to_string_lossy()], None, secs(600.0)).await;
        });
        Ok(())
    }
    async fn redroid_up(&mut self) -> Result<Value, String> {
        let t0 = std::time::Instant::now();
        self.redroid_mount().await?;
        let t_mount = t0.elapsed().as_secs_f64();
        let vol = format!("{}:/data", self.dir().join("data").display());
        let pubp = format!("127.0.0.1:{}:5555", self.port);
        let mut a: Vec<String> = vec![
            "run",
            "-d",
            "--privileged",
            "--name",
            &self.name,
            "-v",
            &vol,
            "-p",
            &pubp,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        if self.cfg.cpus > 0.0 {
            a.push(format!("--cpus={}", self.cfg.cpus));
        }
        if self.cfg.memory_mib > 0 {
            a.push(format!("--memory={}m", self.cfg.memory_mib));
        }
        a.push(self.cfg.image.clone());
        a.extend(self.cfg.boot_args.iter().cloned());
        let ar: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        let o = run(&self.cfg.docker, &ar, None, secs(60.0)).await;
        if !o.ok() {
            return Err(format!("docker run: {}", o.stderr.trim()));
        }
        let t_run = t0.elapsed().as_secs_f64();
        self.adb.wait_boot(self.cfg.boot_timeout_s, 0.1).await?;
        let t_boot = t0.elapsed().as_secs_f64();
        let restored = self.restore_host_sysctl();
        Ok(
            json!({"mount_s": t_mount, "container_s": t_run - t_mount, "boot_s": t_boot - t_run, "total_s": t_boot, "host_sysctl_restored": restored}),
        )
    }

    /// Put pinned host sysctls back; returns what was changed ("path: found -> pinned").
    pub fn restore_host_sysctl(&self) -> Vec<String> {
        let mut out = vec![];
        for (k, want) in &self.cfg.pin_host_sysctl {
            let p = std::path::Path::new("/proc/sys").join(k);
            let cur = std::fs::read_to_string(&p).unwrap_or_default().trim().to_string();
            if cur != want.trim() {
                let ok = std::fs::write(&p, want.trim()).is_ok();
                out.push(format!("{k}: {cur} -> {}{}", want.trim(), if ok { "" } else { " (write failed)" }));
            }
        }
        out
    }
    async fn redroid_down(&mut self) -> Result<(), String> {
        let _ = self.restore_host_sysctl();
        let o = run(
            &self.cfg.docker,
            &["rm", "-f", &self.name],
            None,
            secs(60.0),
        )
        .await;
        self.adb.disconnect().await;
        self.redroid_unmount().await?;
        if !o.ok() && !o.stderr.contains("No such container") {
            return Err(format!("docker rm: {}", o.stderr.trim()));
        }
        Ok(())
    }

    /// Clone mode: make this desktop's AVD (reflink copy + ini). Returns seconds.
    async fn clone_avd(&self) -> Result<f64, String> {
        let (Some(root), Some(tpl)) = (&self.cfg.clone_root, &self.cfg.template_avd) else {
            return Ok(0.0);
        };
        let t0 = std::time::Instant::now();
        let dst = root.join(format!("{}.avd", self.name));
        let _ = run("rm", &["-rf", &dst.to_string_lossy()], None, secs(120.0)).await;
        let o = run(
            "cp",
            &[
                "-a",
                "--reflink=always",
                &tpl.to_string_lossy(),
                &dst.to_string_lossy(),
            ],
            None,
            secs(300.0),
        )
        .await;
        if !o.ok() {
            return Err(format!("reflink clone: {}", o.stderr.trim()));
        }
        let _ = run(
            "sh",
            &["-c", &format!("rm -f {}/*.lock", dst.display())],
            None,
            secs(10.0),
        )
        .await;
        Ok(t0.elapsed().as_secs_f64())
    }
    fn cloned(&self) -> bool {
        self.cfg.clone_root.is_some() && self.cfg.template_avd.is_some()
    }

    async fn emulator_up(&mut self) -> Result<Value, String> {
        let t0 = std::time::Instant::now();
        std::fs::create_dir_all(self.dir()).map_err(|e| e.to_string())?;
        let clone_s = if self.cloned()
            && !self
                .cfg
                .clone_root
                .as_ref()
                .unwrap()
                .join(format!("{}.avd", self.name))
                .exists()
        {
            self.clone_avd().await?
        } else {
            0.0
        };
        let log =
            std::fs::File::create(self.dir().join("emulator.log")).map_err(|e| e.to_string())?;
        // Clone mode: the snapshot records the AVD's name and paths, so each emulator runs in its own mount namespace
        // with its clone bind-mounted over the template's path (same name, same paths, private blocks).
        let mut c = if self.cloned() {
            let mut c = tokio::process::Command::new("unshare");
            let clone = self
                .cfg
                .clone_root
                .as_ref()
                .unwrap()
                .join(format!("{}.avd", self.name));
            c.args([
                "-m",
                "--propagation",
                "private",
                "sh",
                "-c",
                "mount --bind \"$DRUMS_CLONE\" \"$DRUMS_TPL\" && exec \"$0\" \"$@\"",
                &self.cfg.emulator,
            ])
            .env("DRUMS_CLONE", clone)
            // the path the snapshot was saved at: <avd_home>/<avd>.avd
            .env(
                "DRUMS_TPL",
                std::path::Path::new(&self.cfg.avd_home).join(format!("{}.avd", self.cfg.avd)),
            );
            c
        } else {
            tokio::process::Command::new(&self.cfg.emulator)
        };
        let ro: &[&str] = if self.cloned() { &[] } else { &["-read-only"] };
        c.arg(format!("@{}", self.cfg.avd))
            .args(ro)
            .args([
                "-no-window",
                "-no-audio",
                "-no-boot-anim",
                "-no-snapshot-save",
                "-snapshot",
                &self.cfg.snapshot,
                "-port",
                &self.port.to_string(),
            ])
            .args(&self.cfg.emulator_args)
            .env("ANDROID_SDK_ROOT", &self.cfg.sdk_root)
            .env("ANDROID_HOME", &self.cfg.sdk_root)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true);
        if !self.cfg.avd_home.is_empty() {
            c.env("ANDROID_AVD_HOME", &self.cfg.avd_home);
        }
        c.process_group(0);
        let child = c.spawn().map_err(|e| format!("emulator spawn: {e}"))?;
        crate::exec::write_pgid(&self.dir(), child.id());
        self.child = Some(child);
        self.adb.wait_boot(self.cfg.boot_timeout_s, 0.1).await?;
        Ok(
            json!({"clone_s": clone_s, "boot_s": t0.elapsed().as_secs_f64() - clone_s, "total_s": t0.elapsed().as_secs_f64()}),
        )
    }
    async fn emulator_down(&mut self) {
        let _ = self.console("kill", 10.0).await;
        self.adb.disconnect().await;
        if let Some(mut c) = self.child.take() {
            let pg = c.id();
            let t0 = std::time::Instant::now();
            while t0.elapsed().as_secs_f64() < 10.0 {
                if let Ok(Some(_)) = c.try_wait() {
                    break;
                }
                tokio::time::sleep(secs(0.1)).await;
            }
            // the emulator's own QEMU child and helpers share its process group
            if let Some(p) = pg {
                crate::exec::kill_group(p);
            }
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
    }

    /// One emulator console command (telnet protocol on the console port; the console auth token file is empty).
    pub async fn console(&self, cmd: &str, timeout_s: f64) -> Result<String, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let port = self.port;
        let fut = async move {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .map_err(|e| format!("console :{port}: {e}"))?;
            let mut buf = vec![0u8; 65536];
            let mut acc = String::new();
            // banner ends with "OK"
            while !acc.contains("\nOK") {
                let n = s.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("console closed".into());
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            s.write_all(format!("{cmd}\r\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let mut out = String::new();
            loop {
                let n = s.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                out.push_str(&String::from_utf8_lossy(&buf[..n]));
                let t = out.trim_end();
                if t.ends_with("OK") || t.lines().last().is_some_and(|l| l.starts_with("KO")) {
                    break;
                }
            }
            let _ = s.write_all(b"quit\r\n").await;
            Ok(out)
        };
        tokio::time::timeout(secs(timeout_s), fut)
            .await
            .map_err(|_| format!("console {cmd:?}: no answer in {timeout_s:.0} s"))?
    }

    /// Back to the clean image.
    pub async fn reset(&mut self) -> Result<Value, String> {
        match self.cfg.mode {
            AndroidMode::Redroid => {
                let t0 = std::time::Instant::now();
                self.redroid_down().await?;
                let down = t0.elapsed().as_secs_f64();
                let mut v = self.redroid_up().await?;
                v["down_s"] = json!(down);
                v["total_s"] = json!(t0.elapsed().as_secs_f64());
                v["kind"] = json!("container_reboot");
                Ok(v)
            }
            AndroidMode::Emulator => {
                let t0 = std::time::Instant::now();
                let why = if self.cloned() {
                    let o = self
                        .console(&format!("avd snapshot load {}", self.cfg.snapshot), 120.0)
                        .await;
                    let load_s = t0.elapsed().as_secs_f64();
                    match o {
                        Ok(text) if !text.contains("KO") => {
                            // the guest's adbd comes back with the snapshot's connection state: drop the host-side
                            // TCP transport and open a new one (an old transport stayed offline in 11 of 304 loads)
                            self.adb.disconnect().await;
                            if self.adb.wait_boot(15.0, 0.02).await.is_ok() {
                                return Ok(
                                    json!({"kind": "snapshot_load", "load_s": load_s, "total_s": t0.elapsed().as_secs_f64()}),
                                );
                            }
                            "not booted after snapshot load".to_string()
                        }
                        Ok(text) => format!("snapshot load failed: {}", text.trim()),
                        Err(e) => format!("snapshot load failed: {e}"),
                    }
                } else {
                    "read-only emulator (snapshot load is disabled by the emulator): restart from the snapshot".to_string()
                };
                self.emulator_down().await;
                if self.cloned() {
                    self.clone_avd().await?;
                }
                let mut v = self.emulator_up().await?;
                v["kind"] = json!("restart");
                v["fallback_reason"] = json!(why);
                v["total_s"] = json!(t0.elapsed().as_secs_f64());
                Ok(v)
            }
        }
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        match self.cfg.mode {
            AndroidMode::Redroid => self.redroid_down().await,
            AndroidMode::Emulator => {
                self.emulator_down().await;
                if let (true, Some(root)) = (self.cloned(), &self.cfg.clone_root) {
                    let _ = run(
                        "rm",
                        &[
                            "-rf",
                            &root.join(format!("{}.avd", self.name)).to_string_lossy(),
                        ],
                        None,
                        secs(120.0),
                    )
                    .await;
                }
                Ok(())
            }
        }
    }

    pub async fn alive(&mut self) -> bool {
        match self.cfg.mode {
            AndroidMode::Redroid => {
                let o = run(
                    &self.cfg.docker,
                    &["inspect", "-f", "{{.State.Running}}", &self.name],
                    None,
                    secs(10.0),
                )
                .await;
                o.ok() && o.text().trim() == "true"
            }
            AndroidMode::Emulator => {
                matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(None)))
            }
        }
    }

    pub async fn shell(&self, cmd: &str, timeout_s: f64) -> Out {
        self.adb.shell(cmd, timeout_s).await
    }
    pub async fn screenshot(&self) -> Result<Vec<u8>, String> {
        self.adb.screencap().await
    }

    /// One action. Accepts AndroidWorld's JSONAction shape (`action_type`) or `type`, plus `shell` for harness use.
    pub async fn act(&self, a: &Value) -> Result<Value, String> {
        let kind = a
            .get("action_type")
            .or_else(|| a.get("type"))
            .and_then(|v| v.as_str())
            .ok_or("action needs action_type (or type)")?;
        let num = |k: &str| a.get(k).and_then(|v| v.as_f64()).map(|f| f.round() as i64);
        let xy = || -> Result<(i64, i64), String> {
            Ok((num("x").ok_or("x missing")?, num("y").ok_or("y missing")?))
        };
        let cmd: String = match kind {
            "click" | "tap" => {
                let (x, y) = xy()?;
                format!("input tap {x} {y}")
            }
            "double_tap" => {
                let (x, y) = xy()?;
                format!("input tap {x} {y}; input tap {x} {y}")
            }
            "long_press" => {
                let (x, y) = xy()?;
                format!("input swipe {x} {y} {x} {y} 1000")
            }
            "swipe" => {
                let g = |k: &str| num(k).ok_or(format!("{k} missing"));
                format!(
                    "input swipe {} {} {} {} {}",
                    g("x1")?,
                    g("y1")?,
                    g("x2")?,
                    g("y2")?,
                    num("duration_ms").unwrap_or(300)
                )
            }
            "scroll" => {
                // AndroidWorld: direction is where the content moves into view; swipe the opposite way
                let (w, h) = (1080i64, 2400i64);
                let (cx, cy) = (w / 2, h / 2);
                match a
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("down")
                {
                    "down" => format!("input swipe {cx} {} {cx} {} 300", cy + h / 4, cy - h / 4),
                    "up" => format!("input swipe {cx} {} {cx} {} 300", cy - h / 4, cy + h / 4),
                    "right" => format!("input swipe {} {cy} {} {cy} 300", cx + w / 4, cx - w / 4),
                    "left" => format!("input swipe {} {cy} {} {cy} 300", cx - w / 4, cx + w / 4),
                    d => return Err(format!("scroll direction {d}?")),
                }
            }
            "input_text" | "type" => {
                let t = a
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or("text missing")?;
                let pre = match (num("x"), num("y")) {
                    (Some(x), Some(y)) => format!("input tap {x} {y}; "),
                    _ => String::new(),
                };
                format!("{pre}input text {}", sh_quote(&input_text_arg(t)))
            }
            "keyboard_enter" => "input keyevent 66".into(),
            "navigate_home" => "input keyevent 3".into(),
            "navigate_back" => "input keyevent 4".into(),
            "key" => format!(
                "input keyevent {}",
                a.get("key")
                    .and_then(|v| v
                        .as_str()
                        .map(String::from)
                        .or(v.as_i64().map(|i| i.to_string())))
                    .ok_or("key missing")?
            ),
            "open_app" => {
                let p = a
                    .get("app_name")
                    .or_else(|| a.get("package"))
                    .and_then(|v| v.as_str())
                    .ok_or("app_name missing")?;
                // the package's launcher activity, started like a tap on its icon (monkey does not launch on these images)
                format!("am start -n \"$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {} | tail -1)\"", sh_quote(p))
            }
            "wait" | "sleep" => {
                let s = a
                    .get("seconds")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0)
                    .clamp(0.0, 60.0);
                tokio::time::sleep(secs(s)).await;
                return Ok(json!({"status": "ok", "slept_s": s}));
            }
            "status" | "answer" => return Ok(json!({"status": "ok", "noop": true})),
            "shell" => a
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or("command missing")?
                .to_string(),
            k => return Err(format!("unknown android action {k}")),
        };
        let o = self.adb.shell(&cmd, 60.0).await;
        if o.code == -2 || o.code == -1 {
            return Err(format!("adb: {}", o.stderr.trim()));
        }
        Ok(
            json!({"status": if o.ok() {"ok"} else {"error"}, "returncode": o.code, "output": o.text(), "error": o.stderr, "seconds": o.seconds}),
        )
    }

    /// Default probe for the state-hash gate: user-visible state a task can change. `{EXCLUDE}` is replaced with the
    /// pool's `probe_exclude` regex (keys measured to change between two clean boots).
    pub const PROBE: &'static str = "d=/data/local/tmp/.drums_probe; rm -rf $d; mkdir -p $d; settings list system > $d/1 & settings list secure > $d/2 & settings list global > $d/3 & pm list packages > $d/4 & content query --uri content://com.android.contacts/raw_contacts --projection display_name > $d/5 2>/dev/null & content query --uri content://com.android.calendar/events --projection title > $d/6 2>/dev/null & wait; cat $d/1 $d/2 $d/3 | grep -vE '{EXCLUDE}' | sort; find /sdcard/ -type f 2>/dev/null | sort; sort $d/4; sort $d/5; sort $d/6; rm -rf $d; ls /data/local/tmp 2>/dev/null | sort";
}
