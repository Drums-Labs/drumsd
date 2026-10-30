//! Loader of the sysinfo(2) program (bpf/sysinfo.bpf.c): attach it as fexit on `__x64_sys_sysinfo`, keep its host
//! facts current, and pin the link so a restart of `drumsd procview` never leaves slots on the host's values.
//!
//! The link is pinned at [`PIN`]. At start a new link is attached first, then the old pinned one (from an earlier
//! process) is unpinned and dropped, so there is no moment without a program. If the process dies, the pinned link
//! keeps the program attached with the host facts it last had.

use aya::maps::{Array, MapData, PerCpuArray};
use aya::programs::links::{FdLink, PinnedLink};
use aya::programs::FExit;
use aya::{Btf, Ebpf};
use std::path::Path;

pub const PIN: &str = "/sys/fs/bpf/drums-procview-sysinfo";
pub const TARGET: &str = "__x64_sys_sysinfo";

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Cfg {
    pub host_mem_total: u64,
    pub host_swap_total: u64,
}
unsafe impl aya::Pod for Cfg {}

pub struct Attached {
    _bpf: Ebpf,
    pub stats: Option<PerCpuArray<MapData, u64>>,
}

fn host_cfg() -> Cfg {
    let mi = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |n: &str| mi.lines().find(|l| l.starts_with(n)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
    Cfg { host_mem_total: kb("MemTotal:"), host_swap_total: kb("SwapTotal:") }
}

pub fn attach() -> Result<Attached, String> {
    let mut bpf = Ebpf::load(aya::include_bytes_aligned!("../../bpf/sysinfo.bpf.o")).map_err(|e| format!("load object: {e}"))?;
    let btf = Btf::from_sys_fs().map_err(|e| format!("kernel BTF: {e}"))?;
    let mut cfg: Array<MapData, Cfg> = Array::try_from(bpf.take_map("CFG").ok_or("no CFG map")?).map_err(|e| e.to_string())?;
    cfg.set(0, host_cfg(), 0).map_err(|e| format!("CFG: {e}"))?;
    let stats: Option<PerCpuArray<MapData, u64>> = bpf.take_map("STATS").and_then(|m| PerCpuArray::try_from(m).ok());
    let prog: &mut FExit = bpf.program_mut("on_sysinfo").ok_or("no on_sysinfo program")?.try_into().map_err(|e: aya::programs::ProgramError| e.to_string())?;
    prog.load(TARGET, &btf).map_err(|e| format!("load fexit/{TARGET}: {e}"))?;
    let id = prog.attach().map_err(|e| format!("attach: {e}"))?;
    let link: FdLink = prog.take_link(id).map_err(|e| e.to_string())?.into();
    // the new link is live: retire the previous process's pinned one, then pin this one
    if Path::new(PIN).exists() {
        match PinnedLink::from_pin(PIN) {
            Ok(old) => {
                let _ = old.unpin();
            }
            Err(_) => {
                let _ = std::fs::remove_file(PIN);
            }
        }
    }
    link.pin(PIN).map_err(|e| format!("pin {PIN}: {e}"))?;
    // swap can be added or removed while slots run (the GIMP envelope uses a zram device): refresh every 5 s
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let _ = cfg.set(0, host_cfg(), 0);
    });
    Ok(Attached { _bpf: bpf, stats })
}

/// Counters summed over CPUs: [rewritten, not a slot, read error, write error].
pub fn counters(a: &Attached) -> Option<[u64; 4]> {
    let s = a.stats.as_ref()?;
    let mut out = [0u64; 4];
    for (i, o) in out.iter_mut().enumerate() {
        *o = s.get(&(i as u32), 0).ok()?.iter().sum();
    }
    Some(out)
}
