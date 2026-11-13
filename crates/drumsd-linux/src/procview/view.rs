//! What a slot sees in place of the host's /proc and /sys CPU and memory files: values derived ONLY from the slot's
//! own cgroup (its limits and usage), never a host value.
//!
//! A reader is resolved from its cgroup v2 path (`/proc/<tid>/cgroup`, line `0::`). A reader is a slot reader when
//! one of the path's components is a Drums slot cgroup (`drumsbench-<marker>-pool.scope` inside the pool,
//! `drumsbench-<marker>.scope` for a tree made by the Python verbs). Anything else is refused (the FUSE side answers
//! EACCES): a host process never gets a slot view and a slot process never gets the host's.
//!
//! Effective limits follow the kernel: memory.max / memory.swap.max / cpu.max are the minimum over the reader's cgroup
//! and every ancestor; usage is read at the cgroup that sets the effective memory limit (the slot cgroup), which
//! holds the slot's whole tree.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const POOL: &str = "drumsbench-pool.scope";

/// The cgroup v2 path of `tid` relative to the cgroup root ("/system.slice/..."), from `<proc>/<tid>/cgroup`.
pub fn cgroup_of(proc: &Path, tid: u32) -> io::Result<String> {
    let t = fs::read_to_string(proc.join(tid.to_string()).join("cgroup"))?;
    t.lines().find_map(|l| l.strip_prefix("0::").map(|s| s.trim().to_string())).ok_or_else(|| io::Error::other("no cgroup v2 line"))
}

fn is_slot_component(c: &str) -> bool {
    c.starts_with("drumsbench-") && c != POOL && c.ends_with(".scope")
}

/// Whether a cgroup path belongs to a slot (some component is a slot cgroup).
pub fn is_slot_path(rel: &str) -> bool {
    rel.split('/').any(is_slot_component)
}

/// Everything the views are made from, for one reader.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotView {
    pub mem_limit: u64,
    pub mem_current: u64,
    pub swap_limit: u64,
    pub swap_current: u64,
    /// memory.stat of the cgroup that sets the limit (bytes or counts, as the kernel writes them)
    pub stat: BTreeMap<String, u64>,
    pub cpus: u32,
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub pids: u64,
}

/// Host facts that are not secrets about the host's load or size, used for clamping and formatting only.
#[derive(Debug, Clone)]
pub struct HostFacts {
    pub mem_total: u64,
    pub swap_total: u64,
    pub cpus: u32,
    pub cpuinfo: String,
    pub uptime_s: f64,
    pub btime: u64,
    pub overcommit_ratio: u64,
}

fn read_trim(p: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(p)?.trim().to_string())
}

fn limit(p: &Path) -> Option<u64> {
    match read_trim(p).ok()?.as_str() {
        "max" => None,
        v => v.parse().ok(),
    }
}

fn kv(text: &str) -> BTreeMap<String, u64> {
    text.lines().filter_map(|l| l.split_once(' ').and_then(|(k, v)| v.trim().parse().ok().map(|n| (k.to_string(), n)))).collect()
}

/// cpu.max "QUOTA PERIOD" -> CPUs (rounded up), None for "max".
pub fn cpus_of_cpu_max(s: &str) -> Option<u32> {
    let mut it = s.split_whitespace();
    let q = it.next()?;
    if q == "max" {
        return None;
    }
    let q: u64 = q.parse().ok()?;
    let p: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(100000).max(1);
    Some(q.div_ceil(p).max(1) as u32)
}

/// Resolve a reader's view from the cgroup tree at `cg_root` (normally /sys/fs/cgroup). Err when the path is not a
/// slot's or a required file cannot be read (the caller refuses the read: fail closed).
pub fn slot_view(cg_root: &Path, rel: &str, host: &HostFacts) -> io::Result<SlotView> {
    if !is_slot_path(rel) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{rel} is not a Drums slot cgroup")));
    }
    // the chain leaf..root
    let mut chain: Vec<PathBuf> = vec![];
    let mut cur = rel.trim_matches('/').to_string();
    loop {
        chain.push(cg_root.join(&cur));
        match cur.rfind('/') {
            Some(i) => cur.truncate(i),
            None => break,
        }
    }
    let (mut mem, mut mem_at, mut swap, mut cpus) = (None::<u64>, None::<PathBuf>, None::<u64>, None::<u32>);
    for d in &chain {
        if let Some(m) = limit(&d.join("memory.max")) {
            if mem.is_none_or(|x| m < x) {
                mem = Some(m);
                mem_at = Some(d.clone());
            }
        }
        if let Some(s) = limit(&d.join("memory.swap.max")) {
            swap = Some(swap.map_or(s, |x: u64| x.min(s)));
        }
        if let Some(c) = read_trim(&d.join("cpu.max")).ok().as_deref().and_then(cpus_of_cpu_max) {
            cpus = Some(cpus.map_or(c, |x: u32| x.min(c)));
        }
    }
    // a slot always has a memory limit: without one the view would be the host's, so refuse
    let at = mem_at.ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, format!("{rel}: no memory limit on the chain")))?;
    let mem_limit = mem.unwrap().min(host.mem_total.max(1));
    let cpu_stat = kv(&fs::read_to_string(chain[0].join("cpu.stat")).or_else(|_| fs::read_to_string(at.join("cpu.stat")))?);
    let at_cpu = kv(&fs::read_to_string(at.join("cpu.stat")).unwrap_or_default());
    let pick = |k: &str| at_cpu.get(k).or(cpu_stat.get(k)).copied().unwrap_or(0);
    Ok(SlotView {
        mem_limit,
        mem_current: read_trim(&at.join("memory.current"))?.parse().map_err(io::Error::other)?,
        swap_limit: swap.unwrap_or(0).min(host.swap_total),
        swap_current: read_trim(&at.join("memory.swap.current")).ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        stat: kv(&fs::read_to_string(at.join("memory.stat"))?),
        cpus: cpus.unwrap_or(host.cpus).min(host.cpus).max(1),
        usage_usec: pick("usage_usec"),
        user_usec: pick("user_usec"),
        system_usec: pick("system_usec"),
        pids: read_trim(&at.join("pids.current")).ok().and_then(|v| v.parse().ok()).unwrap_or(0),
    })
}

/// The meminfo field names, in the 7.0 kernel's order. Every value is derived from the slot (or is an
/// architecture constant); a field with no slot equivalent is 0. No host value is ever copied.
pub const MEMINFO_FIELDS: [&str; 60] = [
    "MemTotal",
    "MemFree",
    "MemAvailable",
    "Buffers",
    "Cached",
    "SwapCached",
    "Active",
    "Inactive",
    "Active(anon)",
    "Inactive(anon)",
    "Active(file)",
    "Inactive(file)",
    "Unevictable",
    "Mlocked",
    "SwapTotal",
    "SwapFree",
    "Zswap",
    "Zswapped",
    "Dirty",
    "Writeback",
    "AnonPages",
    "Mapped",
    "Shmem",
    "KReclaimable",
    "Slab",
    "SReclaimable",
    "SUnreclaim",
    "KernelStack",
    "PageTables",
    "SecPageTables",
    "NFS_Unstable",
    "Bounce",
    "WritebackTmp",
    "CommitLimit",
    "Committed_AS",
    "VmallocTotal",
    "VmallocUsed",
    "VmallocChunk",
    "Percpu",
    "HardwareCorrupted",
    "AnonHugePages",
    "ShmemHugePages",
    "ShmemPmdMapped",
    "FileHugePages",
    "FilePmdMapped",
    "CmaTotal",
    "CmaFree",
    "Unaccepted",
    "Balloon",
    "GPUActive",
    "GPUReclaim",
    "HugePages_Total",
    "HugePages_Free",
    "HugePages_Rsvd",
    "HugePages_Surp",
    "Hugepagesize",
    "Hugetlb",
    "DirectMap4k",
    "DirectMap2M",
    "DirectMap1G",
];

/// x86_64 constants (the same in every x86_64 guest): the vmalloc area size and the huge page size, in kB.
const VMALLOC_TOTAL_KB: u64 = 34359738367;
const HUGEPAGESIZE_KB: u64 = 2048;

/// The meminfo values in bytes (counts for HugePages_*), derived from the slot.
pub fn meminfo_values(v: &SlotView, host: &HostFacts) -> BTreeMap<&'static str, u64> {
    let s = |k: &str| v.stat.get(k).copied().unwrap_or(0);
    let total = v.mem_limit;
    let used = v.mem_current.min(total);
    let free = total - used;
    let file = s("file");
    let shmem = s("shmem");
    let avail = (free + file.saturating_sub(shmem) + s("slab_reclaimable")).min(total);
    let swap_total = v.swap_limit;
    let swap_free = swap_total.saturating_sub(v.swap_current);
    let mut m = BTreeMap::new();
    for f in MEMINFO_FIELDS {
        m.insert(f, 0u64);
    }
    let vals: [(&'static str, u64); 33] = [
        ("MemTotal", total),
        ("MemFree", free),
        ("MemAvailable", avail),
        ("Cached", file.saturating_sub(s("swapcached"))),
        ("SwapCached", s("swapcached")),
        ("Active", s("active_anon") + s("active_file")),
        ("Inactive", s("inactive_anon") + s("inactive_file")),
        ("Active(anon)", s("active_anon")),
        ("Inactive(anon)", s("inactive_anon")),
        ("Active(file)", s("active_file")),
        ("Inactive(file)", s("inactive_file")),
        ("Unevictable", s("unevictable")),
        ("SwapTotal", swap_total),
        ("SwapFree", swap_free),
        ("Zswap", s("zswap")),
        ("Zswapped", s("zswapped")),
        ("Dirty", s("file_dirty")),
        ("Writeback", s("file_writeback")),
        ("AnonPages", s("anon")),
        ("Mapped", s("file_mapped")),
        ("Shmem", shmem),
        ("KReclaimable", s("slab_reclaimable")),
        ("Slab", s("slab")),
        ("SReclaimable", s("slab_reclaimable")),
        ("SUnreclaim", s("slab_unreclaimable")),
        ("KernelStack", s("kernel_stack")),
        ("PageTables", s("pagetables")),
        ("SecPageTables", s("sec_pagetables")),
        // the kernel's formula with the slot's sizes: RAM * overcommit_ratio / 100 + swap
        ("CommitLimit", total / 100 * host.overcommit_ratio + swap_total),
        // no per-cgroup commit counter exists: the slot's anonymous, shared and swapped memory (an estimate, labelled)
        ("Committed_AS", s("anon") + shmem + v.swap_current),
        ("Percpu", s("percpu")),
        ("AnonHugePages", s("anon_thp")),
        ("ShmemHugePages", s("shmem_thp")),
    ];
    for (k, x) in vals {
        m.insert(k, x);
    }
    m.insert("FileHugePages", s("file_thp"));
    m.insert("VmallocTotal", VMALLOC_TOTAL_KB * 1024);
    m.insert("Hugepagesize", HUGEPAGESIZE_KB * 1024);
    // a guest's direct map covers its RAM; reported as 4k pages of the slot's size
    m.insert("DirectMap4k", total);
    m
}

pub fn render_meminfo(v: &SlotView, host: &HostFacts) -> String {
    let m = meminfo_values(v, host);
    let mut out = String::with_capacity(2048);
    for f in MEMINFO_FIELDS {
        let x = m[f];
        if f.starts_with("HugePages_") {
            out.push_str(&format!("{:<16}{:>8}\n", format!("{f}:"), x));
        } else {
            out.push_str(&format!("{:<16}{:>8} kB\n", format!("{f}:"), x / 1024));
        }
    }
    out
}

/// The host's cpuinfo cut to the slot's first `n` processors and renumbered as one socket of `n` cores (a VM with n
/// vCPUs). The model, flags and caches are the host's, as a VM on this host would show them.
pub fn render_cpuinfo(host_cpuinfo: &str, n: u32) -> String {
    let blocks: Vec<&str> = host_cpuinfo.split("\n\n").map(|b| b.trim_matches('\n')).filter(|b| !b.is_empty()).collect();
    let mut out = String::new();
    for i in 0..n.min(blocks.len() as u32) {
        for line in blocks[i as usize].lines() {
            let key = line.split(':').next().unwrap_or("").trim();
            let val = match key {
                "processor" | "core id" | "apicid" | "initial apicid" => Some(i.to_string()),
                "physical id" => Some("0".into()),
                "siblings" | "cpu cores" => Some(n.to_string()),
                _ => None,
            };
            match val {
                Some(v) => {
                    let (k, _) = line.split_once(':').unwrap_or((line, ""));
                    out.push_str(&format!("{k}: {v}\n"));
                }
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out.push('\n');
    }
    out
}

/// /proc/stat for `n` CPUs: the slot's own CPU time (cpu.stat) spread evenly over its CPUs, idle = the rest of
/// uptime x n. Counters that have no per-slot source (interrupts, context switches) are 0; processes = the slot's
/// task count. USER_HZ = 100.
pub fn render_stat(v: &SlotView, host: &HostFacts) -> String {
    let n = v.cpus.max(1) as u64;
    let t = |us: u64| us / 10_000; // usec -> 1/100 s
    let user = t(v.user_usec);
    let system = t(v.system_usec);
    let busy = t(v.usage_usec).max(user + system);
    let idle = ((host.uptime_s * 100.0) as u64 * n).saturating_sub(busy);
    let mut out = format!("cpu  {user} 0 {system} {idle} 0 0 0 0 0 0\n");
    for i in 0..n {
        out.push_str(&format!("cpu{i} {} 0 {} {} 0 0 0 0 0 0\n", user / n, system / n, idle / n));
    }
    out.push_str(&format!("intr 0\nctxt 0\nbtime {}\nprocesses {}\nprocs_running 1\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n", host.btime, v.pids));
    out
}

pub fn render_swaps(v: &SlotView) -> String {
    let mut out = String::from("Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n");
    if v.swap_limit > 0 {
        out.push_str(&format!(
            "/swap                                   file\t\t{}\t\t{}\t\t-2\n",
            v.swap_limit / 1024,
            v.swap_current.min(v.swap_limit) / 1024
        ));
    }
    out
}

/// /sys/devices/system/cpu/{online,possible,present}: "0-(n-1)".
pub fn render_cpu_list(n: u32) -> String {
    if n <= 1 {
        "0\n".into()
    } else {
        format!("0-{}\n", n - 1)
    }
}

impl HostFacts {
    pub fn read(proc: &Path) -> io::Result<HostFacts> {
        let mi = fs::read_to_string(proc.join("meminfo"))?;
        let kb = |name: &str| {
            mi.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024
        };
        let cpuinfo = fs::read_to_string(proc.join("cpuinfo"))?;
        let cpus = cpuinfo.lines().filter(|l| l.starts_with("processor")).count().max(1) as u32;
        let uptime_s = fs::read_to_string(proc.join("uptime")).ok().and_then(|u| u.split_whitespace().next().and_then(|x| x.parse().ok())).unwrap_or(0.0);
        let btime = fs::read_to_string(proc.join("stat"))
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok())))
            .unwrap_or(0);
        let overcommit_ratio = read_trim(&proc.join("sys/vm/overcommit_ratio")).ok().and_then(|v| v.parse().ok()).unwrap_or(50);
        Ok(HostFacts { mem_total: kb("MemTotal:"), swap_total: kb("SwapTotal:"), cpus, cpuinfo, uptime_s, btime, overcommit_ratio })
    }
}

/// The files the view serves: (name in the FUSE mount, where the slot sees it).
pub const FILES: [(&str, &str); 7] = [
    ("meminfo", "/proc/meminfo"),
    ("cpuinfo", "/proc/cpuinfo"),
    ("stat", "/proc/stat"),
    ("swaps", "/proc/swaps"),
    ("cpu_online", "/sys/devices/system/cpu/online"),
    ("cpu_possible", "/sys/devices/system/cpu/possible"),
    ("cpu_present", "/sys/devices/system/cpu/present"),
];

/// Render file `name` for a reader whose cgroup is `rel`.
pub fn render(name: &str, cg_root: &Path, rel: &str, host: &HostFacts) -> io::Result<String> {
    let v = slot_view(cg_root, rel, host)?;
    Ok(match name {
        "meminfo" => render_meminfo(&v, host),
        "cpuinfo" => render_cpuinfo(&host.cpuinfo, v.cpus),
        "stat" => render_stat(&v, host),
        "swaps" => render_swaps(&v),
        "cpu_online" | "cpu_possible" | "cpu_present" => render_cpu_list(v.cpus),
        _ => return Err(io::Error::new(io::ErrorKind::NotFound, name.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_CPUINFO: &str = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Xeon\nphysical id\t: 0\nsiblings\t: 64\ncore id\t\t: 0\ncpu cores\t: 32\napicid\t\t: 0\ninitial apicid\t: 0\nflags\t\t: fpu sse\n\n\
processor\t: 1\nvendor_id\t: GenuineIntel\nmodel name\t: Xeon\nphysical id\t: 0\nsiblings\t: 64\ncore id\t\t: 0\ncpu cores\t: 32\napicid\t\t: 1\ninitial apicid\t: 1\nflags\t\t: fpu sse\n\n\
processor\t: 2\nvendor_id\t: GenuineIntel\nmodel name\t: Xeon\nphysical id\t: 0\nsiblings\t: 64\ncore id\t\t: 1\ncpu cores\t: 32\napicid\t\t: 2\ninitial apicid\t: 2\nflags\t\t: fpu sse\n\n";

    fn host() -> HostFacts {
        HostFacts {
            mem_total: 496 << 30,
            swap_total: 32 << 30,
            cpus: 64,
            cpuinfo: HOST_CPUINFO.into(),
            uptime_s: 1000.0,
            btime: 1793350000,
            overcommit_ratio: 50,
        }
    }

    /// A pool layout: pool/slot (limits)/leaf (the tree).
    fn tree(root: &Path, mem: &str, swap: &str, cpu: &str) -> String {
        let pool = root.join("system.slice").join(POOL);
        let slot = pool.join("drumsbench-rootfs-ab12cd34-7-pool.scope");
        let leaf = slot.join("drumsbench-rootfs-ab12cd34-7-r0.scope");
        fs::create_dir_all(&leaf).unwrap();
        for d in [root.join("system.slice"), pool.clone(), leaf.clone()] {
            fs::write(d.join("memory.max"), "max\n").unwrap();
            fs::write(d.join("memory.swap.max"), "max\n").unwrap();
            fs::write(d.join("cpu.max"), "max 100000\n").unwrap();
        }
        fs::write(slot.join("memory.max"), format!("{mem}\n")).unwrap();
        fs::write(slot.join("memory.swap.max"), format!("{swap}\n")).unwrap();
        fs::write(slot.join("cpu.max"), format!("{cpu}\n")).unwrap();
        fs::write(slot.join("memory.current"), "1073741824\n").unwrap();
        fs::write(slot.join("memory.swap.current"), "104857600\n").unwrap();
        fs::write(slot.join("pids.current"), "57\n").unwrap();
        fs::write(slot.join("memory.stat"), "anon 600000000\nfile 400000000\nshmem 100000000\nslab_reclaimable 20000000\nslab 30000000\nslab_unreclaimable 10000000\nactive_anon 300000000\ninactive_anon 300000000\nactive_file 200000000\ninactive_file 200000000\nkernel_stack 1000000\npagetables 2000000\n").unwrap();
        fs::write(slot.join("cpu.stat"), "usage_usec 9000000\nuser_usec 6000000\nsystem_usec 3000000\n").unwrap();
        fs::write(leaf.join("cpu.stat"), "usage_usec 9000000\nuser_usec 6000000\nsystem_usec 3000000\n").unwrap();
        "/system.slice/drumsbench-pool.scope/drumsbench-rootfs-ab12cd34-7-pool.scope/drumsbench-rootfs-ab12cd34-7-r0.scope".into()
    }

    fn field(text: &str, name: &str) -> u64 {
        text.lines().find(|l| l.starts_with(&format!("{name}:"))).unwrap().split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    #[test]
    fn slot_paths_are_recognised_and_others_refused() {
        assert!(is_slot_path("/system.slice/drumsbench-pool.scope/drumsbench-rootfs-ab-1-pool.scope/drumsbench-rootfs-ab-1-r1.scope"));
        assert!(is_slot_path("/system.slice/drumsbench-rootfs-ab-0.scope"));
        assert!(!is_slot_path("/system.slice/drumsbench-pool.scope"));
        assert!(!is_slot_path("/system.slice/drumsbench-pool.scope/keeper"));
        assert!(!is_slot_path("/user.slice/user-1000.slice/session-1.scope"));
        assert!(!is_slot_path("/system.slice/drumsd.service"));
        let t = tempfile::tempdir().unwrap();
        let e = slot_view(t.path(), "/system.slice/drumsd.service", &host()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn meminfo_is_the_slots_limit_and_usage_and_carries_no_host_value() {
        let t = tempfile::tempdir().unwrap();
        let rel = tree(t.path(), "4294967296", "3376414720", "400000 100000");
        let h = host();
        let m = render("meminfo", t.path(), &rel, &h).unwrap();
        assert_eq!(field(&m, "MemTotal"), 4 << 20);
        assert_eq!(field(&m, "MemFree"), (4u64 << 20) - (1 << 20));
        assert_eq!(field(&m, "SwapTotal"), 3220 << 10);
        assert_eq!(field(&m, "SwapFree"), (3220 << 10) - (100 << 10));
        assert_eq!(field(&m, "Shmem"), 100000000 / 1024);
        assert!(field(&m, "MemAvailable") <= field(&m, "MemTotal"));
        assert_eq!(field(&m, "DirectMap4k"), 4 << 20);
        assert_eq!(field(&m, "DirectMap2M") + field(&m, "DirectMap1G") + field(&m, "HugePages_Total"), 0);
        assert_eq!(m.lines().count(), MEMINFO_FIELDS.len());
        // nothing that equals or reveals the host size
        for l in m.lines() {
            let v: u64 = l.split_whitespace().nth(1).unwrap().parse().unwrap();
            assert!(v != h.mem_total / 1024 && v != h.swap_total / 1024, "host value in {l}");
            assert!(v <= VMALLOC_TOTAL_KB, "{l}");
        }
        assert!(m.starts_with("MemTotal:        4194304 kB\n"), "{}", &m[..40]);
    }

    #[test]
    fn swap_is_bounded_by_the_host_swap_and_zero_without_one() {
        let t = tempfile::tempdir().unwrap();
        let rel = tree(t.path(), "4294967296", "3376414720", "400000 100000");
        let mut h = host();
        h.swap_total = 0;
        let m = render("meminfo", t.path(), &rel, &h).unwrap();
        assert_eq!(field(&m, "SwapTotal"), 0);
        assert_eq!(render("swaps", t.path(), &rel, &h).unwrap().lines().count(), 1);
    }

    #[test]
    fn cpu_views_match_cpu_max() {
        let t = tempfile::tempdir().unwrap();
        let rel = tree(t.path(), "4294967296", "0", "250000 100000");
        let h = host();
        assert_eq!(render("cpu_online", t.path(), &rel, &h).unwrap(), "0-2\n");
        let ci = render("cpuinfo", t.path(), &rel, &h).unwrap();
        assert_eq!(ci.lines().filter(|l| l.starts_with("processor")).count(), 3);
        assert!(ci.contains("siblings\t: 3\n") && ci.contains("cpu cores\t: 3\n") && ci.contains("processor\t: 2\n") && ci.contains("apicid\t\t: 2\n"), "{ci}");
        assert!(!ci.contains("64") && !ci.contains("32"), "{ci}");
        let st = render("stat", t.path(), &rel, &h).unwrap();
        assert_eq!(st.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count(), 3);
        assert!(st.starts_with("cpu  600 0 300 "), "{st}");
        assert!(st.contains("processes 57\n"));
        assert_eq!(cpus_of_cpu_max("max 100000"), None);
        assert_eq!(cpus_of_cpu_max("400000 100000"), Some(4));
        assert_eq!(cpus_of_cpu_max("50000 100000"), Some(1));
        assert_eq!(render_cpu_list(1), "0\n");
    }

    #[test]
    fn a_child_limit_below_the_slot_wins_and_a_changed_limit_is_seen_at_once() {
        let t = tempfile::tempdir().unwrap();
        let rel = tree(t.path(), "4294967296", "0", "400000 100000");
        let h = host();
        assert_eq!(field(&render("meminfo", t.path(), &rel, &h).unwrap(), "MemTotal"), 4 << 20);
        let slot = t.path().join("system.slice").join(POOL).join("drumsbench-rootfs-ab12cd34-7-pool.scope");
        fs::write(slot.join("memory.max"), "3221225472\n").unwrap();
        assert_eq!(field(&render("meminfo", t.path(), &rel, &h).unwrap(), "MemTotal"), 3 << 20);
    }

    #[test]
    fn a_slot_without_a_memory_limit_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let rel = tree(t.path(), "max", "0", "400000 100000");
        assert_eq!(slot_view(t.path(), &rel, &host()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn cgroup_of_reads_the_v2_line() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("42")).unwrap();
        fs::write(t.path().join("42/cgroup"), "0::/system.slice/x.scope\n").unwrap();
        assert_eq!(cgroup_of(t.path(), 42).unwrap(), "/system.slice/x.scope");
    }
}
