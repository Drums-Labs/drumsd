//! pidfd helpers: signal and wait on one process incarnation, never a recycled pid.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub fn open(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn send(fd: &OwnedFd, sig: i32) -> io::Result<()> {
    let r = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// True when the process exited within `timeout` (the pidfd becomes readable on exit).
pub fn wait_exit(fd: &OwnedFd, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut p, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if r > 0 {
            return true;
        }
        if r == 0 || Instant::now() >= deadline {
            return false;
        }
    }
}
