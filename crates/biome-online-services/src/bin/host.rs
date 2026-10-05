//! biome-os-host — standalone reference IPC server.
//!
//! Lets a game test against the BOS protocol without needing biome-app to be
//! running. Provides a `BosBackend` impl backed by canned responses + CLI
//! subcommands to trigger pushes on demand.
//!
//! Usage:
//!   biome-os-host serve --dev-mode --verbose
//!   biome-os-host serve --launch ./my-game.exe --app-id cafe-shooter -- --windowed
//!   biome-os-host serve --dev-mode --match-ready-after 5 --game-server 127.0.0.1:47238
//!
//! --launch is the secure path: the host issues a one-time session nonce and
//! passes it to the game via BIOME_SESSION_NONCE (the Steam pattern), so the
//! handshake works without --dev-mode.
//!
//! When you start `serve`, it prints the IPC endpoint path it's listening on.
//! Point your game at that path (or just use the default).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use biome_online_services::{
    error::BosError,
    events::BiomeEvent,
    server::{BosBackend, BosServer, ServerConfig},
    transport,
    types::{ConnectInfo, GameMode, MapId, MatchId, Region, RoomId, Session, TeamId, VoiceRooms},
    protocol::{RequestBody, ResponseBody},
};
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser)]
#[command(version, about = "Biome Online Services reference IPC host", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the IPC server until killed. Prints incoming frames if --verbose.
    Serve {
        /// Skip nonce verification on incoming handshakes (no `BIOME_SESSION_NONCE`
        /// required from clients).
        #[arg(long)]
        dev_mode: bool,
        /// Log every IPC frame in/out.
        #[arg(long)]
        verbose: bool,
        /// Override the IPC endpoint path.
        #[arg(long)]
        endpoint: Option<String>,
        /// Pre-canned `Session` to surface in handshake replies. Empty = no session.
        #[arg(long, default_value = "dev-user")]
        username: String,
        /// Spawn this game executable with a freshly issued one-time session
        /// nonce in BIOME_SESSION_NONCE, so it can handshake without --dev-mode.
        #[arg(long)]
        launch: Option<std::path::PathBuf>,
        /// App id passed to the launched game as BIOME_APP_ID.
        #[arg(long, default_value = "dev-app")]
        app_id: String,
        /// Broadcast a canned MatchReady to connected clients after N seconds.
        #[arg(long)]
        match_ready_after: Option<u64>,
        /// Game server address used in the canned MatchReady bundle.
        #[arg(long, default_value = "127.0.0.1:47238")]
        game_server: SocketAddr,
        /// Extra arguments for the launched game (after --).
        #[arg(last = true)]
        launch_args: Vec<String>,
    },

    /// Send a one-shot push event into an existing `serve` instance via its
    /// IPC endpoint. Useful for triggering `MatchReady` etc. against a connected
    /// game without going through real matchmaking.
    Simulate {
        #[command(subcommand)]
        what: SimulateCmd,
    },
}

#[derive(Subcommand)]
enum SimulateCmd {
    /// Push a MatchReady event with a canned ConnectInfo bundle.
    MatchReady {
        #[arg(long)]
        match_id: Option<String>,
        /// Vultr game server endpoint, e.g. 127.0.0.1:47238
        #[arg(long)]
        game_server: SocketAddr,
        /// Which team to claim the player is on (0 or 1).
        #[arg(long, default_value_t = 0)]
        team: u8,
        #[arg(long, default_value_t = 128)]
        tick_hz: u16,
        #[arg(long, default_value_t = 1)]
        protocol_version: u32,
        #[arg(long)]
        endpoint: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────

struct DevBackend {
    session: Option<Session>,
}

impl BosBackend for DevBackend {
    async fn match_connect_info(&self, match_id: MatchId) -> Result<ConnectInfo, BosError> {
        Ok(canned_connect_info(match_id, "127.0.0.1:47238".parse().unwrap(), 0))
    }
    async fn session(&self) -> Option<Session> {
        self.session.clone()
    }
    async fn handle_request(&self, body: RequestBody) -> ResponseBody {
        tracing::info!(?body, "unhandled dev request (returning Ack)");
        ResponseBody::Ack
    }
}

fn canned_connect_info(match_id: MatchId, game_server: SocketAddr, team: u8) -> ConnectInfo {
    let now = SystemTime::now();
    let mid = match_id_short(match_id);
    ConnectInfo {
        game_server,
        server_region: Region::UsEast,
        netcode_token: vec![0; 32],
        protocol_version: 1,
        tick_hz: 128,
        voice: VoiceRooms {
            team: RoomId(format!("team-{team}-{mid}")),
            all_chat: RoomId(format!("all-{mid}")),
            party: None,
            access_token: "dev-cf-calls-token".to_string(),
        },
        match_do_url: format!("wss://biome-rt.aerodeskio.workers.dev/match/{mid}"),
        match_do_token: "dev-match-jwt".to_string(),
        match_id,
        map_id: MapId(1),
        mode: GameMode::Custom,
        max_duration_secs: 30 * 60,
        player_count: 16,
        my_team: TeamId(team),
        issued_at: now,
        expires_at: now + Duration::from_secs(120),
    }
}

fn match_id_short(id: MatchId) -> String {
    id.0.to_string().chars().take(8).collect()
}

// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,biome_online_services=debug")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            dev_mode,
            verbose,
            endpoint,
            username,
            launch,
            app_id,
            match_ready_after,
            game_server,
            launch_args,
        } => {
            let endpoint = endpoint.unwrap_or_else(transport::default_endpoint);
            let session = if username.is_empty() {
                None
            } else {
                Some(Session {
                    user_id: Uuid::new_v4(),
                    username: username.clone(),
                    entitlements: vec!["dev".into()],
                })
            };
            let backend = Arc::new(DevBackend { session });
            let server = match BosServer::start(
                ServerConfig {
                    endpoint: Some(endpoint.clone()),
                    dev_mode,
                    verbose,
                },
                backend,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("failed to start BOS host: {e}");
                    std::process::exit(1);
                }
            };

            println!("biome-os-host listening on: {endpoint}");
            if dev_mode {
                println!("DEV MODE: nonce verification disabled, any client accepts");
            }
            let server = Arc::new(server);

            // Secure launch: one nonce per spawned game, consumed on handshake.
            let _child = match launch {
                Some(exe) => {
                    let nonce = server.issue_launch_nonce(&app_id).await;
                    match tokio::process::Command::new(&exe)
                        .args(&launch_args)
                        .env(biome_online_services::handshake::ENV_SESSION_NONCE, &nonce)
                        .env(biome_online_services::handshake::ENV_APP_ID, &app_id)
                        .spawn()
                    {
                        Ok(child) => {
                            println!("launched {} (pid {:?}) with a one-time session nonce", exe.display(), child.id());
                            Some(child)
                        }
                        Err(e) => {
                            eprintln!("failed to launch {}: {e}", exe.display());
                            server.shutdown().await;
                            std::process::exit(1);
                        }
                    }
                }
                None => {
                    if !dev_mode {
                        println!("note: without --dev-mode or --launch, clients need a nonce this host issued");
                    }
                    None
                }
            };

            if let Some(secs) = match_ready_after {
                let server = server.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    let match_id = MatchId(Uuid::new_v4());
                    let bundle = canned_connect_info(match_id, game_server, 0);
                    println!("broadcasting MatchReady for {}", match_id_short(match_id));
                    server.broadcast(BiomeEvent::MatchReady { match_id, bundle });
                });
            }

            println!("press Ctrl-C to stop.");

            // Wait for ctrl-c so the accept loop keeps running.
            let _ = tokio::signal::ctrl_c().await;
            server.shutdown().await;
        }

        Cmd::Simulate { what } => match what {
            SimulateCmd::MatchReady {
                match_id,
                game_server,
                team,
                tick_hz,
                protocol_version,
                endpoint: _,
            } => {
                // For now: not wired to a running server. Print what would be sent.
                // Full implementation requires a separate "control" IPC endpoint
                // on the running host that accepts admin commands — out of scope
                // for Day 1. Game can call DevBackend::match_connect_info
                // directly through the normal client request to get equivalent
                // canned data.
                let match_id = match_id
                    .and_then(|s| Uuid::parse_str(&s).ok())
                    .map(MatchId)
                    .unwrap_or_else(|| MatchId(Uuid::new_v4()));
                let mut info = canned_connect_info(match_id, game_server, team);
                info.tick_hz = tick_hz;
                info.protocol_version = protocol_version;
                let ev = BiomeEvent::MatchReady { match_id, bundle: info };
                println!("would broadcast: {ev:#?}");
                // There's no admin control socket on a running host yet, so this
                // can't reach connected games. Fail loudly instead of pretending.
                eprintln!("simulate can't reach a running host (no control socket yet).");
                eprintln!("use: biome-os-host serve --match-ready-after <secs> --game-server <addr>");
                std::process::exit(2);
            }
        },
    }
}
