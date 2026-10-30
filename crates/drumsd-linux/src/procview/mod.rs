//! Per-env CPU and memory view ("procview"): a Drums env sees its own limits, not the host's.
//!
//! Without it every app in an env reads the host's /proc/meminfo (185-496 GiB) and CPU count, and sizes caches and
//! thread pools for the host. GIMP sizes its tile cache to half of "physical memory" and is then OOM-killed inside
//! its 4 GiB env, which a 4 GiB VM never does (drums-info rl-ab 20261030T0545Z round 2).
//!
//! Two parts, both keyed on the cgroup of the task that asks (so a fork child, a restored tree and a task inside a
//! nested namespace each see their own slot, with nothing per process to checkpoint or copy):
//! * [`fuse`]: a read-only FUSE file system at [`MOUNTPOINT`] serving meminfo, cpuinfo, stat, swaps and the cpu
//!   online/possible/present lists; the slot's init bind-mounts them over its /proc and /sys files (drums-lab tree.py),
//!   and CRIU carries the binds as external mounts (`--external mnt[...]`), re-bound from this mount at every restore.
//! * [`sysinfo`]: a BPF fexit program on the sysinfo(2) syscall that rewrites the memory, swap and process fields
//!   for slot tasks (glibc's sysconf(_SC_PHYS_PAGES), which GIMP and Chrome use, is sysinfo(2)).
//!
//! `drumsd procview` runs both (systemd unit drums-procview.service, installed by install.sh).

pub mod fuse;
pub mod sysinfo;
pub mod view;

use std::os::fd::IntoRawFd;
use std::path::Path;

pub const MOUNTPOINT: &str = "/run/drums/procview";
/// CRIU external-mount key of a view file: `drums-pv-<name>`.
pub fn ext_key(name: &str) -> String {
    format!("drums-pv-{name}")
}

/// `--external mnt[KEY]:PATH` restore arguments for the view binds a checkpoint's mount table holds
/// (`mounts-pre-dump.json` rows [mountpoint, fstype, root]).
pub fn restore_externals(rows: &[[String; 3]], dir: &Path) -> Vec<String> {
    let mut v = vec![];
    for r in rows {
        if r[1] != fuse::FSTYPE {
            continue;
        }
        let name = r[2].trim_start_matches('/');
        if let Some((n, target)) = view::FILES.iter().find(|(n, _)| *n == name) {
            if *target == r[0] {
                v.push("--external".into());
                v.push(format!("mnt[{}]:{}", ext_key(n), dir.join(n).display()));
            }
        }
    }
    v
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// `drumsd procview [--mount DIR] [--threads N] [--no-bpf]`: mount (or take back from systemd's fd store) the view
/// file system, attach the sysinfo program, tell systemd READY, serve until unmounted.
pub fn main(args: &[String]) -> i32 {
    let dir = arg(args, "--mount").unwrap_or_else(|| MOUNTPOINT.into());
    let threads: usize = arg(args, "--threads").and_then(|t| t.parse().ok()).unwrap_or(8);
    let dir = Path::new(&dir);
    let ctx = match fuse::Ctx::system() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("drumsd procview: host facts: {e}");
            return 2;
        }
    };
    let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let fd = match fuse::stored_fd("fuse") {
        Some(fd) if fuse::mounted_here(&mi, dir) => {
            eprintln!("drumsd procview: serving the existing mount at {} (fd {fd} from the fd store)", dir.display());
            fd
        }
        other => {
            if let Some(fd) = other {
                unsafe { libc::close(fd) };
            }
            if fuse::mounted_here(&mi, dir) {
                // a mount whose server is gone: every bind of it fails (ENOTCONN); new restores need a live one
                let c = std::ffi::CString::new(dir.display().to_string()).unwrap();
                unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
                eprintln!("drumsd procview: detached a dead mount at {}", dir.display());
            }
            match fuse::mount(dir) {
                Ok(fd) => fd.into_raw_fd(),
                Err(e) => {
                    eprintln!("drumsd procview: mount {}: {e}", dir.display());
                    return 2;
                }
            }
        }
    };
    match fuse::sd_notify("FDSTORE=1\nFDNAME=fuse", &[fd]) {
        Ok(true) => {}
        Ok(false) => eprintln!("drumsd procview: not run by systemd: the mount will not survive a restart of this process"),
        Err(e) => eprintln!("drumsd procview: FDSTORE: {e}"),
    }
    let bpf = if args.iter().any(|a| a == "--no-bpf") {
        None
    } else {
        match sysinfo::attach() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("drumsd procview: sysinfo program: {e}");
                return 3;
            }
        }
    };
    let _ = fuse::sd_notify("READY=1\nSTATUS=serving", &[]);
    eprintln!("drumsd procview: serving {} ({threads} threads, sysinfo program {})", dir.display(), if bpf.is_some() { "attached" } else { "off" });
    let keep = bpf;
    fuse::serve(fd, ctx, threads);
    drop(keep);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_externals_name_only_the_view_binds_in_the_dump() {
        let rows = vec![
            ["/".to_string(), "overlay".into(), "/".into()],
            ["/proc".to_string(), "proc".into(), "/".into()],
            ["/proc/meminfo".to_string(), fuse::FSTYPE.into(), "/meminfo".into()],
            ["/sys/devices/system/cpu/online".to_string(), fuse::FSTYPE.into(), "/cpu_online".into()],
            // a view file bound somewhere else is not mapped (the mount contract refuses such a dump anyway)
            ["/etc/passwd".to_string(), fuse::FSTYPE.into(), "/meminfo".into()],
        ];
        let v = restore_externals(&rows, Path::new("/run/drums/procview"));
        assert_eq!(v, vec!["--external", "mnt[drums-pv-meminfo]:/run/drums/procview/meminfo", "--external", "mnt[drums-pv-cpu_online]:/run/drums/procview/cpu_online"]);
        assert!(restore_externals(&rows[..2], Path::new("/x")).is_empty());
    }
}
