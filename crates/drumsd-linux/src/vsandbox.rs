//! The verifier sandbox (docs/design/verifier-sandbox.md): a lab verifier is client code, so each checker call runs
//! in a fresh, confined sibling of the desktop, never as a host process with drumsd's or the helper's rights.
//!
//! Two halves:
//!
//! * [`run`] (drumsd, the parent): makes a cgroup with the call's budgets, starts `drumsd verifier-sandbox`, hands it the
//!   [`Spec`] on its stdin, then brokers the checker's `ctx` requests (read a desktop file, run a desktop command) from
//!   the slot's guest server, enforces the wall, request and read budgets, and turns what came back into a verdict.
//! * [`sandbox_main`] (the `verifier-sandbox` internal mode, a fresh single-threaded process): joins the cgroup,
//!   unshares mount, network, pid, ipc, uts and cgroup namespaces, builds a read-only root from an allow list and
//!   `pivot_root`s into it, drops to an unprivileged uid with `no_new_privs`, installs a seccomp filter, and execs the
//!   checker venv's Python on the embedded standard-library runner (osw/verifier_runner.py).
//!
//! The checker reaches nothing but its own pipe: no network (an empty network namespace), no host file outside the
//! allow list, no process outside its pid namespace.

use crate::guest::Guest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// The runner the sandbox execs (standard library only).
pub const RUNNER: &str = include_str!("../../../osw/verifier_runner.py");
/// Where the verifier, the runner and the spec appear inside the sandbox.
pub const IN_VERIFIER: &str = "/verifier/verifier.py";
pub const IN_RUNNER: &str = "/verifier/runner.py";
pub const IN_SPEC: &str = "/verifier/spec.json";
pub const IN_WORKDIR: &str = "/tmp/work";
/// The read-only allow list a checker sees besides the checker venv (missing paths are skipped).
pub const DEFAULT_RO_PATHS: &[&str] = &["/usr", "/lib", "/lib64", "/lib32", "/bin", "/sbin", "/etc/ld.so.cache", "/etc/localtime", "/etc/alternatives"];
/// A request line from the checker (a path or a command) is small; anything longer is a protocol error.
const MAX_REQUEST_LINE: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    /// Wall seconds for one call when the task names no `deadline_s`.
    pub wall_s: f64,
    /// The most a task's `evaluator.deadline_s` may ask for.
    pub max_wall_s: f64,
    /// CPU-seconds (RLIMIT_CPU: SIGXCPU, then SIGKILL one second later).
    pub cpu_s: u64,
    /// CPUs at a time (cgroup cpu.max).
    pub cpus: f64,
    /// cgroup memory.max (no swap).
    pub memory_mib: u64,
    /// cgroup pids.max.
    pub pids: u64,
    /// Size of the writable /tmp (RLIMIT_FSIZE too).
    pub tmp_mib: u64,
    /// Total bytes `ctx.read_file` may return in one call.
    pub read_bytes: u64,
    /// `ctx` requests in one call.
    pub requests: u64,
    /// The verdict line, at most.
    pub verdict_bytes: usize,
    /// The checker's stderr kept in the verdict's `log` (its tail).
    pub log_bytes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            wall_s: 90.0,
            max_wall_s: 600.0,
            cpu_s: 60,
            cpus: 1.0,
            memory_mib: 1024,
            pids: 64,
            tmp_mib: 256,
            read_bytes: 256 << 20,
            requests: 10_000,
            verdict_bytes: 1 << 20,
            log_bytes: 4096,
        }
    }
}

impl Budget {
    /// The wall budget of a call whose task asks for `deadline_s` (none or invalid: the default; capped at max_wall_s).
    pub fn wall_for(&self, deadline_s: Option<f64>) -> f64 {
        match deadline_s {
            Some(d) if d.is_finite() && d > 0.0 => d.min(self.max_wall_s),
            _ => self.wall_s.min(self.max_wall_s),
        }
    }
}

/// What the sandbox process is told (one JSON line on its stdin).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    /// The checker venv's python (its venv directory must be in `ro_paths`).
    pub python: PathBuf,
    /// Host paths visible read-only at the same place inside (directories, files or symlinks).
    pub ro_paths: Vec<PathBuf>,
    /// The verifier's source on the host (the store file).
    pub verifier: PathBuf,
    pub sha256: String,
    pub function: String,
    pub options: Value,
    pub task: Value,
    pub uid: u32,
    pub gid: u32,
    /// This call's cgroup (made by the parent with the budgets).
    pub cgroup: PathBuf,
    /// An empty directory on the host: the new root is mounted on it inside the sandbox's own mount namespace.
    pub root: PathBuf,
    pub budget: Budget,
    pub wall_s: f64,
}

/// What a checker can ask of its desktop.
pub trait Desktop: Send + Sync {
    /// The file's bytes, None if it does not exist; an error if it is larger than `max_bytes` or the read failed.
    fn read_file(&self, path: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>>;
    /// Run a command in the desktop as its user: {"returncode", "output", "error"}.
    fn run(&self, command: &Value, shell: bool, timeout: Duration) -> io::Result<Value>;
}

/// The slot's guest server as a [`Desktop`] (OSWorld's `/file` and `/execute` routes).
pub struct GuestDesktop {
    pub port: u16,
}

impl Desktop for GuestDesktop {
    fn read_file(&self, path: &str, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        let g = Guest::new(self.port, Duration::from_secs(120));
        let r = g.post_form("/file", &[("file_path", path)], max_bytes.saturating_add(64 << 10))?;
        match r.status {
            200 if r.body.len() as u64 > max_bytes => Err(io::Error::other(format!("{path}: larger than the {max_bytes} bytes left of the read budget"))),
            200 => Ok(Some(r.body)),
            404 => Ok(None),
            s => Err(io::Error::other(format!("guest /file HTTP {s}: {}", String::from_utf8_lossy(&r.body).chars().take(200).collect::<String>()))),
        }
    }
    fn run(&self, command: &Value, shell: bool, timeout: Duration) -> io::Result<Value> {
        let g = Guest::new(self.port, timeout);
        let body = serde_json::to_vec(&json!({"command": command, "shell": shell})).unwrap();
        let r = g.request_raw("POST", "/execute", "application/json", &body, 64 << 20)?;
        if r.status != 200 {
            return Err(io::Error::other(format!(
                "guest /execute HTTP {}: {}",
                r.status,
                String::from_utf8_lossy(&r.body).chars().take(200).collect::<String>()
            )));
        }
        let v: Value = serde_json::from_slice(&r.body).map_err(|e| io::Error::other(format!("guest /execute: {e}")))?;
        Ok(
            json!({"returncode": v.get("returncode"), "output": v.get("output").cloned().unwrap_or(json!("")), "error": v.get("error").cloned().unwrap_or(json!(""))}),
        )
    }
}

/// How a call is run.
pub enum Confine {
    /// The real thing: `exe verifier-sandbox` (root), one cgroup per call under `cgroup_parent`, new roots under `run_dir`.
    Sandbox { exe: PathBuf, cgroup_parent: PathBuf, run_dir: PathBuf },
    /// Tests of the broker only: the runner as a plain child process, no confinement at all.
    Unconfined,
}

/// The cgroup that holds every call's cgroup: created if missing, with cpu, memory and pids enabled for its children.
pub fn ensure_parent(parent: &Path) -> io::Result<()> {
    fs::create_dir_all(parent)?;
    let sc = fs::read_to_string(parent.join("cgroup.subtree_control")).unwrap_or_default();
    if !(sc.contains("cpu") && sc.contains("memory") && sc.contains("pids")) {
        fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
    }
    Ok(())
}

fn unique_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("v{}-{}-{:08x}", std::process::id(), N.fetch_add(1, Ordering::Relaxed), ns)
}

fn cg_read(cg: &Path, file: &str, key: &str) -> Option<u64> {
    let s = fs::read_to_string(cg.join(file)).ok()?;
    if key.is_empty() {
        return s.trim().parse().ok();
    }
    s.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(' ')).and_then(|v| v.trim().parse().ok()))
}

fn cg_remove(cg: &Path) {
    let _ = fs::write(cg.join("cgroup.kill"), "1");
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if fs::read_to_string(cg.join("cgroup.events")).map(|e| e.contains("populated 0")).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_dir(cg);
}

/// The runner's spec (the file the runner reads) for a verifier at `verifier` and a workdir.
pub fn runner_spec(spec: &Spec, verifier: &str, workdir: &str) -> Value {
    json!({"verifier": verifier, "function": spec.function, "options": spec.options, "task": spec.task, "workdir": workdir})
}

fn error_verdict(class: &str, msg: impl Into<String>) -> Value {
    json!({"score": null, "info": null, "error": msg.into(), "error_class": class})
}

/// Read one line (up to and including '\n'); Ok(None) at EOF; an error past `cap` bytes.
fn read_line_capped(r: &mut impl BufRead, cap: usize) -> io::Result<Option<Vec<u8>>> {
    let mut out = vec![];
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok(if out.is_empty() { None } else { Some(out) });
        }
        let (take, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        out.extend_from_slice(&buf[..take]);
        r.consume(take);
        if out.len() > cap {
            return Err(io::Error::other(format!("a line from the checker is longer than {cap} bytes")));
        }
        if done {
            return Ok(Some(out));
        }
    }
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

/// One checker call. Never panics on anything the checker does; every failure is a verdict with an error class:
/// none | exception | bad_return (the checker's own) | timeout | cpu | oom | protocol | sandbox.
pub fn run(confine: &Confine, spec: &Spec, desktop: &dyn Desktop) -> Value {
    let t0 = Instant::now();
    let mut spec = spec.clone();
    let wall = Duration::from_secs_f64(spec.wall_s.max(0.1));
    let b = spec.budget.clone();
    let tmp_guard: Option<PathBuf>;
    let (mut cmd, cg, first_line) = match confine {
        Confine::Sandbox { exe, cgroup_parent, run_dir } => {
            if spec.uid == 0 || spec.gid == 0 {
                return finish(error_verdict("sandbox", "the verifier sandbox never runs a checker as root (set verifiers.uid/gid)"), &spec, t0, None, 0, 0);
            }
            let id = unique_id();
            let cg = cgroup_parent.join(&id);
            let setup = (|| -> io::Result<()> {
                ensure_parent(cgroup_parent)?;
                fs::create_dir(&cg)?;
                fs::write(cg.join("memory.max"), (b.memory_mib << 20).to_string())?;
                let _ = fs::write(cg.join("memory.swap.max"), "0"); // absent without swap accounting
                fs::write(cg.join("pids.max"), b.pids.to_string())?;
                fs::write(cg.join("cpu.max"), format!("{} 100000", ((b.cpus.max(0.01)) * 100000.0) as u64))?;
                fs::create_dir_all(run_dir)?;
                let root = run_dir.join(&id);
                fs::create_dir(&root)?;
                spec.root = root;
                Ok(())
            })();
            if let Err(e) = setup {
                cg_remove(&cg);
                return finish(error_verdict("sandbox", format!("verifier sandbox setup: {e}")), &spec, t0, None, 0, 0);
            }
            spec.cgroup = cg.clone();
            tmp_guard = Some(spec.root.clone());
            let mut c = Command::new(exe);
            c.arg("verifier-sandbox").env_clear().env("PATH", "/usr/bin:/bin");
            let line = serde_json::to_vec(&spec).unwrap();
            (c, Some(cg), Some(line))
        }
        Confine::Unconfined => {
            let d = std::env::temp_dir().join(format!("drums-vtest-{}", unique_id()));
            let made = (|| -> io::Result<()> {
                fs::create_dir_all(d.join("work"))?;
                fs::write(d.join("runner.py"), RUNNER)?;
                fs::write(
                    d.join("spec.json"),
                    serde_json::to_vec(&runner_spec(&spec, &spec.verifier.to_string_lossy(), &d.join("work").to_string_lossy())).unwrap(),
                )
            })();
            if let Err(e) = made {
                return finish(error_verdict("sandbox", format!("runner setup: {e}")), &spec, t0, None, 0, 0);
            }
            tmp_guard = Some(d.clone());
            let mut c = Command::new(&spec.python);
            c.arg("-I").arg("-B").arg(d.join("runner.py")).arg(d.join("spec.json"));
            use std::os::unix::process::CommandExt;
            c.process_group(0);
            (c, None, None)
        }
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(cg) = &cg {
                cg_remove(cg);
            }
            cleanup_dir(confine, tmp_guard.as_deref());
            return finish(error_verdict("sandbox", format!("start the verifier sandbox: {e}")), &spec, t0, None, 0, 0);
        }
    };
    let pid = child.id() as i32;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let log_cap = b.log_bytes;
    let log_thread = std::thread::spawn(move || {
        // keep the tail; drain everything so the checker never blocks on a full pipe
        let mut tail: Vec<u8> = vec![];
        let mut buf = [0u8; 8192];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 2 * log_cap.max(1) {
                tail.drain(..tail.len() - log_cap);
            }
        }
        if tail.len() > log_cap {
            tail.drain(..tail.len() - log_cap);
        }
        String::from_utf8_lossy(&tail).to_string()
    });
    // the wall budget: a watchdog kills the whole call at the deadline
    let timed_out = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let kill: Arc<dyn Fn() + Send + Sync> = match &cg {
        Some(cg) => {
            let cg = cg.clone();
            Arc::new(move || {
                let _ = fs::write(cg.join("cgroup.kill"), "1");
            })
        }
        None => Arc::new(move || unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }),
    };
    let watchdog = {
        let (timed_out, kill) = (timed_out.clone(), kill.clone());
        std::thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(wall) {
                timed_out.store(true, Ordering::SeqCst);
                kill();
            }
        })
    };
    if let Some(line) = first_line {
        let _ = stdin.write_all(&line).and_then(|_| stdin.write_all(b"\n"));
    }
    // the broker
    let mut reader = BufReader::with_capacity(1 << 16, stdout);
    let (mut requests, mut read_bytes) = (0u64, 0u64);
    let mut verdict: Option<Value> = None;
    let mut protocol: Option<String> = None;
    loop {
        let line = match read_line_capped(&mut reader, MAX_REQUEST_LINE.max(b.verdict_bytes)) {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                protocol = Some(e.to_string());
                kill();
                break;
            }
        };
        let msg: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(e) => {
                protocol = Some(format!("a line from the checker is not JSON: {e}"));
                kill();
                break;
            }
        };
        let op = msg["op"].as_str().unwrap_or("");
        if verdict.is_some() {
            protocol = Some(format!("the checker sent {op:?} after its verdict"));
            kill();
            break;
        }
        let reply = match op {
            "verdict" => {
                if line.len() > b.verdict_bytes {
                    protocol = Some(format!("the verdict is {} bytes; the limit is {}", line.len(), b.verdict_bytes));
                    kill();
                    break;
                }
                verdict = Some(msg);
                continue;
            }
            _ if requests >= b.requests => json!({"ok": false, "error": format!("the checker used its {} requests", b.requests)}),
            "read_file" => {
                requests += 1;
                match msg["path"].as_str() {
                    Some(p) if !p.is_empty() && p.len() <= 4096 => {
                        let left = b.read_bytes.saturating_sub(read_bytes);
                        match desktop.read_file(p, left) {
                            Ok(Some(data)) => {
                                read_bytes += data.len() as u64;
                                json!({"ok": true, "b64": b64(&data)})
                            }
                            Ok(None) => json!({"ok": true, "missing": true}),
                            Err(e) => json!({"ok": false, "error": format!("read_file {p}: {e}")}),
                        }
                    }
                    _ => json!({"ok": false, "error": "read_file: path must be a string of 1 to 4096 characters"}),
                }
            }
            "run" => {
                requests += 1;
                let left = wall.saturating_sub(t0.elapsed());
                let asked = msg["timeout"].as_f64().filter(|t| t.is_finite() && *t > 0.0).unwrap_or(120.0);
                let timeout = Duration::from_secs_f64(asked).min(left).max(Duration::from_millis(100));
                match &msg["command"] {
                    c @ (Value::String(_) | Value::Array(_)) => match desktop.run(c, msg["shell"].as_bool().unwrap_or(false), timeout) {
                        Ok(r) => json!({"ok": true, "result": r}),
                        Err(e) => json!({"ok": false, "error": format!("run: {e}")}),
                    },
                    _ => json!({"ok": false, "error": "run: command must be a string or a list"}),
                }
            }
            other => {
                protocol = Some(format!("unknown request {other:?}"));
                kill();
                break;
            }
        };
        let mut out = serde_json::to_vec(&reply).unwrap();
        out.push(b'\n');
        if stdin.write_all(&out).is_err() {
            break;
        }
    }
    drop(stdin);
    drop(reader);
    let status = child.wait().ok();
    let _ = done_tx.send(());
    let _ = watchdog.join();
    let log = log_thread.join().unwrap_or_default();
    let (cpu_us, mem_peak, ooms, pids_peak) = match &cg {
        Some(cg) => (
            cg_read(cg, "cpu.stat", "usage_usec"),
            cg_read(cg, "memory.peak", ""),
            cg_read(cg, "memory.events", "oom_kill").unwrap_or(0),
            cg_read(cg, "pids.peak", ""),
        ),
        None => (None, None, 0, None),
    };
    if let Some(cg) = &cg {
        cg_remove(cg);
    }
    cleanup_dir(confine, tmp_guard.as_deref());
    // what happened, most specific first
    let code = status.and_then(|s| s.code());
    let sig = status.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
    let killed_by = sig.or(code.filter(|c| *c > 128 && *c < 160).map(|c| c - 128)); // the sandbox's pid-ns parent forwards 128+sig
    let cpu_used = cpu_us.map(|u| u as f64 / 1e6);
    let mut v = if timed_out.load(Ordering::SeqCst) {
        error_verdict("timeout", format!("checker exceeded its {:.0} s wall budget", wall.as_secs_f64()))
    } else if ooms > 0 {
        error_verdict("oom", format!("checker exceeded its {} MiB memory budget (OOM kill)", b.memory_mib))
    } else if killed_by == Some(libc::SIGXCPU) || (killed_by == Some(libc::SIGKILL) && cpu_used.map(|c| c + 0.5 >= b.cpu_s as f64).unwrap_or(false)) {
        error_verdict("cpu", format!("checker exceeded its {} CPU-second budget", b.cpu_s))
    } else if let Some(p) = protocol {
        error_verdict("protocol", p)
    } else if let Some(vd) = verdict {
        check_verdict(vd)
    } else {
        error_verdict("protocol", format!("the checker exited without a verdict (exit status {:?}, signal {:?})", code, killed_by))
    };
    v["log"] = json!(log);
    finish(v, &spec, t0, mem_peak, requests, read_bytes).tap(|o| {
        if let Some(c) = cpu_used {
            o["cpu_s"] = json!((c * 1000.0).round() / 1000.0);
        }
        if let Some(p) = pids_peak {
            o["pids_peak"] = json!(p);
        }
    })
}

trait Tap {
    fn tap(self, f: impl FnOnce(&mut Self)) -> Self;
}
impl Tap for Value {
    fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}

fn cleanup_dir(confine: &Confine, d: Option<&Path>) {
    if let Some(d) = d {
        match confine {
            // the new root was a mount in the sandbox's own namespace: on the host the directory is empty
            Confine::Sandbox { .. } => {
                let _ = fs::remove_dir(d);
            }
            Confine::Unconfined => {
                let _ = fs::remove_dir_all(d);
            }
        }
    }
}

/// A verdict line as the runner sent it, checked: a finite score, or an error with a known class.
pub fn check_verdict(vd: Value) -> Value {
    let class = vd["error_class"].as_str().unwrap_or("none");
    if !matches!(class, "none" | "exception" | "bad_return" | "sandbox") {
        return error_verdict("protocol", format!("unknown error class {class:?} in the verdict"));
    }
    if class != "none" {
        return error_verdict(class, vd["error"].as_str().unwrap_or("checker error").chars().take(2000).collect::<String>());
    }
    match vd["score"].as_f64() {
        Some(s) if s.is_finite() => json!({"score": s, "info": vd.get("info").cloned().unwrap_or(Value::Null), "error": null, "error_class": "none"}),
        _ => error_verdict("bad_return", format!("verdict without a finite score: {}", vd["score"])),
    }
}

fn finish(mut v: Value, spec: &Spec, t0: Instant, mem_peak: Option<u64>, requests: u64, read_bytes: u64) -> Value {
    v["sha256"] = json!(spec.sha256);
    v["function"] = json!(spec.function);
    v["seconds"] = json!((t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0);
    v["requests"] = json!(requests);
    v["read_bytes"] = json!(read_bytes);
    if let Some(m) = mem_peak {
        v["memory_peak_mib"] = json!((m as f64 / 1048576.0 * 10.0).round() / 10.0);
    }
    v["budget"] = json!({"wall_s": spec.wall_s, "cpu_s": spec.budget.cpu_s, "memory_mib": spec.budget.memory_mib, "pids": spec.budget.pids,
                          "tmp_mib": spec.budget.tmp_mib, "read_bytes": spec.budget.read_bytes, "requests": spec.budget.requests});
    v
}

// ------------------------------------------------------------------------------------------------ the sandbox process

fn last_err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

fn check(r: libc::c_int, what: &str) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(last_err(what))
    }
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_encoded_bytes()).map_err(io::Error::other)
}

fn mount(src: Option<&Path>, dst: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let s = src.map(cstr).transpose()?;
    let d = cstr(dst)?;
    let t = fstype.map(|t| CString::new(t).unwrap());
    let o = data.map(|o| CString::new(o).unwrap());
    let r = unsafe {
        libc::mount(
            s.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            d.as_ptr(),
            t.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
            flags,
            o.as_ref().map(|c| c.as_ptr() as *const libc::c_void).unwrap_or(std::ptr::null()),
        )
    };
    check(r, &format!("mount {} on {}", src.map(|p| p.display().to_string()).unwrap_or_else(|| fstype.unwrap_or("-").into()), dst.display()))
}

/// Bind `src` on `dst` and make it read-only (and nosuid, nodev), submounts included where the kernel can.
fn bind_ro(src: &Path, dst: &Path) -> io::Result<()> {
    mount(Some(src), dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    mount(None, dst, None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None)?;
    // submounts too (mount_setattr, Linux 5.12+); the plain remount above covers the top mount on older kernels
    #[repr(C)]
    struct MountAttr {
        set: u64,
        clr: u64,
        prop: u64,
        userns: u64,
    }
    const AT_RECURSIVE: libc::c_uint = 0x8000;
    const MOUNT_ATTR_RDONLY: u64 = 0x1;
    const MOUNT_ATTR_NOSUID: u64 = 0x2;
    const MOUNT_ATTR_NODEV: u64 = 0x4;
    let a = MountAttr { set: MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV, clr: 0, prop: 0, userns: 0 };
    let d = cstr(dst)?;
    unsafe { libc::syscall(libc::SYS_mount_setattr, libc::AT_FDCWD, d.as_ptr(), AT_RECURSIVE, &a as *const MountAttr, std::mem::size_of::<MountAttr>()) };
    Ok(())
}

fn inside(root: &Path, p: &Path) -> PathBuf {
    root.join(p.strip_prefix("/").unwrap_or(p))
}

/// Make host path `p` appear at the same place under `root`: a symlink as the same symlink, a directory or a file as a
/// read-only bind. A missing path is skipped.
fn replicate(root: &Path, p: &Path) -> io::Result<()> {
    let Ok(md) = fs::symlink_metadata(p) else { return Ok(()) };
    let dst = inside(root, p);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if md.file_type().is_symlink() {
        if fs::symlink_metadata(&dst).is_err() {
            std::os::unix::fs::symlink(fs::read_link(p)?, &dst)?;
        }
        return Ok(());
    }
    if md.is_dir() {
        fs::create_dir_all(&dst)?;
    } else {
        fs::File::create(&dst)?;
    }
    bind_ro(p, &dst)
}

/// The new root: an allow list read-only, a private /tmp, a fresh /proc, five device nodes, the verifier and the runner.
fn build_root(spec: &Spec) -> io::Result<()> {
    let root = &spec.root;
    // nothing done here propagates back to the host's mounts
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    mount(Some(Path::new("tmpfs")), root, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("size=16m,nr_inodes=4096,mode=0755"))?;
    for p in &spec.ro_paths {
        replicate(root, p)?;
    }
    // the verifier (read-only bind of the store file), the runner and its spec
    let vdir = inside(root, Path::new("/verifier"));
    fs::create_dir_all(&vdir)?;
    fs::write(vdir.join("runner.py"), RUNNER)?;
    fs::write(vdir.join("spec.json"), serde_json::to_vec(&runner_spec(spec, IN_VERIFIER, IN_WORKDIR)).unwrap())?;
    let vfile = inside(root, Path::new(IN_VERIFIER));
    fs::File::create(&vfile)?;
    bind_ro(&spec.verifier, &vfile)?;
    // devices
    let dev = inside(root, Path::new("/dev"));
    fs::create_dir_all(&dev)?;
    for n in ["null", "zero", "full", "random", "urandom"] {
        let (src, dst) = (Path::new("/dev").join(n), dev.join(n));
        fs::File::create(&dst)?;
        mount(Some(&src), &dst, None, libc::MS_BIND, None)?;
        mount(None, &dst, None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_NOSUID | libc::MS_NOEXEC, None)?;
    }
    // a /proc of this pid namespace only
    let proc_ = inside(root, Path::new("/proc"));
    fs::create_dir_all(&proc_)?;
    mount(Some(Path::new("proc")), &proc_, Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)?;
    // the only writable place
    let tmp = inside(root, Path::new("/tmp"));
    fs::create_dir_all(&tmp)?;
    mount(
        Some(Path::new("tmpfs")),
        &tmp,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&format!("size={}m,nr_inodes=65536,mode=1777", spec.budget.tmp_mib)),
    )?;
    // pivot, drop the old root, make the new one read-only
    let old = root.join(".oldroot");
    fs::create_dir_all(&old)?;
    let (r, o) = (cstr(root)?, cstr(&old)?);
    if unsafe { libc::syscall(libc::SYS_pivot_root, r.as_ptr(), o.as_ptr()) } != 0 {
        return Err(last_err("pivot_root"));
    }
    std::env::set_current_dir("/")?;
    let o = CString::new("/.oldroot").unwrap();
    check(unsafe { libc::umount2(o.as_ptr(), libc::MNT_DETACH) }, "umount the old root")?;
    fs::remove_dir("/.oldroot")?;
    mount(None, Path::new("/"), None, libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, None)?;
    Ok(())
}

fn set_rlimit(res: libc::__rlimit_resource_t, soft: u64, hard: u64) -> io::Result<()> {
    let r = libc::rlimit { rlim_cur: soft as libc::rlim_t, rlim_max: hard as libc::rlim_t };
    check(unsafe { libc::setrlimit(res, &r) }, "setrlimit")
}

// ---- seccomp
/// AUDIT_ARCH_X86_64 / AUDIT_ARCH_AARCH64 (linux/audit.h).
#[cfg(target_arch = "x86_64")]
const ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const ARCH: u32 = 0xc000_00b7;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
/// Namespace flags `clone` must not carry.
const CLONE_NS_MASK: u32 =
    (libc::CLONE_NEWNS | libc::CLONE_NEWCGROUP | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::CLONE_NEWUSER | libc::CLONE_NEWPID | libc::CLONE_NEWNET)
        as u32;

/// The syscalls a checker gets EPERM for (the usual escalation and escape surfaces).
pub fn denied_syscalls() -> Vec<libc::c_long> {
    #[allow(unused_mut)]
    let mut v = vec![
        libc::SYS_ptrace,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_userfaultfd,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_mount_setattr,
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
        libc::SYS_syslog,
        libc::SYS_fanotify_init,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        libc::SYS_pidfd_getfd,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([libc::SYS_iopl, libc::SYS_ioperm, libc::SYS_uselib, libc::SYS_modify_ldt]);
    v
}

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}
fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// The seccomp program: wrong architecture (or x32) kills; the denied list answers EPERM; `socket` only for AF_UNIX
/// (else EAFNOSUPPORT); `clone` with a namespace flag EPERM; `clone3` ENOSYS (libc falls back to clone); else allow.
pub fn seccomp_program() -> Vec<libc::sock_filter> {
    const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
    const JSET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
    const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    let (off_nr, off_arch, off_arg0) = (0u32, 4u32, 16u32);
    let mut p = vec![
        stmt(LD_W_ABS, off_arch),
        jump(JEQ, ARCH, 1, 0),
        stmt(RET, RET_KILL_PROCESS),
        stmt(LD_W_ABS, off_nr),
        // x32 system calls (bit 30 set) on x86_64: never
        jump(JGE, 0x4000_0000, 0, 1),
        stmt(RET, RET_KILL_PROCESS),
    ];
    for nr in denied_syscalls() {
        p.push(jump(JEQ, nr as u32, 0, 1));
        p.push(stmt(RET, RET_ERRNO | libc::EPERM as u32));
    }
    p.push(jump(JEQ, libc::SYS_clone3 as u32, 0, 1));
    p.push(stmt(RET, RET_ERRNO | libc::ENOSYS as u32));
    // socket(domain, ...): AF_UNIX only
    p.push(jump(JEQ, libc::SYS_socket as u32, 0, 4));
    p.push(stmt(LD_W_ABS, off_arg0));
    p.push(jump(JEQ, libc::AF_UNIX as u32, 0, 1));
    p.push(stmt(RET, RET_ALLOW));
    p.push(stmt(RET, RET_ERRNO | libc::EAFNOSUPPORT as u32));
    // clone(flags, ...): no namespace flags (the accumulator still holds nr when socket did not match)
    p.push(jump(JEQ, libc::SYS_clone as u32, 0, 4));
    p.push(stmt(LD_W_ABS, off_arg0));
    p.push(jump(JSET, CLONE_NS_MASK, 1, 0));
    p.push(stmt(RET, RET_ALLOW));
    p.push(stmt(RET, RET_ERRNO | libc::EPERM as u32));
    p.push(stmt(RET, RET_ALLOW));
    p
}

fn install_seccomp() -> io::Result<()> {
    let prog = seccomp_program();
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut libc::sock_filter };
    check(unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog as *const libc::sock_fprog) }, "seccomp")
}

/// The confined child: everything after the pid namespace's first fork, ending in exec.
fn confine_and_exec(spec: &Spec) -> io::Result<()> {
    build_root(spec)?;
    let host = CString::new("drums-verify").unwrap();
    unsafe { libc::sethostname(host.as_ptr(), 12) };
    let b = &spec.budget;
    set_rlimit(libc::RLIMIT_CPU, b.cpu_s, b.cpu_s + 1)?;
    set_rlimit(libc::RLIMIT_FSIZE, b.tmp_mib << 20, b.tmp_mib << 20)?;
    set_rlimit(libc::RLIMIT_NOFILE, 256, 256)?;
    set_rlimit(libc::RLIMIT_CORE, 0, 0)?;
    // the unprivileged identity: no groups, then gid, then uid; verified
    check(unsafe { libc::setgroups(0, std::ptr::null()) }, "setgroups")?;
    check(unsafe { libc::setresgid(spec.gid, spec.gid, spec.gid) }, "setresgid")?;
    check(unsafe { libc::setresuid(spec.uid, spec.uid, spec.uid) }, "setresuid")?;
    if unsafe { libc::getuid() == 0 || libc::geteuid() == 0 || libc::setuid(0) == 0 } {
        return Err(io::Error::other("still root after dropping privileges"));
    }
    check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }, "no_new_privs")?;
    install_seccomp()?;
    // nothing inherited but the protocol pipes and stderr
    unsafe {
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) != 0 {
            for fd in 3..1024 {
                libc::close(fd);
            }
        }
    }
    let py = cstr(&spec.python)?;
    let args: Vec<CString> = ["python", "-I", "-B", IN_RUNNER, IN_SPEC].iter().map(|a| CString::new(*a).unwrap()).collect();
    let env: Vec<CString> =
        ["PATH=/usr/bin:/bin", "HOME=/tmp", "PYTHONDONTWRITEBYTECODE=1", "LANG=C.UTF-8", "TMPDIR=/tmp"].iter().map(|a| CString::new(*a).unwrap()).collect();
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|a| a.as_ptr()).collect();
    envp.push(std::ptr::null());
    unsafe { libc::execve(py.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    Err(last_err(&format!("exec {}", spec.python.display())))
}

fn emit_error(msg: &str) {
    let line =
        serde_json::to_vec(&json!({"op": "verdict", "score": null, "info": null, "error": format!("verifier sandbox: {msg}"), "error_class": "sandbox"}))
            .unwrap();
    unsafe {
        libc::write(1, line.as_ptr() as *const libc::c_void, line.len());
        libc::write(1, b"\n".as_ptr() as *const libc::c_void, 1);
    }
}

/// `drumsd verifier-sandbox`: read the spec line from stdin (byte by byte, so nothing after it is consumed), confine,
/// exec the runner. Runs as root in a fresh process with one thread. Returns only on failure, or as the pid
/// namespace's parent with the confined child's status (128 + signal when it was killed).
pub fn sandbox_main() -> i32 {
    let mut line = vec![];
    let mut byte = [0u8; 1];
    loop {
        match unsafe { libc::read(0, byte.as_mut_ptr() as *mut libc::c_void, 1) } {
            1 if byte[0] == b'\n' => break,
            1 => line.push(byte[0]),
            _ => break,
        }
        if line.len() > 16 << 20 {
            emit_error("spec line too long");
            return 70;
        }
    }
    let spec: Spec = match serde_json::from_slice(&line) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&format!("bad spec: {e}"));
            return 70;
        }
    };
    if spec.uid == 0 || spec.gid == 0 {
        emit_error("refusing to run a checker as root");
        return 70;
    }
    let pre = (|| -> io::Result<()> {
        fs::write(spec.cgroup.join("cgroup.procs"), "0")?;
        let flags = libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS | libc::CLONE_NEWPID | libc::CLONE_NEWCGROUP;
        check(unsafe { libc::unshare(flags) }, "unshare")
    })();
    if let Err(e) = pre {
        emit_error(&e.to_string());
        return 70;
    }
    match unsafe { libc::fork() } {
        -1 => {
            emit_error(&last_err("fork").to_string());
            70
        }
        0 => {
            // pid 1 of the new pid namespace
            if let Err(e) = confine_and_exec(&spec) {
                emit_error(&e.to_string());
            }
            unsafe { libc::_exit(70) }
        }
        child => {
            // keep no pipe end the checker could be waiting on, then wait for it
            unsafe {
                libc::close(0);
                libc::close(1);
            }
            let mut st = 0;
            loop {
                let r = unsafe { libc::waitpid(child, &mut st, 0) };
                if r == child {
                    break;
                }
                if r == -1 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    return 70;
                }
            }
            if libc::WIFEXITED(st) {
                libc::WEXITSTATUS(st)
            } else if libc::WIFSIGNALED(st) {
                128 + libc::WTERMSIG(st)
            } else {
                70
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A desktop made of a map of files; `run` echoes its command.
    pub struct FakeDesktop {
        pub files: BTreeMap<String, Vec<u8>>,
        pub runs: Mutex<Vec<Value>>,
    }
    impl Desktop for FakeDesktop {
        fn read_file(&self, path: &str, max: u64) -> io::Result<Option<Vec<u8>>> {
            match self.files.get(path) {
                Some(b) if b.len() as u64 > max => Err(io::Error::other("larger than the read budget")),
                Some(b) => Ok(Some(b.clone())),
                None => Ok(None),
            }
        }
        fn run(&self, command: &Value, _shell: bool, _t: Duration) -> io::Result<Value> {
            self.runs.lock().unwrap().push(command.clone());
            Ok(json!({"returncode": 0, "output": format!("ran {command}"), "error": ""}))
        }
    }

    fn python() -> Option<PathBuf> {
        ["/usr/bin/python3", "/usr/local/bin/python3", "/opt/homebrew/bin/python3"].iter().map(PathBuf::from).find(|p| p.exists())
    }

    fn spec(dir: &Path, src: &str, function: &str, options: Value) -> Spec {
        let v = dir.join("v.py");
        fs::write(&v, src).unwrap();
        Spec {
            python: python().unwrap(),
            ro_paths: vec![],
            verifier: v,
            sha256: crate::vsandbox::tests::sha(src),
            function: function.into(),
            options,
            task: json!({"id": "t"}),
            uid: 65534,
            gid: 65534,
            cgroup: PathBuf::new(),
            root: PathBuf::new(),
            budget: Budget::default(),
            wall_s: 20.0,
        }
    }
    pub fn sha(s: &str) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(s.as_bytes()))
    }
    fn desk() -> FakeDesktop {
        let mut files = BTreeMap::new();
        files.insert("/home/user/a.txt".to_string(), b"hello".to_vec());
        FakeDesktop { files, runs: Mutex::new(vec![]) }
    }

    #[test]
    fn the_broker_serves_read_file_and_run_and_returns_the_hash() {
        let Some(_) = python() else { return };
        let d = tempfile::tempdir().unwrap();
        let src = "def check(ctx, want):\n    print('noise on stdout')\n    b = ctx.read_file('/home/user/a.txt')\n    r = ctx.run(['ls', '/'])\n    return {'score': float(b == want.encode() and ctx.read_file('/nope') is None), 'info': {'run': r['output'], 'task': ctx.task['id']}}\n";
        let s = spec(d.path(), src, "check", json!({"want": "hello"}));
        let fake = desk();
        let v = run(&Confine::Unconfined, &s, &fake);
        assert_eq!(v["score"], 1.0, "{v}");
        assert_eq!(v["error_class"], "none");
        assert_eq!(v["sha256"], sha(src));
        assert_eq!(v["info"]["task"], "t");
        assert_eq!(v["requests"], 3);
        assert_eq!(v["read_bytes"], 5);
        assert!(v["log"].as_str().unwrap().contains("noise on stdout"), "a print goes to the log, not the protocol: {v}");
        assert_eq!(fake.runs.lock().unwrap().len(), 1);
    }

    #[test]
    fn checker_errors_are_classed_never_scored() {
        let Some(_) = python() else { return };
        let d = tempfile::tempdir().unwrap();
        for (src, class) in [
            ("def check(ctx):\n    raise ValueError('boom')\n", "exception"),
            ("def check(ctx):\n    return 'yes'\n", "bad_return"),
            ("def check(ctx):\n    return float('nan')\n", "bad_return"),
            ("import sys\ndef check(ctx):\n    sys.exit(3)\n", "exception"),
            ("def other(ctx):\n    return 1\n", "exception"),
            ("import os\ndef check(ctx):\n    os._exit(0)\n", "protocol"),
            ("import __main__\ndef check(ctx):\n    __main__.PROTO_OUT.write(b'not json\\n')\n    return 1\n", "protocol"),
        ] {
            let v = run(&Confine::Unconfined, &spec(d.path(), src, "check", json!({})), &desk());
            assert_eq!(v["error_class"], class, "{src}: {v}");
            assert!(v["score"].is_null(), "{src}: {v}");
        }
    }

    #[test]
    fn the_wall_request_and_read_budgets_hold() {
        let Some(_) = python() else { return };
        let d = tempfile::tempdir().unwrap();
        let mut s = spec(d.path(), "def check(ctx):\n    while True:\n        pass\n", "check", json!({}));
        s.wall_s = 1.0;
        let t0 = Instant::now();
        let v = run(&Confine::Unconfined, &s, &desk());
        assert_eq!(v["error_class"], "timeout", "{v}");
        assert!(t0.elapsed() < Duration::from_secs(5));
        let mut s = spec(d.path(), "def check(ctx):\n    n = 0\n    while True:\n        try:\n            ctx.read_file('/home/user/a.txt')\n        except RuntimeError as e:\n            return {'score': 0, 'info': {'n': n, 'e': str(e)}}\n        n += 1\n", "check", json!({}));
        s.budget.requests = 5;
        let v = run(&Confine::Unconfined, &s, &desk());
        assert_eq!(v["info"]["n"], 5, "{v}");
        assert!(v["info"]["e"].as_str().unwrap().contains("5 requests"));
        let mut s = spec(d.path(), "def check(ctx):\n    try:\n        ctx.read_file('/home/user/a.txt')\n    except RuntimeError as e:\n        return {'score': 0, 'info': str(e)}\n    return 1\n", "check", json!({}));
        s.budget.read_bytes = 3;
        let v = run(&Confine::Unconfined, &s, &desk());
        assert!(v["info"].as_str().unwrap().contains("read budget"), "{v}");
        let mut s = spec(d.path(), "def check(ctx):\n    return {'score': 1, 'info': 'x' * 5000}\n", "check", json!({}));
        s.budget.verdict_bytes = 1000;
        assert_eq!(run(&Confine::Unconfined, &s, &desk())["error_class"], "protocol");
    }

    #[test]
    fn a_root_spec_is_refused_before_anything_runs() {
        let d = tempfile::tempdir().unwrap();
        if python().is_none() {
            return;
        }
        let mut s = spec(d.path(), "def check(ctx):\n    return 1\n", "check", json!({}));
        s.uid = 0;
        let c = Confine::Sandbox { exe: "/nonexistent".into(), cgroup_parent: d.path().join("cg"), run_dir: d.path().join("run") };
        let v = run(&c, &s, &desk());
        assert_eq!(v["error_class"], "sandbox");
        assert!(v["error"].as_str().unwrap().contains("never runs a checker as root"));
        assert!(!d.path().join("cg").exists());
    }

    #[test]
    fn verdicts_are_checked() {
        assert_eq!(check_verdict(json!({"score": 1, "info": {"a": 1}, "error_class": "none"}))["score"], 1.0);
        assert_eq!(check_verdict(json!({"score": null, "error_class": "none"}))["error_class"], "bad_return");
        assert_eq!(check_verdict(json!({"score": 1, "error_class": "made_up"}))["error_class"], "protocol");
        assert_eq!(check_verdict(json!({"error": "x", "error_class": "exception"}))["error"], "x");
    }

    #[test]
    fn the_wall_budget_follows_the_task_within_the_cap() {
        let b = Budget::default();
        assert_eq!(b.wall_for(None), 90.0);
        assert_eq!(b.wall_for(Some(30.0)), 30.0);
        assert_eq!(b.wall_for(Some(10_000.0)), 600.0);
        assert_eq!(b.wall_for(Some(f64::NAN)), 90.0);
        assert_eq!(b.wall_for(Some(-1.0)), 90.0);
    }

    #[test]
    fn the_seccomp_program_is_well_formed() {
        let p = seccomp_program();
        assert!(p.len() < 255, "a sock_fprog length is a u16, and short programs are cheap");
        // every jump lands inside the program, and the last instruction returns
        for (i, f) in p.iter().enumerate() {
            if f.code & 0x07 == libc::BPF_JMP as u16 {
                assert!(i + 1 + (f.jt.max(f.jf) as usize) < p.len(), "jump at {i} leaves the program");
            }
        }
        assert_eq!(p.last().unwrap().code, (libc::BPF_RET | libc::BPF_K) as u16);
        assert_eq!(p.last().unwrap().k, RET_ALLOW);
        let denied = denied_syscalls();
        for nr in [libc::SYS_ptrace, libc::SYS_mount, libc::SYS_unshare, libc::SYS_setns, libc::SYS_bpf, libc::SYS_io_uring_setup] {
            assert!(denied.contains(&nr));
        }
        for nr in [libc::SYS_read, libc::SYS_write, libc::SYS_execve, libc::SYS_clone, libc::SYS_socket] {
            assert!(!denied.contains(&nr), "{nr} is handled, not denied outright");
        }
        assert_eq!(CLONE_NS_MASK & libc::CLONE_THREAD as u32, 0, "threads stay allowed");
    }

    /// A guest server that answers `n` requests: /file with the body for `file_path=/a` (404 otherwise), /execute with
    /// a fixed reply.
    fn fake_guest(n: usize) -> u16 {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..n {
                let (mut c, _) = l.accept().unwrap();
                let mut got = vec![];
                let mut buf = [0u8; 4096];
                loop {
                    let k = c.read(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..k]);
                    if let Some(he) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&got[..he]).to_ascii_lowercase();
                        let cl = head.lines().find_map(|l| l.strip_prefix("content-length:")).map(|v| v.trim().parse::<usize>().unwrap()).unwrap_or(0);
                        if got.len() >= he + 4 + cl {
                            break;
                        }
                    }
                    if k == 0 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&got).to_string();
                let reply: &[u8] = if req.starts_with("POST /file") && req.ends_with("file_path=/a") {
                    b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\nabc"
                } else if req.starts_with("POST /file") {
                    b"HTTP/1.0 404 NOT FOUND\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.0 200 OK\r\n\r\n{\"status\": \"error\", \"returncode\": 2, \"output\": \"o\", \"error\": \"e\"}"
                };
                c.write_all(reply).unwrap();
            }
        });
        port
    }

    #[test]
    fn the_guest_desktop_reads_files_and_runs_commands() {
        let g = GuestDesktop { port: fake_guest(4) };
        assert_eq!(g.read_file("/a", 100).unwrap(), Some(b"abc".to_vec()));
        assert_eq!(g.read_file("/b", 100).unwrap(), None);
        assert!(g.read_file("/a", 2).unwrap_err().to_string().contains("read budget"));
        let r = g.run(&json!(["false"]), false, Duration::from_secs(5)).unwrap();
        assert_eq!(r, json!({"returncode": 2, "output": "o", "error": "e"}), "a non-zero exit is an answer, not an error");
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(b64(i.as_bytes()), o);
        }
    }
}
