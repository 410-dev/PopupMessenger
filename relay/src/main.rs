use popup_relay::{start, Config};

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();
    match start(cfg).await {
        Ok(r) => {
            let s = &r.server;
            println!("popup-relay {} · server id {}", popup_relay::VERSION, s.id);
            println!("relay  : {} (TLS 1.3 + plain, auto-detected)", r.relay_addr);
            println!("admin  : http://{}", r.admin_addr);
            println!(
                "TLS    : {} certificate, SHA-256 {}",
                if s.tls.self_signed {
                    "self-signed"
                } else {
                    "provided"
                },
                s.tls.fingerprint
            );
            println!("data   : {}", s.dir.display());
            let _ = tokio::signal::ctrl_c().await;
            // SIGTERM (docker stop) ends the process by default; SQLite WAL is crash-safe.
        }
        Err(e) => {
            eprintln!("popup-relay: {e}");
            std::process::exit(1);
        }
    }
}
