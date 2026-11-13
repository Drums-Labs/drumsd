//! Trusted provisioning: a task's setup step that needs root runs here, started by drumsd, never by the agent.
//!
//! The desktop user (the agent) has no privilege escalation in a slot: it is in no administrator group and every
//! process it can drive has `no_new_privs`, so `sudo` refuses. OSWorld tasks still write setup steps as
//! `echo {CLIENT_PASSWORD} | sudo -S <cmd>` (e0df059f creates its folder that way). drumsd runs such a step itself:
//! one short-lived process that joins the slot's cgroup, enters the slot's mount, uts, ipc, net and pid namespaces
//! and its root (`nsenter`), and runs the step's command there as the desktop user, as the guest server would, with
//! two differences: no `no_new_privs`, and a sudoers rule `USER ALL=(ALL:ALL) NOPASSWD: ALL` that exists only for the
//! length of the step (a file in the slot's /etc/sudoers.d, removed when the step ends, and again by a second pass in
//! case the first was killed). The parts of the command under `sudo` run as root, the rest as the user, exactly as on
//! the VM. While the rule exists, every process the agent can drive still has `no_new_privs`, so it cannot use it.
//!
//! A step runs only while drumsd is setting up an env (before the env is handed out, or in a checker's postconfig),
//! through a one-time capability URL handed to the OSWorld helper for that call (see the manager's `osw`).

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SECURE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub const SUDOERS_PREFIX: &str = "/etc/sudoers.d/zz-drums-provision-";

/// Inside the slot, as root: the sudoers rule for the step, then the command as the user, then the rule's removal.
/// $1.. is the command (argv); DRUMS_* come from the environment drumsd sets.
pub const SCRIPT: &str = r#"set -u
f=/etc/sudoers.d/zz-drums-provision-$$
( umask 0337 && printf '%s ALL=(ALL:ALL) NOPASSWD: ALL\n' "$DRUMS_USER" > "$f" ) || exit 125
cd "$DRUMS_HOME" 2>/dev/null || cd /
setpriv --reuid="$DRUMS_UID" --regid="$DRUMS_GID" --init-groups -- env -i HOME="$DRUMS_HOME" USER="$DRUMS_USER" LOGNAME="$DRUMS_USER" \
  SHELL=/bin/bash PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin DISPLAY=:0 LANG=C.UTF-8 \
  XDG_RUNTIME_DIR="/run/user/$DRUMS_UID" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$DRUMS_UID/bus" "$@" < /dev/null
rc=$?
rm -f "$f"
exit $rc
"#;

#[derive(Clone, Debug)]
pub struct ProvisionSpec {
    /// The slot's init (host pid): its namespaces and root are entered.
    pub init_pid: i32,
    /// The cgroup the step runs in (the episode's leaf): it is killed with the tree and counted in its limits.
    pub cgroup: PathBuf,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    /// argv run as the user (a shell command is `["/bin/sh", "-c", CMD]`).
    pub argv: Vec<String>,
    pub timeout: Duration,
    /// The slot has its own user namespace (the hardened slot profile): the step enters it too, so it runs as the
    /// slot's mapped root/user (unprivileged on the host) and the sudoers rule it writes is owned by the slot's root.
    pub userns: bool,
}

/// Is `pid` in a user namespace other than drumsd's own?
pub fn own_userns(pid: i32) -> bool {
    match (std::fs::read_link(format!("/proc/{pid}/ns/user")), std::fs::read_link("/proc/self/ns/user")) {
        (Ok(a), Ok(b)) => a != b,
        _ => false,
    }
}

fn ns_flags(spec: &ProvisionSpec, all: bool) -> Vec<String> {
    let mut v = vec!["-t".to_string(), spec.init_pid.to_string()];
    if spec.userns {
        v.push("-U".into());
    }
    let f: &[&str] = if all { &["-m", "-u", "-i", "-n", "-p", "-r", "-w"] } else { &["-m", "-r"] };
    v.extend(f.iter().map(|s| s.to_string()));
    v
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ProvisionOut {
    pub returncode: Option<i32>,
    pub output: String,
    pub error: String,
    pub timed_out: bool,
    pub seconds: f64,
}

/// The argv for a step's command: OSWorld's `{"command": ..., "shell": ...}` (a string with shell, or a list).
pub fn step_argv(command: &serde_json::Value, shell: bool) -> Result<Vec<String>, String> {
    match command {
        serde_json::Value::String(s) if shell => Ok(vec!["/bin/sh".into(), "-c".into(), s.clone()]),
        serde_json::Value::String(s) => Ok(vec!["/bin/sh".into(), "-c".into(), s.clone()]),
        serde_json::Value::Array(a) => {
            let v: Option<Vec<String>> = a.iter().map(|x| x.as_str().map(|s| s.to_string())).collect();
            let v = v.ok_or("command list items must be strings")?;
            if v.is_empty() {
                return Err("empty command".into());
            }
            if shell {
                // OSWorld's server joins a list with spaces for a shell (subprocess with shell=True takes argv[0] as the script)
                Ok(vec!["/bin/sh".into(), "-c".into(), v.join(" ")])
            } else {
                Ok(v)
            }
        }
        _ => Err("command must be a string or a list of strings".into()),
    }
}

/// The nsenter command line (host side) for a step.
pub fn command(spec: &ProvisionSpec) -> Command {
    let mut c = Command::new("nsenter");
    c.args(ns_flags(spec, true));
    c.args(["--", "/bin/sh", "-c", SCRIPT, "drums-provision"]);
    c.args(&spec.argv);
    c.env_clear()
        .env("PATH", SECURE_PATH)
        .env("DRUMS_USER", &spec.user)
        .env("DRUMS_UID", spec.uid.to_string())
        .env("DRUMS_GID", spec.gid.to_string())
        .env("DRUMS_HOME", &spec.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn join_cgroup_and_group(cg: &Path) -> io::Result<()> {
    // in the child, before exec: its own process group (a timeout kills nsenter and everything below it) and the
    // slot's cgroup (a reset or destroy kills what the step left running, and the slot's limits apply)
    unsafe {
        libc::setpgid(0, 0);
    }
    std::fs::write(cg.join("cgroup.procs"), b"0")
}

fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut b = vec![];
        let _ = r.read_to_end(&mut b);
        String::from_utf8_lossy(&b).into_owned()
    })
}

/// Run one step. The slot's init must be alive. Blocking.
pub fn run(spec: &ProvisionSpec) -> io::Result<ProvisionOut> {
    let t0 = Instant::now();
    let mut c = command(spec);
    let cg = spec.cgroup.clone();
    unsafe {
        c.pre_exec(move || join_cgroup_and_group(&cg));
    }
    let mut child = c.spawn()?;
    let pid = child.id() as i32;
    let out_h = drain(child.stdout.take().unwrap());
    let err_h = drain(child.stderr.take().unwrap());
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if t0.elapsed() > spec.timeout {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            timed_out = true;
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = out_h.join().unwrap_or_default();
    let error = err_h.join().unwrap_or_default();
    // the rule must not outlive the step, even when the step was killed before its own `rm`
    let _ = cleanup(spec);
    Ok(ProvisionOut { returncode: status.and_then(|s| s.code()), output, error, timed_out, seconds: t0.elapsed().as_secs_f64() })
}

/// Remove every provisioning sudoers rule in the slot (they are named zz-drums-provision-<pid>).
pub fn cleanup(spec: &ProvisionSpec) -> io::Result<bool> {
    let st = Command::new("nsenter")
        .args(ns_flags(spec, false))
        .args(["--", "/bin/sh", "-c", "rm -f /etc/sudoers.d/zz-drums-provision-*"])
        .env_clear()
        .env("PATH", SECURE_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(st.success())
}

/// Does a setup step need root (provisioning) rather than the desktop user? A step whose command runs `sudo`.
pub fn needs_root(command: &serde_json::Value) -> bool {
    let text = match command {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
        _ => return false,
    };
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.')).any(|w| w == "sudo" || w.ends_with("/sudo"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn steps_that_run_sudo_need_root() {
        assert!(needs_root(&json!("echo {CLIENT_PASSWORD} | sudo -S mkdir ~/Desktop/todo_list_Jan_1")));
        assert!(needs_root(&json!(["bash", "-c", "echo '{CLIENT_PASSWORD}' | sudo -S timedatectl set-timezone Asia/Tokyo"])));
        assert!(needs_root(&json!(["/usr/bin/sudo", "true"])));
        assert!(needs_root(&json!("sudo chown -R user:user /home/user/.config/google-chrome/Default/History")));
        assert!(!needs_root(&json!(["python3", "/home/user/acme/acme_app.py"])));
        assert!(!needs_root(&json!("echo pseudo-sudoku > x")));
        assert!(!needs_root(&json!(42)));
    }

    #[test]
    fn step_argv_matches_the_guest_servers_forms() {
        assert_eq!(step_argv(&json!("a && b"), true).unwrap(), vec!["/bin/sh", "-c", "a && b"]);
        assert_eq!(step_argv(&json!(["bash", "-c", "x"]), false).unwrap(), vec!["bash", "-c", "x"]);
        assert_eq!(step_argv(&json!(["ls", "-la"]), true).unwrap(), vec!["/bin/sh", "-c", "ls -la"]);
        assert!(step_argv(&json!([]), false).is_err());
        assert!(step_argv(&json!([1]), false).is_err());
    }

    #[test]
    fn the_command_enters_the_slot_and_the_script_scopes_the_rule_to_the_step() {
        let s = ProvisionSpec {
            init_pid: 42,
            cgroup: "/sys/fs/cgroup/x".into(),
            user: "user".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/user".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "mkdir /x".into()],
            timeout: Duration::from_secs(1),
            userns: false,
        };
        let c = command(&s);
        let a: Vec<String> = c.get_args().map(|x| x.to_string_lossy().to_string()).collect();
        assert_eq!(&a[..12], &["-t", "42", "-m", "-u", "-i", "-n", "-p", "-r", "-w", "--", "/bin/sh", "-c"]);
        assert_eq!(&a[13..], &["drums-provision", "/bin/sh", "-c", "mkdir /x"]);
        // the rule is written before the command and removed after it; the command runs as the user, never as root
        let w = SCRIPT.find("NOPASSWD").unwrap();
        let r = SCRIPT.find("setpriv --reuid=\"$DRUMS_UID\"").unwrap();
        let rm = SCRIPT.find("rm -f \"$f\"").unwrap();
        assert!(w < r && r < rm);
        assert!(!SCRIPT.contains("--no-new-privs"));
        assert!(SCRIPT.contains(SUDOERS_PREFIX));
        // a hardened slot (own user namespace): the step enters it first, so it is never host root inside the slot
        let h = ProvisionSpec { userns: true, ..s };
        let a: Vec<String> = command(&h).get_args().map(|x| x.to_string_lossy().to_string()).collect();
        assert_eq!(&a[..4], &["-t", "42", "-U", "-m"]);
        assert!(!own_userns(std::process::id() as i32), "drumsd's own process is in its own user namespace");
    }
}
