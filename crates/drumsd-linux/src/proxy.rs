//! Persistent host-side forwarders into a slot's network namespace.
//!
//! The Python path starts a new `python -m sharedos_rootfs.proxy` process per port after every restore (it is pinned
//! to the old init pid) and polls for it. drumsd keeps one listener per slot port for the slot's whole life and only
//! swaps the target network namespace when a restore finishes. Each client connection is opened from a short-lived
//! thread that `setns(CLONE_NEWNET)`s into the slot's namespace (setns is per thread), exactly the Python proxy's
//! method, so drumsd itself stays in the host namespace. Between a reset's kill and its restore the target is
//! cleared and new connections are closed at once.

use std::io;
use std::net::{SocketAddr, TcpStream as StdTcp};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Default)]
pub struct Target {
    inner: Arc<RwLock<Option<Arc<OwnedFd>>>>,
}

impl Target {
    /// Point at the network namespace of `pid` (opened now: later pid reuse cannot redirect it).
    pub fn set_pid(&self, pid: i32) -> io::Result<()> {
        let f = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
        *self.inner.write().unwrap() = Some(Arc::new(OwnedFd::from(f)));
        Ok(())
    }
    pub fn clear(&self) {
        *self.inner.write().unwrap() = None;
    }
    pub fn get(&self) -> Option<Arc<OwnedFd>> {
        self.inner.read().unwrap().clone()
    }
}

/// Open a TCP connection to 127.0.0.1:port inside the namespace `ns` (from a throwaway thread).
pub fn connect_in_netns(ns: Arc<OwnedFd>, port: u16, timeout: Duration) -> io::Result<StdTcp> {
    std::thread::spawn(move || {
        let r = unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        StdTcp::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), timeout)
    })
    .join()
    .map_err(|_| io::Error::other("netns connect thread panicked"))?
}

async fn handle(mut client: TcpStream, target: Target, guest_port: u16) {
    let Some(ns) = target.get() else { return };
    let backend = tokio::task::spawn_blocking(move || connect_in_netns(ns, guest_port, Duration::from_secs(10))).await;
    let Ok(Ok(b)) = backend else { return };
    if b.set_nonblocking(true).is_err() {
        return;
    }
    let Ok(mut b) = TcpStream::from_std(b) else { return };
    let _ = client.set_nodelay(true);
    let _ = b.set_nodelay(true);
    let _ = tokio::io::copy_bidirectional(&mut client, &mut b).await;
}

/// Bind now (so a port conflict is an error at slot creation), then serve in the background.
pub async fn spawn(listen_port: u16, guest_port: u16, target: Target) -> io::Result<tokio::task::JoinHandle<()>> {
    let l = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], listen_port))).await?;
    Ok(tokio::spawn(async move {
        loop {
            match l.accept().await {
                Ok((c, _)) => {
                    tokio::spawn(handle(c, target.clone(), guest_port));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Forwarding into our own network namespace (needs CAP_SYS_ADMIN for setns; skipped otherwise).
    #[tokio::test]
    async fn forwards_and_refuses_without_target() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: needs root for setns");
            return;
        }
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gport = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut c, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 5];
                    c.read_exact(&mut b).await.unwrap();
                    c.write_all(&b).await.unwrap();
                });
            }
        });
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let lport = probe.local_addr().unwrap().port();
        drop(probe);
        let t = Target::default();
        spawn(lport, gport, t.clone()).await.unwrap();
        // no target: closed at once
        let mut c = TcpStream::connect(("127.0.0.1", lport)).await.unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(c.read(&mut buf).await.unwrap(), 0);
        t.set_pid(std::process::id() as i32).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", lport)).await.unwrap();
        c.write_all(b"hello").await.unwrap();
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }
}
