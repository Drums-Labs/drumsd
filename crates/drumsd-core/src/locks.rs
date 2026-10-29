//! Port of `sharedos_rootfs/locks.py` (the lock-fidelity gate), same formats and the same comparisons.
//!
//! * `dumped`: CRIU's `LOCK flag:` lines in dump.log, recorded at snapshot time in `locks-dumped.json`
//!   as `[flag, type, pid, fd, start|null, len|null]` (FLOCK entries carry no range).
//! * `applied`: CRIU's `(posix)flag: ...` lines in the restore log.
//! * the pre-resume census: locks seen in `/proc/PID/fdinfo` of every restored task while the tasks are still stopped,
//!   compared to the dump as a set of `(flag, type, start, len)`.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;
use std::sync::OnceLock;

pub type Entry = [Option<i64>; 6];

fn lock_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^(?P<id>\d+):\s*(?P<blocked>->\s*)?(?P<flag>\S+)\s+(?P<adv>\S+)\s+(?P<type>\S+)\s+(?P<owner>-?\d+)\s+(?P<maj>[0-9a-fA-F]+):(?P<min>[0-9a-fA-F]+):(?P<ino>\d+)\s+(?P<start>\d+)\s+(?P<end>\S+)\s*$").unwrap()
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub id: i64,
    pub kind: String,
    pub advisory: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub blocked: bool,
    pub owner: i64,
    pub dev: String,
    pub ino: u64,
    pub start: i64,
    pub end: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldLock {
    #[serde(flatten)]
    pub lock: Lock,
    pub pid: i64,
    pub fd: i64,
    pub path: Option<String>,
    pub mnt_id: Option<i64>,
}

pub fn parse_lock(text: &str) -> Result<Lock, String> {
    let c = lock_re().captures(text.trim()).ok_or_else(|| format!("unparseable lock entry: {:?}", &text.trim()[..text.trim().len().min(120)]))?;
    let kind = match &c["flag"] {
        "POSIX" => "posix".to_string(),
        "FLOCK" => "flock".to_string(),
        "OFDLCK" => "ofd".to_string(),
        "LEASE" => "lease".to_string(),
        o => format!("unknown:{o}"),
    };
    let maj = i64::from_str_radix(&c["maj"], 16).map_err(|e| e.to_string())?;
    let min = i64::from_str_radix(&c["min"], 16).map_err(|e| e.to_string())?;
    Ok(Lock {
        id: c["id"].parse().unwrap(),
        kind,
        advisory: c["adv"].to_string(),
        typ: c["type"].to_string(),
        blocked: c.name("blocked").is_some(),
        owner: c["owner"].parse().unwrap(),
        dev: format!("{maj}:{min}"),
        ino: c["ino"].parse().unwrap(),
        start: c["start"].parse().unwrap(),
        end: c["end"].to_string(),
    })
}

/// Read-only access to /proc (injected so the census is testable without a kernel).
pub trait ProcFs {
    fn read(&self, path: &str) -> io::Result<String>;
    fn listdir(&self, path: &str) -> io::Result<Vec<String>>;
    fn readlink(&self, path: &str) -> io::Result<String>;
}

/// Every process in the same pid namespace as `root` (the whole restored tree, whatever its ppid shape).
pub fn pid_ns_members(p: &dyn ProcFs, root: i64) -> Vec<i64> {
    let Ok(mine) = p.readlink(&format!("/proc/{root}/ns/pid")) else { return vec![] };
    let mut out: Vec<i64> = p
        .listdir("/proc")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| n.parse::<i64>().ok())
        .filter(|n| p.readlink(&format!("/proc/{n}/ns/pid")).map(|x| x == mine).unwrap_or(false))
        .collect();
    out.sort();
    out
}

/// Descendants through /proc/PID/task/TID/children.
pub fn children_walk(p: &dyn ProcFs, root: i64) -> Vec<i64> {
    let mut seen: Vec<i64> = vec![];
    let mut todo = vec![root];
    while let Some(cur) = todo.pop() {
        let Ok(tids) = p.listdir(&format!("/proc/{cur}/task")) else { continue };
        for tid in tids {
            let Ok(kids) = p.read(&format!("/proc/{cur}/task/{tid}/children")) else { continue };
            for k in kids.split_whitespace().filter_map(|k| k.parse::<i64>().ok()) {
                if !seen.contains(&k) {
                    seen.push(k);
                    todo.push(k);
                }
            }
        }
    }
    seen.sort();
    seen
}

/// Locks on fds of the given tasks (`locks.py::census`, without the `mapped_by_holder` diagnostic).
pub fn census(p: &dyn ProcFs, tree: &[i64]) -> Result<Vec<HeldLock>, String> {
    let mut out = vec![];
    for pid in tree {
        let Ok(mut fds) = p.listdir(&format!("/proc/{pid}/fd")) else { continue };
        fds.sort_by_key(|f| f.parse::<i64>().unwrap_or(i64::MAX));
        for fd in fds {
            let Ok(info) = p.read(&format!("/proc/{pid}/fdinfo/{fd}")) else { continue };
            let mnt = info
                .lines()
                .find(|l| l.starts_with("mnt_id:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|x| x.parse::<i64>().ok());
            for line in info.lines() {
                if let Some(body) = line.strip_prefix("lock:") {
                    let body = body.trim();
                    if body.is_empty() {
                        continue;
                    }
                    let lock = parse_lock(body)?;
                    let path = p.readlink(&format!("/proc/{pid}/fd/{fd}")).ok();
                    out.push(HeldLock { lock, pid: *pid, fd: fd.parse().unwrap_or(-1), path, mnt_id: mnt });
                }
            }
        }
    }
    Ok(out)
}

fn kind_code(k: &str) -> i64 {
    match k {
        "posix" => 1,
        "flock" => 2,
        "ofd" => 4,
        "lease" => 8,
        _ => -1,
    }
}
fn type_code(t: &str) -> i64 {
    match t {
        "READ" => 0,
        "WRITE" => 1,
        _ => -1,
    }
}

fn census_len(l: &Lock) -> i64 {
    if l.end == "EOF" {
        0
    } else {
        l.end.parse::<i64>().unwrap_or(-1) + 1 - l.start
    }
}

pub type Shape = (i64, i64, Option<i64>, Option<i64>);

/// `census` over several threads (the work is per process: /proc/PID/fd and one fdinfo read per fd).
pub fn census_parallel(p: &(dyn ProcFs + Sync), tree: &[i64], threads: usize) -> Result<Vec<HeldLock>, String> {
    let threads = threads.max(1).min(tree.len().max(1));
    let chunk = tree.len().div_ceil(threads).max(1);
    let parts: Vec<Result<Vec<HeldLock>, String>> = std::thread::scope(|s| {
        let hs: Vec<_> = tree.chunks(chunk).map(|c| s.spawn(move || census(p, c))).collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("census thread panicked".into()))).collect()
    });
    let mut out = vec![];
    for r in parts {
        out.extend(r?);
    }
    out.sort_by_key(|h| (h.pid, h.fd, h.lock.id));
    Ok(out)
}

/// `shape_of_census`: the census as a set of (flag, type, start, len); FLOCK without a range.
pub fn shape_of_census(locks: &[HeldLock]) -> BTreeSet<Shape> {
    locks
        .iter()
        .map(|h| {
            let flock = h.lock.kind == "flock";
            (
                kind_code(&h.lock.kind),
                type_code(&h.lock.typ),
                if flock { None } else { Some(h.lock.start) },
                if flock { None } else { Some(census_len(&h.lock)) },
            )
        })
        .collect()
}

/// `census_tuples`: (flag, type, fd, start, len), sorted.
pub fn census_tuples(locks: &[HeldLock]) -> Vec<[Option<i64>; 5]> {
    let mut v: Vec<[Option<i64>; 5]> = locks
        .iter()
        .map(|h| {
            let f = kind_code(&h.lock.kind);
            [
                Some(f),
                Some(type_code(&h.lock.typ)),
                Some(h.fd),
                if f == 2 { None } else { Some(h.lock.start) },
                if f == 2 { None } else { Some(census_len(&h.lock)) },
            ]
        })
        .collect();
    v.sort();
    v
}

/// `shape_of_dump`: CRIU's dumped entries as a set of (flag, type, start, len).
pub fn shape_of_dump(entries: &[Entry]) -> BTreeSet<Shape> {
    entries.iter().map(|e| (e[0].unwrap_or(-1), e[1].unwrap_or(-1), e[4], e[5])).collect()
}

fn norm(flag: i64, typ: i64, pid: i64, fd: i64, start: Option<i64>, len: Option<i64>) -> Entry {
    let fl = flag == 2;
    [Some(flag), Some(typ), Some(pid), Some(fd), if fl { None } else { start }, if fl { None } else { len }]
}

fn dump_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"LOCK flag: (\d+),type: (\d+),pid: (-?\d+),fd: (-?\d+),start:\s*([0-9a-f]+),len:\s*([0-9a-f]+)").unwrap())
}
fn apply_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\((posix|flock|ofd|lease)\)flag: (\d+), type: (\d+)(?:, cmd: \d+)?, pid: (-?\d+), fd: (-?\d+)(?:, start:\s*(?:0x)?([0-9a-f]+), len:\s*(?:0x)?([0-9a-f]+))?").unwrap()
    })
}

fn hex(s: &str) -> i64 {
    i64::from_str_radix(s, 16).unwrap_or(-1)
}

/// What CRIU wrote to filelocks.img, from dump.log.
pub fn dumped_from_log(text: &str) -> Vec<Entry> {
    let mut v: Vec<Entry> = dump_re()
        .captures_iter(text)
        .map(|m| norm(m[1].parse().unwrap(), m[2].parse().unwrap(), m[3].parse().unwrap(), m[4].parse().unwrap(), Some(hex(&m[5])), Some(hex(&m[6]))))
        .collect();
    v.sort();
    v
}

/// What the restore applied, from the restore log.
pub fn applied_from_log(text: &str) -> Vec<Entry> {
    let mut v: Vec<Entry> = apply_re()
        .captures_iter(text)
        .map(|m| {
            norm(
                m[2].parse().unwrap(),
                m[3].parse().unwrap(),
                m[4].parse().unwrap(),
                m[5].parse().unwrap(),
                m.get(6).map(|x| hex(x.as_str())),
                m.get(7).map(|x| hex(x.as_str())),
            )
        })
        .collect();
    v.sort();
    v
}

/// Multiset equality of two entry lists (both sides sorted the same way).
pub fn same_entries(a: &[Entry], b: &[Entry]) -> bool {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    b.sort();
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake {
        files: HashMap<String, String>,
        dirs: HashMap<String, Vec<String>>,
        links: HashMap<String, String>,
    }
    impl ProcFs for Fake {
        fn read(&self, p: &str) -> io::Result<String> {
            self.files.get(p).cloned().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn listdir(&self, p: &str) -> io::Result<Vec<String>> {
            self.dirs.get(p).cloned().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn readlink(&self, p: &str) -> io::Result<String> {
            self.links.get(p).cloned().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    #[test]
    fn parses_fdinfo_and_proc_locks_lines() {
        let l = parse_lock("1: POSIX  ADVISORY  WRITE 1234 00:2e:5678 0 EOF").unwrap();
        assert_eq!((l.kind.as_str(), l.typ.as_str(), l.owner, l.dev.as_str(), l.ino, l.start, l.end.as_str()), ("posix", "WRITE", 1234, "0:46", 5678, 0, "EOF"));
        let b = parse_lock("2: -> FLOCK  ADVISORY  READ 9 08:01:7 0 EOF").unwrap();
        assert!(b.blocked);
        assert_eq!(b.kind, "flock");
        assert!(parse_lock("garbage").is_err());
    }

    // Lines as CRIU 4.3 prints them (file-lock.c dump_one_file_lock / restore_file_lock).
    const DUMP: &str = "(00.5) LOCK flag: 1,type: 1,pid: 77,fd: 12,start:   0,len:   0\n(00.5) LOCK flag: 1,type: 0,pid: 77,fd: 13,start:  64,len:   1\n(00.6) LOCK flag: 2,type: 1,pid: 80,fd: 5,start:   0,len:   0\n";
    const APPLIED: &str = "(00.2) (posix)flag: 1, type: 1, cmd: 7, pid: 77, fd: 12, start: 0x0, len: 0x0\n(00.2) (posix)flag: 1, type: 0, cmd: 7, pid: 77, fd: 13, start: 0x64, len: 0x1\n(00.2) (flock)flag: 2, type: 1, pid: 80, fd: 5\n";

    #[test]
    fn dumped_and_applied_agree_when_criu_restored_every_lock() {
        let d = dumped_from_log(DUMP);
        let a = applied_from_log(APPLIED);
        assert_eq!(d.len(), 3);
        assert!(same_entries(&d, &a), "{d:?} vs {a:?}");
        // the flock entry carries no range on either side
        assert!(d.contains(&[Some(2), Some(1), Some(80), Some(5), None, None]));
        // a lost lock is detected
        assert!(!same_entries(&d, &a[..2]));
    }

    #[test]
    fn census_shape_matches_the_dump_shape() {
        let mut f = Fake { files: HashMap::new(), dirs: HashMap::new(), links: HashMap::new() };
        f.dirs.insert("/proc/10/fd".into(), vec!["12".into(), "13".into(), "3".into()]);
        f.files.insert("/proc/10/fdinfo/3".into(), "pos:\t0\nflags:\t02\nmnt_id:\t30\n".into());
        f.files.insert("/proc/10/fdinfo/12".into(), "pos:\t0\nmnt_id:\t31\nlock:\t1: POSIX  ADVISORY  WRITE 10 00:2e:99 0 EOF\n".into());
        f.files.insert("/proc/10/fdinfo/13".into(), "pos:\t0\nmnt_id:\t31\nlock:\t2: POSIX  ADVISORY  READ 10 00:2e:98 100 100\n".into());
        f.dirs.insert("/proc/11/fd".into(), vec!["5".into()]);
        f.files.insert("/proc/11/fdinfo/5".into(), "mnt_id:\t31\nlock:\t3: FLOCK  ADVISORY  WRITE 11 00:2e:97 0 EOF\n".into());
        f.links.insert("/proc/10/fd/12".into(), "/home/user/.local/share/x.db".into());
        let c = census(&f, &[10, 11]).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].fd, 12);
        assert_eq!(c[0].mnt_id, Some(31));
        let shape = shape_of_census(&c);
        let want: BTreeSet<Shape> = [(1, 1, Some(0), Some(0)), (1, 0, Some(100), Some(1)), (2, 1, None, None)].into_iter().collect();
        assert_eq!(shape, want);
        let dumped = vec![
            [Some(1), Some(1), Some(77), Some(12), Some(0), Some(0)],
            [Some(1), Some(0), Some(77), Some(13), Some(100), Some(1)],
            [Some(2), Some(1), Some(80), Some(5), None, None],
        ];
        assert_eq!(shape_of_dump(&dumped), want);
        assert_eq!(census_tuples(&c)[0], [Some(1), Some(0), Some(13), Some(100), Some(1)]);
    }

    #[test]
    fn tree_membership_by_pid_namespace_and_children() {
        let mut f = Fake { files: HashMap::new(), dirs: HashMap::new(), links: HashMap::new() };
        f.dirs.insert("/proc".into(), vec!["1".into(), "10".into(), "11".into(), "12".into(), "self".into()]);
        f.links.insert("/proc/1/ns/pid".into(), "pid:[1]".into());
        for p in [10, 11, 12] {
            f.links.insert(format!("/proc/{p}/ns/pid"), "pid:[2]".into());
        }
        assert_eq!(pid_ns_members(&f, 10), vec![10, 11, 12]);
        f.dirs.insert("/proc/10/task".into(), vec!["10".into()]);
        f.files.insert("/proc/10/task/10/children".into(), "11 ".into());
        f.dirs.insert("/proc/11/task".into(), vec!["11".into(), "13".into()]);
        f.files.insert("/proc/11/task/11/children".into(), "".into());
        f.files.insert("/proc/11/task/13/children".into(), "12".into());
        assert_eq!(children_walk(&f, 10), vec![11, 12]);
    }
}
