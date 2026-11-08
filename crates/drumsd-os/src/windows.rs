//! Windows desktops: QEMU/KVM VMs with copy-on-write disks and memory snapshots.
//!
//! The clean image is a frozen qcow2 (`clean_disk`), a saved VM state (`clean_state`, a QEMU migration stream taken
//! with the VM stopped at the moment the disk was frozen) and the UEFI variable store at that moment (`clean_vars`).
//! osvm/hosts/windows-clean-state.sh makes them from the installed golden disk with the same `vm_args`.
//! A desktop = a fresh qcow2 overlay on the clean disk + a copy of the vars + a QEMU process that loads the clean
//! state (`-incoming defer`, `migrate-incoming exec:cat <state>`). Reset = kill QEMU, new overlay, load again.
//! Actions and checkers go through the in-guest server (WindowsAgentArena's, port 5000, OSWorld-compatible
//! `/execute`); screenshots come from QEMU's display (QMP `screendump`), not from the guest.
//! Capture = stop, write the state, freeze the overlay (`blockdev-snapshot-sync`), continue: a saved state that
//! children start from exactly like the clean one.

use crate::exec::{run, secs};
use crate::http;
use crate::qmp::Qmp;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn d_qemu() -> String {
    "qemu-system-x86_64".into()
}
fn d_img() -> String {
    "qemu-img".into()
}
fn d_gport() -> u16 {
    5000
}
fn d_ready() -> f64 {
    120.0
}
fn d_python() -> String {
    "python".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowsCfg {
    #[serde(default = "d_qemu")]
    pub qemu: String,
    #[serde(default = "d_img")]
    pub qemu_img: String,
    pub clean_disk: PathBuf,
    pub clean_state: PathBuf,
    pub clean_vars: PathBuf,
    /// Machine, CPU, memory, firmware code and device arguments, identical to the ones the clean state was saved with.
    /// `{DISK}` and `{VARS}` are replaced with the desktop's overlay and vars paths.
    pub vm_args: Vec<String>,
    /// NIC model for the user-mode network (the guest server is forwarded from 127.0.0.1:<port>).
    pub nic: String,
    #[serde(default = "d_gport")]
    pub guest_port: u16,
    #[serde(default = "d_ready")]
    pub ready_timeout_s: f64,
    /// Interpreter in the guest that has pyautogui.
    #[serde(default = "d_python")]
    pub python: String,
    pub work_dir: PathBuf,
}

/// A saved VM state a desktop can start from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WinImage {
    pub disk: PathBuf,
    pub state: PathBuf,
    pub vars: PathBuf,
}

pub struct WinDesk {
    pub cfg: WindowsCfg,
    pub name: String,
    pub port: u16,
    pub image: WinImage,
    child: Option<tokio::process::Child>,
    gen: u32,
    /// Overlays frozen by captures (saved states point at them): never deleted by the desktop.
    pub frozen: Vec<PathBuf>,
}

pub const PYAUTOGUI_PREFIX: &str = "import pyautogui; import time; pyautogui.FAILSAFE = False; ";

impl WinDesk {
    pub fn new(cfg: WindowsCfg, name: String, port: u16, image: WinImage) -> WinDesk {
        WinDesk { cfg, name, port, image, child: None, gen: 0, frozen: vec![] }
    }
    fn dir(&self) -> PathBuf {
        self.cfg.work_dir.join(&self.name)
    }
    fn qmp_path(&self) -> PathBuf {
        self.dir().join("qmp.sock")
    }
    fn disk_path(&self) -> PathBuf {
        self.dir().join(format!("disk-{}.qcow2", self.gen))
    }
    fn vars_path(&self) -> PathBuf {
        self.dir().join(format!("vars-{}.fd", self.gen))
    }
    pub fn clean_image(cfg: &WindowsCfg) -> WinImage {
        WinImage { disk: cfg.clean_disk.clone(), state: cfg.clean_state.clone(), vars: cfg.clean_vars.clone() }
    }

    pub async fn start(&mut self) -> Result<Value, String> {
        let t0 = std::time::Instant::now();
        std::fs::create_dir_all(self.dir()).map_err(|e| e.to_string())?;
        self.gen += 1;
        let disk = self.disk_path();
        let o = run(&self.cfg.qemu_img, &["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", &self.image.disk.to_string_lossy(), &disk.to_string_lossy()], None, secs(30.0)).await;
        if !o.ok() {
            return Err(format!("qemu-img create: {}", o.stderr.trim()));
        }
        std::fs::copy(&self.image.vars, self.vars_path()).map_err(|e| format!("vars copy: {e}"))?;
        let t_disk = t0.elapsed().as_secs_f64();
        let _ = std::fs::remove_file(self.qmp_path());
        let mut args: Vec<String> = self.cfg.vm_args.iter().map(|a| a.replace("{DISK}", &disk.to_string_lossy()).replace("{VARS}", &self.vars_path().to_string_lossy())).collect();
        args.extend([
            "-name".into(),
            self.name.clone(),
            "-netdev".into(),
            format!("user,id=n0,hostfwd=tcp:127.0.0.1:{}-:{}", self.port, self.cfg.guest_port),
            "-device".into(),
            format!("{},netdev=n0", self.cfg.nic),
            "-qmp".into(),
            format!("unix:{},server=on,wait=off", self.qmp_path().display()),
            "-display".into(),
            "none".into(),
            "-incoming".into(),
            "defer".into(),
        ]);
        let log = std::fs::File::create(self.dir().join(format!("qemu-{}.log", self.gen))).map_err(|e| e.to_string())?;
        let child = tokio::process::Command::new(&self.cfg.qemu).args(&args).stdout(log.try_clone().unwrap()).stderr(log).kill_on_drop(true).spawn().map_err(|e| format!("qemu spawn: {e}"))?;
        self.child = Some(child);
        let mut q = Qmp::connect(&self.qmp_path(), 30.0).await?;
        let t_spawn = t0.elapsed().as_secs_f64();
        q.cmd("migrate-incoming", json!({"uri": format!("exec:cat {}", self.image.state.display())})).await?;
        let st = q.wait_not_inmigrate(300.0).await?;
        if st != "running" {
            // -incoming leaves the VM paused only with -S; be explicit
            q.cmd("cont", json!({})).await?;
        }
        let t_load = t0.elapsed().as_secs_f64();
        self.wait_guest(self.cfg.ready_timeout_s).await?;
        let t_ready = t0.elapsed().as_secs_f64();
        Ok(json!({"disk_s": t_disk, "spawn_s": t_spawn - t_disk, "load_s": t_load - t_spawn, "guest_s": t_ready - t_load, "total_s": t_ready}))
    }

    pub async fn wait_guest(&self, timeout_s: f64) -> Result<f64, String> {
        let t0 = std::time::Instant::now();
        loop {
            if let Ok(r) = http::request(self.port, "GET", "/probe", None, 2.0).await {
                if r.status == 200 {
                    return Ok(t0.elapsed().as_secs_f64());
                }
            }
            if t0.elapsed().as_secs_f64() > timeout_s {
                return Err(format!("guest server not answering on :{} after {timeout_s:.0} s", self.port));
            }
            tokio::time::sleep(secs(0.02)).await;
        }
    }

    async fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
    }
    fn drop_gen_files(&self) {
        let (d, v) = (self.disk_path(), self.vars_path());
        if !self.frozen.contains(&d) {
            let _ = std::fs::remove_file(d);
        }
        let _ = std::fs::remove_file(v);
    }

    pub async fn reset(&mut self) -> Result<Value, String> {
        let t0 = std::time::Instant::now();
        self.kill().await;
        self.drop_gen_files();
        let down = t0.elapsed().as_secs_f64();
        let mut v = self.start().await?;
        v["down_s"] = json!(down);
        v["total_s"] = json!(t0.elapsed().as_secs_f64());
        v["kind"] = json!("state_load");
        Ok(v)
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        self.kill().await;
        self.drop_gen_files();
        Ok(())
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(None)))
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>, String> {
        let p = self.dir().join("shot.png");
        let mut q = Qmp::connect(&self.qmp_path(), 5.0).await?;
        q.cmd("screendump", json!({"filename": p.to_string_lossy(), "format": "png"})).await?;
        std::fs::read(&p).map_err(|e| format!("screendump read: {e}"))
    }

    /// Run a command through the guest server's /execute.
    pub async fn execute(&self, command: Value, shell: bool, timeout_s: f64) -> Result<Value, String> {
        let r = http::request(self.port, "POST", "/execute", Some(&json!({"command": command, "shell": shell})), timeout_s).await?;
        let v: Value = serde_json::from_slice(&r.body).map_err(|e| format!("guest /execute status {} not json: {e}", r.status))?;
        if r.status != 200 {
            return Err(format!("guest /execute status {}: {}", r.status, v));
        }
        Ok(v)
    }
    /// PowerShell command (checkers, setup, probes). Returns (returncode, stdout, stderr).
    pub async fn powershell(&self, cmd: &str, timeout_s: f64) -> Result<(i32, String, String), String> {
        let v = self.execute(json!(["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", cmd]), false, timeout_s).await?;
        Ok((v["returncode"].as_i64().unwrap_or(-1) as i32, v["output"].as_str().unwrap_or("").replace("\r\n", "\n"), v["error"].as_str().unwrap_or("").to_string()))
    }

    /// One action, in the OSWorld/Linux shape: a pyautogui string, or {type: pyautogui|execute|launch|sleep}.
    pub async fn act(&self, a: &Value) -> Result<Value, String> {
        let (kind, code) = match a {
            Value::String(s) => ("pyautogui", s.clone()),
            _ => (a.get("type").and_then(|v| v.as_str()).ok_or("action needs type")?, a.get("code").and_then(|v| v.as_str()).unwrap_or("").to_string()),
        };
        match kind {
            "pyautogui" => self.execute(json!([self.cfg.python, "-c", format!("{PYAUTOGUI_PREFIX}{code}")]), false, 130.0).await,
            "execute" | "launch" => {
                let cmd = a.get("command").cloned().ok_or("command missing")?;
                let shell = a.get("shell").and_then(|v| v.as_bool()).unwrap_or(false);
                if kind == "launch" {
                    let r = http::request(self.port, "POST", "/setup/launch", Some(&json!({"command": cmd, "shell": shell})), 30.0).await?;
                    return Ok(json!({"status": if r.status == 200 {"ok"} else {"error"}, "http": r.status, "output": String::from_utf8_lossy(&r.body)}));
                }
                self.execute(cmd, shell, 130.0).await
            }
            "powershell" => {
                let c = a.get("command").and_then(|v| v.as_str()).ok_or("command missing")?;
                let (rc, out, err) = self.powershell(c, 130.0).await?;
                Ok(json!({"status": "ok", "returncode": rc, "output": out, "error": err}))
            }
            "sleep" | "wait" => {
                let s = a.get("seconds").and_then(|v| v.as_f64()).unwrap_or(1.0).clamp(0.0, 60.0);
                tokio::time::sleep(secs(s)).await;
                Ok(json!({"status": "ok", "slept_s": s}))
            }
            k => Err(format!("unknown windows action {k}")),
        }
    }

    /// Capture the running desktop into `dst_dir` (state file, vars copy; the current overlay is frozen in place).
    pub async fn capture(&mut self, dst_dir: &Path) -> Result<(WinImage, Value), String> {
        let t0 = std::time::Instant::now();
        std::fs::create_dir_all(dst_dir).map_err(|e| e.to_string())?;
        let state = dst_dir.join("vm.state");
        let vars = dst_dir.join("vars.fd");
        let mut q = Qmp::connect(&self.qmp_path(), 5.0).await?;
        q.cmd("stop", json!({})).await?;
        let r = async {
            q.cmd("migrate", json!({"uri": format!("exec:cat > {}", state.display())})).await?;
            let m = q.wait_migrated(600.0).await?;
            let t_state = t0.elapsed().as_secs_f64();
            std::fs::copy(self.vars_path(), &vars).map_err(|e| format!("vars copy: {e}"))?;
            let frozen = self.disk_path();
            self.gen += 1;
            let next = self.disk_path();
            // the old vars file of this generation stays with the desktop; the new generation needs its own name
            std::fs::copy(dst_dir.join("vars.fd"), self.vars_path()).map_err(|e| e.to_string())?;
            q.cmd("blockdev-snapshot-sync", json!({"device": "d0", "snapshot-file": next.to_string_lossy(), "format": "qcow2"})).await?;
            self.frozen.push(frozen.clone());
            Ok::<_, String>((WinImage { disk: frozen, state: state.clone(), vars: vars.clone() }, json!({"state_s": t_state, "state_bytes": m["ram"]["total"], "migrate": m})))
        }
        .await;
        // the VM keeps running whatever happened (a failed capture leaves it paused otherwise)
        let _ = q.cmd("cont", json!({})).await;
        let (img, mut v) = r?;
        v["total_s"] = json!(t0.elapsed().as_secs_f64());
        Ok((img, v))
    }

    /// Default probe for the state-hash gate (PowerShell): the user's files and the registry values tasks touch.
    pub const PROBE: &'static str = r#"$ErrorActionPreference='SilentlyContinue'; $h=$env:USERPROFILE; foreach($d in 'Desktop','Documents','Downloads','Pictures','Music','Videos'){ Get-ChildItem -Recurse -Force (Join-Path $h $d) | Sort-Object FullName | ForEach-Object { '{0}|{1}' -f $_.FullName.Substring($h.Length), $(if($_.PSIsContainer){'d'}else{$_.Length}) } }; Get-ChildItem -Force C:\Users\Public\Desktop | ForEach-Object { 'pub|'+$_.Name }; foreach($k in 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize','HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced','HKCU:\Control Panel\Desktop','HKCU:\Environment','HKCU:\Control Panel\Mouse','HKCU:\Control Panel\International'){ $p=Get-ItemProperty $k; if($p){ $p.PSObject.Properties | Where-Object { $_.Name -notlike 'PS*' -and $_.Name -notmatch '{EXCLUDE}' } | Sort-Object Name | ForEach-Object { $k+'|'+$_.Name+'='+$_.Value } } }; (Get-TimeZone).Id; Get-ChildItem -Force C:\Users\Public\drums_canary_* | ForEach-Object { 'canary|'+$_.Name }"#;
}
