//! attach_demo — "does Biome see me?" from a process that is not a game.
//!
//! Depends only on the SDK client. Walks the whole identity path:
//!
//! 1. detect: running / installed / not installed
//! 2. wake Biome if it is installed but closed, wait for the IPC endpoint
//! 3. attach (no launch nonce) — first run prompts inside Biome, later runs don't
//! 4. print the real user's id + username
//! 5. request a game ticket; verify it when a JWKS is given, else just decode it
//!
//! ```text
//! cargo run -p biome-online-services --features ticket-verify --example attach_demo -- \
//!     [--app mecha-aether] [--name "Mecha Aether"] [--jwks <file-or-url-contents.json>] \
//!     [--endpoint <pipe-or-socket>] [--no-wake] [--consent-timeout 120]
//! ```
//!
//! Exit codes: 0 ok · 2 not installed · 3 not signed in · 4 consent denied ·
//! 5 unknown app · 1 anything else.

use std::time::Duration;

use biome_online_services::{detect, BiomeClient, BosConfig, BosError, ConnectOptions, PlatformState, Transport};

struct Args {
    app_id: String,
    app_name: String,
    jwks: Option<String>,
    endpoint: Option<String>,
    wake: bool,
    consent_timeout: u64,
}

fn parse_args() -> Args {
    let mut a = Args {
        app_id: "mecha-aether".into(),
        app_name: "Mecha Aether".into(),
        jwks: None,
        endpoint: None,
        wake: true,
        consent_timeout: 120,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--app" => a.app_id = it.next().unwrap_or_default(),
            "--name" => a.app_name = it.next().unwrap_or_default(),
            "--jwks" => a.jwks = it.next(),
            "--endpoint" => a.endpoint = it.next(),
            "--no-wake" => a.wake = false,
            "--consent-timeout" => a.consent_timeout = it.next().and_then(|s| s.parse().ok()).unwrap_or(120),
            other => eprintln!("ignoring unknown flag {other}"),
        }
    }
    a
}

fn main() {
    let args = parse_args();
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(biome_online_services::transport::default_endpoint);

    // 1. detect
    let state = detect::detect_at(&endpoint);
    println!("biome: {state:?}");
    match &state {
        PlatformState::Running => {}
        PlatformState::Installed { launcher } if args.wake => {
            println!("waking {} …", launcher.display());
            if let Err(e) = detect::wake(&args.app_id) {
                eprintln!("wake failed: {e}");
                std::process::exit(1);
            }
            match detect::wait_until_running_at(&endpoint, Duration::from_secs(90)) {
                Ok(()) => println!("biome is up"),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        PlatformState::Installed { .. } => {
            eprintln!("biome is installed but not running (and --no-wake was given)");
            std::process::exit(1);
        }
        PlatformState::NotInstalled => {
            eprintln!("biome is not installed on this machine — https://biome.cafe/download");
            std::process::exit(2);
        }
    }

    // 2. attach (or nonce handshake if BIOME_SESSION_NONCE is set)
    let mut config = BosConfig::new(args.app_id.clone(), args.app_name.clone());
    if let Some(ep) = args.endpoint.clone() {
        config = config.with_transport(Transport::Custom { path: ep });
    }
    let opts = ConnectOptions {
        consent_timeout: Duration::from_secs(args.consent_timeout),
        client_version: env!("CARGO_PKG_VERSION").into(),
        ..ConnectOptions::default()
    };
    println!("connecting as app {:?} … (first time: allow it in the Biome prompt)", args.app_id);
    let client = match BiomeClient::connect_with(config, opts) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {e}");
            let code = match e {
                BosError::NotSignedIn => 3,
                BosError::ConsentDenied => 4,
                BosError::UnknownApp => 5,
                _ => 1,
            };
            std::process::exit(code);
        }
    };

    match client.session() {
        Some(s) => println!(
            "session: user_id={} username={} via={:?}",
            s.user_id, s.username, client.attached_via()
        ),
        None => println!("connected, but the host has no session (dev host?)"),
    }

    // 3. ticket
    match client.game_ticket().wait() {
        Ok(t) => {
            let secs = t
                .expires_at
                .duration_since(std::time::SystemTime::now())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            println!("ticket ({} chars, expires in {secs}s):\n{}", t.ticket.len(), t.ticket);
            match &args.jwks {
                Some(src) => {
                    let json = std::fs::read_to_string(src).unwrap_or_else(|_| src.clone());
                    match biome_online_services::ticket::Jwks::parse(&json)
                        .and_then(|jwks| biome_online_services::ticket::verify(&t.ticket, &jwks, &args.app_id))
                    {
                        Ok(c) => println!("VERIFIED: {c:#?}"),
                        Err(e) => {
                            eprintln!("verification failed: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                None => match biome_online_services::ticket::decode_unverified(&t.ticket) {
                    Ok(c) => println!("claims (unverified, no --jwks given): {c:#?}"),
                    Err(e) => eprintln!("could not decode ticket: {e}"),
                },
            }
        }
        Err(e) => {
            eprintln!("ticket request failed: {e}");
            std::process::exit(1);
        }
    }
}
