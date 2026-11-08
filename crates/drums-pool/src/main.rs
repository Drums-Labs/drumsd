//! drums-pool: many drumsd hosts behind one drumsd-compatible API.
//!
//!     drums-pool --config /etc/drums-pool/pool.json
//!
//! See crates/drums-pool/README.md.

mod api;
mod config;
mod metrics;
mod pool;

use config::{read_token, Config};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let args: Vec<String> = std::env::args().collect();
    let mut cfg_path = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                cfg_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--version" => {
                println!("drums-pool {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            a => {
                eprintln!("unknown argument {a}\nusage: drums-pool --config pool.json");
                std::process::exit(2);
            }
        }
    }
    let cfg: Config = match cfg_path {
        Some(p) => match std::fs::read(&p).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("config {p}: {e}");
                std::process::exit(2);
            }
        },
        None => Config::default(),
    };
    let token = read_token(&cfg.token_file).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2)
    });
    let admin = match &cfg.admin_token_file {
        Some(p) => read_token(p).unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(2)
        }),
        None => token.clone(),
    };
    let listen = cfg.listen.clone();
    let hosts = cfg.hosts.clone();
    let p = pool::Pool::new(cfg, token, admin);
    for h in hosts {
        if let Err(e) = p.add_host(h) {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
    p.spawn_background();
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| {
        eprintln!("listen {listen}: {e}");
        std::process::exit(2)
    });
    tracing::info!("drums-pool {} listening on {listen}", env!("CARGO_PKG_VERSION"));
    axum::serve(l, api::router(p)).await.unwrap();
}
