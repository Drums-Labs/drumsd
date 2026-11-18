//! The verifier sandbox against checkers that try to escape (docs/design/verifier-sandbox.md). Each test runs a real
//! `drumsd verifier-sandbox` (namespaces, pivot_root, cgroup budgets, seccomp, an unprivileged uid). They need root and
//! cgroup v2 and skip themselves otherwise; CI runs them under sudo (the `verifier sandbox (root)` job).

use drumsd_linux::vsandbox::{self, Budget, Confine, Desktop, Spec};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct FakeDesktop {
    files: BTreeMap<String, Vec<u8>>,
    runs: Mutex<Vec<Value>>,
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
        Ok(json!({"returncode": 0, "output": "desktop says hi", "error": ""}))
    }
}

fn desktop() -> FakeDesktop {
    let mut files = BTreeMap::new();
    files.insert("/home/user/report.txt".to_string(), b"total 42".to_vec());
    FakeDesktop { files, runs: Mutex::new(vec![]) }
}

fn python() -> Option<PathBuf> {
    ["/usr/bin/python3", "/usr/local/bin/python3"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// None (and a note) when this host cannot run the sandbox: not root, no cgroup v2, no python.
fn usable() -> Option<PathBuf> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skip: the verifier sandbox tests need root");
        return None;
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        eprintln!("skip: no cgroup v2");
        return None;
    }
    python().or_else(|| {
        eprintln!("skip: no python3");
        None
    })
}

struct Host {
    _dir: tempfile::TempDir,
    store: PathBuf,
    cg_parent: PathBuf,
    run_dir: PathBuf,
    python: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.cg_parent);
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

fn host(name: &str) -> Option<Host> {
    let python = usable()?;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let tag = format!("drums-vtest-{}-{name}", std::process::id());
    Some(Host { store, cg_parent: Path::new("/sys/fs/cgroup").join(&tag), run_dir: Path::new("/run").join(&tag), python, _dir: dir })
}

fn check(h: &Host, src: &str, budget: Budget, wall_s: f64) -> Value {
    let sha = drumsd_core::verifiers::sha256_hex(src.as_bytes());
    let file = h.store.join(format!("{sha}.py"));
    std::fs::write(&file, src).unwrap();
    let mut ro: Vec<PathBuf> = vsandbox::DEFAULT_RO_PATHS.iter().map(PathBuf::from).collect();
    ro.push(h.python.clone());
    let spec = Spec {
        python: h.python.clone(),
        ro_paths: ro,
        verifier: file,
        sha256: sha.clone(),
        function: "check".into(),
        options: json!({}),
        task: json!({"id": "escape-test"}),
        uid: 65534,
        gid: 65534,
        cgroup: PathBuf::new(),
        root: PathBuf::new(),
        budget,
        wall_s,
    };
    let c = Confine::Sandbox { exe: PathBuf::from(env!("CARGO_BIN_EXE_drumsd")), cgroup_parent: h.cg_parent.clone(), run_dir: h.run_dir.clone() };
    let v = vsandbox::run(&c, &spec, &desktop());
    eprintln!("verdict: {v}");
    assert_eq!(v["sha256"], sha, "every verdict carries the verifier's hash");
    // nothing is left behind: the call's cgroup and its root directory are gone
    let left: Vec<_> = std::fs::read_dir(&h.cg_parent).map(|d| d.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name()).collect()).unwrap_or_default();
    assert!(left.is_empty(), "cgroups left behind: {left:?}");
    let left: Vec<_> = std::fs::read_dir(&h.run_dir).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
    assert!(left.is_empty(), "roots left behind: {left:?}");
    v
}

fn quick() -> Budget {
    Budget { wall_s: 20.0, ..Budget::default() }
}

#[test]
fn a_well_behaved_checker_scores_through_the_broker() {
    let Some(h) = host("ok") else { return };
    let src = r#"
import json, os, socket
def check(ctx):
    data = ctx.read_file("/home/user/report.txt")
    r = ctx.run(["cat", "/etc/hostname"])
    open(os.path.join(ctx.workdir, "scratch"), "w").write("x")       # the workdir is writable
    a, b = socket.socketpair()                                         # AF_UNIX is allowed
    status = dict(l.split(":\t", 1) for l in open("/proc/self/status").read().splitlines() if ":\t" in l)
    return {"score": float(data == b"total 42" and ctx.read_file("/nope") is None),
            "info": {"uid": os.getuid(), "gid": os.getgid(), "groups": os.getgroups(), "run": r["output"],
                     "nnp": status.get("NoNewPrivs"), "seccomp": status.get("Seccomp"), "cap_eff": status.get("CapEff"),
                     "pid": os.getpid(), "hostname": socket.gethostname(), "root": sorted(os.listdir("/"))}}
"#;
    let v = check(&h, src, quick(), 20.0);
    assert_eq!(v["error_class"], "none", "{v}");
    assert_eq!(v["score"], 1.0);
    let i = &v["info"];
    assert_eq!((i["uid"].as_u64(), i["gid"].as_u64()), (Some(65534), Some(65534)), "never root");
    assert_eq!(i["groups"], json!([]), "no supplementary groups");
    assert_eq!(i["nnp"].as_str().map(str::trim), Some("1"));
    assert_eq!(i["seccomp"].as_str().map(str::trim), Some("2"), "a seccomp filter is installed");
    assert_eq!(i["cap_eff"].as_str().map(str::trim), Some("0000000000000000"));
    assert_eq!(i["pid"], 1, "its own pid namespace");
    assert_eq!(i["hostname"], "drums-verify");
    assert_eq!(i["run"], "desktop says hi");
    let root: Vec<&str> = i["root"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    for hidden in ["home", "root", "var", "run", "srv", "mnt", "opt", "boot"] {
        assert!(!root.contains(&hidden), "/{hidden} is visible: {root:?}");
    }
    assert!(v["memory_peak_mib"].as_f64().unwrap() > 0.0, "the cgroup measured it");
}

#[test]
fn no_network() {
    let Some(h) = host("net") else { return };
    let src = r#"
import socket
def check(ctx):
    tried = {}
    for name, fam, typ in [("inet_tcp", socket.AF_INET, socket.SOCK_STREAM), ("inet_udp", socket.AF_INET, socket.SOCK_DGRAM),
                           ("inet6", socket.AF_INET6, socket.SOCK_STREAM), ("netlink", socket.AF_NETLINK, socket.SOCK_RAW),
                           ("packet", getattr(socket, "AF_PACKET", 17), socket.SOCK_RAW)]:
        try:
            s = socket.socket(fam, typ)
            try:
                s.settimeout(2)
                s.connect(("169.254.169.254", 80) if fam == socket.AF_INET else ("::1", 80, 0, 0))
                tried[name] = "connected"
            except OSError as e:
                tried[name] = "created, connect failed: %s" % e.errno
        except OSError as e:
            tried[name] = "refused: %s" % e.errno
    try:
        socket.create_connection(("1.1.1.1", 53), timeout=2)
        tried["dns"] = "connected"
    except OSError as e:
        tried["dns"] = "refused: %s" % e.errno
    return {"score": float(any(v == "connected" for v in tried.values())), "info": tried}
"#;
    let v = check(&h, src, quick(), 20.0);
    assert_eq!(v["error_class"], "none", "{v}");
    assert_eq!(v["score"], 0.0, "a network connection was made: {v}");
    for k in ["inet_tcp", "inet_udp", "inet6", "netlink", "packet"] {
        assert!(v["info"][k].as_str().unwrap().starts_with("refused: 97"), "{k} sockets are refused (EAFNOSUPPORT): {v}");
    }
}

#[test]
fn no_write_outside_its_tmp() {
    let Some(h) = host("write") else { return };
    let src = r#"
import os
def check(ctx):
    wrote = []
    for p in ["/usr/escape", "/etc/escape", "/escape", "/verifier/verifier.py", "/verifier/escape", "/dev/escape",
              "/proc/sys/kernel/hostname", "/lib/escape"]:
        try:
            with open(p, "a") as f:
                f.write("x")
            wrote.append(p)
        except OSError:
            pass
    try:
        os.mkdir("/home")
        wrote.append("/home")
    except OSError:
        pass
    return {"score": float(bool(wrote)), "info": wrote}
"#;
    let v = check(&h, src, quick(), 20.0);
    assert_eq!(v["error_class"], "none", "{v}");
    assert_eq!(v["score"], 0.0, "wrote outside /tmp: {v}");
    assert!(!Path::new("/usr/escape").exists() && !Path::new("/etc/escape").exists() && !Path::new("/escape").exists());
}

#[test]
fn no_read_of_the_host_token_or_host_files() {
    let Some(h) = host("token") else { return };
    // a stand-in for /etc/drums/token, in a place a careless allow list could expose
    let secret_dir = Path::new("/etc/drums-vtest-secret");
    std::fs::create_dir_all(secret_dir).unwrap();
    let secret = format!("drums-vtest-secret-{}", std::process::id());
    std::fs::write(secret_dir.join("token"), &secret).unwrap();
    let src = format!(
        r#"
import os
SECRET = {secret:?}
def check(ctx):
    found = []
    for p in ["/etc/drums-vtest-secret/token", "/etc/drums/token", "/etc/shadow", "/etc/passwd", "/root/.ssh/id_rsa",
              "/proc/1/root/etc/drums-vtest-secret/token", "/proc/self/root/etc/drums-vtest-secret/token",
              "/sys/fs/cgroup/cgroup.procs", "/run/drums-osw/osw.sock", "/var/lib/drums/verifiers"]:
        try:
            found.append((p, open(p).read()[:40]))
        except OSError:
            pass
    # a walk of everything it can see, looking for the secret's bytes
    hits = []
    for d, dirs, files in os.walk("/", onerror=lambda e: None):
        if d.startswith(("/proc", "/usr", "/lib", "/verifier")):   # /verifier holds this source, which names the secret
            dirs[:] = []
            continue
        for f in files:
            try:
                if SECRET.encode() in open(os.path.join(d, f), "rb").read(1 << 20):
                    hits.append(os.path.join(d, f))
            except OSError:
                pass
    return {{"score": float(bool(found or hits)), "info": {{"found": found, "hits": hits, "etc": sorted(os.listdir("/etc"))}}}}
"#
    );
    let v = check(&h, &src, quick(), 30.0);
    let _ = std::fs::remove_dir_all(secret_dir);
    assert_eq!(v["error_class"], "none", "{v}");
    assert_eq!(v["score"], 0.0, "read a host file: {v}");
    let etc: Vec<&str> = v["info"]["etc"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(etc.iter().all(|e| ["ld.so.cache", "localtime", "alternatives"].contains(e)), "only the allow list of /etc: {etc:?}");
}

#[test]
fn no_privilege_escalation_or_new_namespaces() {
    let Some(h) = host("priv") else { return };
    let src = r#"
import ctypes, os
def check(ctx):
    libc = ctypes.CDLL(None, use_errno=True)
    got = {}
    def t(name, f):
        try:
            r = f()
            got[name] = "ok %r" % (r,) if r in (0, None) else "failed %d" % ctypes.get_errno()
        except Exception as e:
            got[name] = "failed %s" % type(e).__name__
    t("setuid0", lambda: os.setuid(0))
    t("unshare_user", lambda: libc.unshare(0x10000000))
    t("unshare_net", lambda: libc.unshare(0x40000000))
    t("mount_tmpfs", lambda: libc.mount(b"none", b"/tmp", b"tmpfs", 0, None))
    t("chroot", lambda: libc.chroot(b"/tmp"))
    t("ptrace_traceme", lambda: libc.ptrace(0, 0, None, None))
    t("bpf", lambda: libc.syscall(321 if os.uname().machine == "x86_64" else 280, 0, None, 0))
    t("sethostname", lambda: libc.sethostname(b"x", 1))
    return {"score": float(any(v.startswith("ok") for v in got.values())), "info": got}
"#;
    let v = check(&h, src, quick(), 20.0);
    assert_eq!(v["error_class"], "none", "{v}");
    assert_eq!(v["score"], 0.0, "an escalation worked: {v}");
}

#[test]
fn a_fork_bomb_hits_the_pid_budget_and_is_killed_at_the_wall() {
    let Some(h) = host("fork") else { return };
    let src = r#"
import os
def check(ctx):
    while True:
        try:
            os.fork()
        except OSError:
            pass
"#;
    let b = Budget { pids: 32, ..Budget::default() };
    let t0 = Instant::now();
    let v = check(&h, src, b, 4.0);
    assert_eq!(v["error_class"], "timeout", "{v}");
    assert!(v["score"].is_null());
    assert!(t0.elapsed() < Duration::from_secs(15), "stopped promptly: {:?}", t0.elapsed());
    if let Some(p) = v["pids_peak"].as_u64() {
        assert!(p <= 32, "pids.max held: {v}");
    }
    // the host still forks
    assert!(std::process::Command::new("true").status().unwrap().success());
}

#[test]
fn a_checker_past_its_budgets_is_stopped() {
    let Some(h) = host("budget") else { return };
    // CPU
    let t0 = Instant::now();
    let v = check(&h, "def check(ctx):\n    while True:\n        pass\n", Budget { cpu_s: 2, ..Budget::default() }, 60.0);
    assert_eq!(v["error_class"], "cpu", "{v}");
    assert!(t0.elapsed() < Duration::from_secs(20));
    // wall (sleeping uses no CPU)
    let t0 = Instant::now();
    let v = check(&h, "import time\ndef check(ctx):\n    time.sleep(600)\n", Budget::default(), 2.0);
    assert_eq!(v["error_class"], "timeout", "{v}");
    assert!(t0.elapsed() < Duration::from_secs(10));
    // memory
    let v = check(
        &h,
        "def check(ctx):\n    x = []\n    while True:\n        x.append(bytearray(16 << 20))\n",
        Budget { memory_mib: 128, ..Budget::default() },
        30.0,
    );
    assert!(v["error_class"] == "oom" || (v["error_class"] == "exception" && v["error"].as_str().unwrap().contains("MemoryError")), "{v}");
    // disk: /tmp is small and RLIMIT_FSIZE caps one file
    let src = "import os\ndef check(ctx):\n    try:\n        with open('/tmp/fill', 'wb') as f:\n            for _ in range(64):\n                f.write(b'x' * (1 << 20))\n    except OSError as e:\n        return {'score': 0, 'info': e.errno}\n    return 1\n";
    let v = check(&h, src, Budget { tmp_mib: 8, ..Budget::default() }, 20.0);
    assert!(v["error_class"] == "none" && v["score"] == 0.0 || v["error_class"] == "protocol", "{v}");
}

#[test]
fn the_bundled_drums_t6_runs_in_the_sandbox() {
    let Some(h) = host("t6") else { return };
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../verifiers/drums_t6.py")).unwrap();
    // drums_t6's check(ctx, kind, **options) for a files_state task, through a thin wrapper (check takes options)
    let want = drumsd_core::verifiers::sha256_hex(b"total 42");
    let wrapper = format!(
        "{src}\n\ndef check_wrapped(ctx):\n    return check(ctx, 'files_state', must_exist={{'/home/user/report.txt': {want:?}}}, must_absent=['/home/user/gone.txt'])\n"
    );
    let v = check_fn(&h, &wrapper, "check_wrapped");
    assert_eq!((v["error_class"].as_str(), v["score"].as_f64()), (Some("none"), Some(1.0)), "{v}");
}

fn check_fn(h: &Host, src: &str, function: &str) -> Value {
    let sha = drumsd_core::verifiers::sha256_hex(src.as_bytes());
    let file = h.store.join(format!("{sha}.py"));
    std::fs::write(&file, src).unwrap();
    let mut ro: Vec<PathBuf> = vsandbox::DEFAULT_RO_PATHS.iter().map(PathBuf::from).collect();
    ro.push(h.python.clone());
    let spec = Spec {
        python: h.python.clone(),
        ro_paths: ro,
        verifier: file,
        sha256: sha,
        function: function.into(),
        options: json!({}),
        task: Value::Null,
        uid: 65534,
        gid: 65534,
        cgroup: PathBuf::new(),
        root: PathBuf::new(),
        budget: quick(),
        wall_s: 30.0,
    };
    let c = Confine::Sandbox { exe: PathBuf::from(env!("CARGO_BIN_EXE_drumsd")), cgroup_parent: h.cg_parent.clone(), run_dir: h.run_dir.clone() };
    vsandbox::run(&c, &spec, &desktop())
}
