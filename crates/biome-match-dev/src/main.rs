//! biome-match-dev — run a match host on your own machine.
//!
//! ```text
//! cargo build -p my-game-rules --release --target wasm32-unknown-unknown
//! biome-match-dev --rules my-game=target/wasm32-unknown-unknown/release/my_game_rules.wasm
//! ```
//!
//! then point the game's `MatchClient` at `http://127.0.0.1:8787` with a dev
//! ticket (`dev:<any user id>:<any name>`). Same routes and frames as the
//! production host; no Cloudflare, no database, **no ticket verification**.
//!
//! Matchmaking works too: queues, open matches, and ranked ratings kept in
//! memory. `--rating a=1800` starts dev user `a` at 1800 in ranked;
//! `--widen-every-ms 1000` makes ranked windows widen ten times faster.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use biome_match_dev::{serve, Host, WindowRule, RANKED_WINDOW};
use biome_rules_host::RulesModule;
use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Local Biome match host for development (no ticket verification)")]
struct Cli {
    /// `<app id>=<path to .wasm>`. Repeat for several games or versions.
    #[arg(long = "rules", value_name = "APP=WASM", required = true)]
    rules: Vec<String>,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Let one user take several seats of a match (play both sides alone).
    #[arg(long)]
    self_play: bool,
    /// `<user id>=<rating>`: start a dev user (the id in `dev:<id>:<name>`)
    /// at this ranked rating until it has played. Repeat for several users.
    #[arg(long = "rating", value_name = "USER=RATING")]
    ratings: Vec<String>,
    /// Ranked windows grow by 25 every this many milliseconds (production: 10000).
    #[arg(long, value_name = "MS")]
    widen_every_ms: Option<i64>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mut host = Host::new();
    for spec in &cli.ratings {
        match spec.split_once('=').map(|(u, r)| (u, r.trim().parse::<f64>())) {
            Some((user, Ok(rating))) if !user.is_empty() && rating.is_finite() => host = host.seed_rating(user, rating),
            _ => {
                eprintln!("--rating expects <user id>=<rating>, got {spec:?}");
                std::process::exit(2);
            }
        }
    }
    if let Some(ms) = cli.widen_every_ms {
        if ms < 1 {
            eprintln!("--widen-every-ms must be at least 1");
            std::process::exit(2);
        }
        host = host.with_ranked_window(WindowRule { every_ms: ms, ..RANKED_WINDOW });
    }
    for spec in &cli.rules {
        let Some((app, path)) = spec.split_once('=') else {
            eprintln!("--rules expects <app id>=<path to .wasm>, got {spec:?}");
            std::process::exit(2);
        };
        let path = PathBuf::from(path);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("read {}: {e}", path.display());
                std::process::exit(1);
            }
        };
        match RulesModule::load(&bytes) {
            Ok(module) => {
                let meta = module.meta().clone();
                println!(
                    "loaded {app}: {} {} ({}..={} seats, {} bytes)",
                    meta.name, meta.rules_version, meta.min_seats, meta.max_seats, bytes.len()
                );
                host = host.add_rules(app, module);
                if cli.self_play {
                    host = host.allow_self_play(app);
                }
            }
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }

    let (addr, task) = match serve(Arc::new(host), cli.listen).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("bind {}: {e}", cli.listen);
            std::process::exit(1);
        }
    };
    println!("biome-match-dev listening on http://{addr}");
    println!("DEV ONLY: tickets are not verified. Use `Authorization: Bearer dev:<user id>:<username>`.");
    let _ = task.await;
}
