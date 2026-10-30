//! drumsd: control plane for Drums desktop slots.
//!
//!     drumsd serve --config /etc/drumsd.json      the daemon (root)
//!     drumsd openapi                               print the OpenAPI spec
//!     drumsd restore-client ...                    internal: one restore (see drumsd_linux::restore)
//!     drumsd hook DIR                              internal: CRIU action script (see drumsd_linux::hook)

mod api;
mod config;
mod manager;

use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        // the two internal modes run before any thread exists (the client unshares its mount namespace)
        Some("restore-client") => std::process::exit(drumsd_linux::restore::client_main(&args[2..])),
        Some("hook") => std::process::exit(drumsd_linux::hook::hook_main(args.get(2).map(|s| s.as_str()).unwrap_or("."))),
        Some("openapi") => {
            print!("{}", api::OPENAPI);
        }
        Some("serve") => {
            let cfg_path = args.iter().position(|a| a == "--config").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "/etc/drumsd.json".into());
            let cfg = match config::Config::load(&cfg_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("drumsd: {e}");
                    std::process::exit(2);
                }
            };
            tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().max_blocking_threads(1024).build().unwrap();
            rt.block_on(serve(cfg));
        }
        _ => {
            eprintln!("usage: drumsd serve --config PATH | drumsd openapi");
            std::process::exit(2);
        }
    }
}

/// Raise the soft open-files limit to the hard one. Each env holds listeners, guest connections, pidfds and journal
/// writes; at 64 envs drumsd ran past the default soft limit of 1024 and failed restores, gates, checker spawns and
/// journal writes with EMFILE. Returns (soft, hard) after the change.
pub fn raise_nofile() -> (u64, u64) {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 && r.rlim_cur < r.rlim_max {
            let want = libc::rlimit { rlim_cur: r.rlim_max, rlim_max: r.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &want) == 0 {
                r = want;
            }
        }
    }
    (r.rlim_cur as u64, r.rlim_max as u64)
}

async fn serve(cfg: config::Config) {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("drumsd: must run as root (cgroups, mount namespaces, CRIU)");
        std::process::exit(2);
    }
    let (soft, hard) = raise_nofile();
    tracing::info!("open files limit {soft} (hard {hard})");
    if soft < 65536 {
        tracing::warn!("open files limit {soft} is low for many envs; set LimitNOFILE in the service unit");
    }
    let listen = cfg.listen.clone();
    let m: Arc<manager::Manager> = match manager::Manager::new(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("drumsd: {e}");
            std::process::exit(2);
        }
    };
    m.start_flusher();
    if let Err(e) = drumsd_linux::cgroup::pool_init() {
        eprintln!("drumsd: pool cgroup: {e}");
        std::process::exit(2);
    }
    match m.recover().await {
        Ok(r) => tracing::info!("recovered: {r}"),
        Err(e) => {
            eprintln!("drumsd: recovery from the journal failed: {e}");
            std::process::exit(2);
        }
    }
    let l = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    tracing::info!("drumsd listening on {listen}");
    axum::serve(l, api::router(m)).await.unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn nofile_soft_limit_is_raised_to_the_hard_limit() {
        let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) };
        let low = libc::rlimit { rlim_cur: r.rlim_cur.min(256), rlim_max: r.rlim_max };
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) };
        let (soft, hard) = super::raise_nofile();
        assert_eq!(soft, hard);
        assert_eq!(hard, r.rlim_max as u64);
    }
}
