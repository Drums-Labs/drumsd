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

async fn serve(cfg: config::Config) {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("drumsd: must run as root (cgroups, mount namespaces, CRIU)");
        std::process::exit(2);
    }
    let listen = cfg.listen.clone();
    let m: Arc<manager::Manager> = match manager::Manager::new(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("drumsd: {e}");
            std::process::exit(2);
        }
    };
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
