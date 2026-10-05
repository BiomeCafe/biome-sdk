//! tictactoe_bot — play tic-tac-toe through a match host, end to end.
//!
//! Two ways to run it:
//!
//! **Locally, no accounts** (dev server, tickets not verified):
//!
//! ```text
//! bash scripts/build-rules-wasm.sh
//! cargo run -p biome-match-dev -- --rules tictactoe=cloudflare/biome-match/rules/tictactoe/1.0.0.wasm
//! cargo run -p biome-online-services --features match-client --example tictactoe_bot -- --dev-user a:mandy --create
//! cargo run -p biome-online-services --features match-client --example tictactoe_bot -- --dev-user b:rex --join <CODE>
//! ```
//!
//! **Against production** (real ticket from the running Biome app; the first
//! run asks you to allow "Tic-Tac-Toe (SDK demo)" in Biome):
//!
//! ```text
//! … --example tictactoe_bot -- --url https://biome-match.<account>.workers.dev --create
//! ```
//!
//! and a second Biome account on another machine joins with `--join <CODE>`.
//!
//! The bot picks a random legal move each turn, using the same rules crate
//! the host runs (`rules-tictactoe`), and prints the board as it goes.

use std::time::Duration;

use biome_online_services::match_client::{Entry, MatchClient, MatchEndpoint, MatchEvent};
use biome_online_services::{detect, BiomeClient, BosConfig};
use biome_rules::{LegalMoves, Rng, Rules, Seat};
use rules_tictactoe::{TicTacToe, View};

const APP_ID: &str = "tictactoe";

struct Args {
    url: String,
    dev_user: Option<String>,
    join: Option<String>,
    create: bool,
    delay_ms: u64,
}

fn parse_args() -> Args {
    let mut a = Args { url: "http://127.0.0.1:8787".into(), dev_user: None, join: None, create: false, delay_ms: 400 };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--url" => a.url = it.next().unwrap_or_default(),
            "--dev-user" => a.dev_user = it.next(),
            "--join" => a.join = it.next(),
            "--create" => a.create = true,
            "--delay-ms" => a.delay_ms = it.next().and_then(|s| s.parse().ok()).unwrap_or(400),
            other => eprintln!("ignoring unknown flag {other}"),
        }
    }
    if !a.create && a.join.is_none() {
        eprintln!("say --create or --join <CODE>");
        std::process::exit(2);
    }
    a
}

/// A dev ticket, or a real one from the Biome desktop app.
fn ticket(args: &Args) -> String {
    if let Some(user) = &args.dev_user {
        return format!("dev:{user}");
    }
    println!("asking the Biome app for a ticket…");
    if let Err(e) = detect::ensure_running(APP_ID, Duration::from_secs(90)) {
        eprintln!("biome: {e}");
        std::process::exit(1);
    }
    let bos = match BiomeClient::connect(BosConfig::new(APP_ID, "Tic-Tac-Toe (SDK demo)")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not attach to Biome: {e}");
            std::process::exit(1);
        }
    };
    if let Some(s) = bos.session() {
        println!("signed in as {} ({})", s.username, s.user_id);
    }
    match bos.game_ticket().wait() {
        Ok(t) => t.ticket,
        Err(e) => {
            eprintln!("ticket request failed: {e}");
            std::process::exit(1);
        }
    }
}

fn draw(view: &View) {
    let mark = |c: u8| match c {
        1 => 'X',
        2 => 'O',
        _ => '.',
    };
    for row in view.board.chunks(3) {
        println!("   {} {} {}", mark(row[0]), mark(row[1]), mark(row[2]));
    }
}

fn main() {
    let args = parse_args();
    let ticket = ticket(&args);
    let entry = match &args.join {
        Some(code) => Entry::Join { code: code.clone() },
        None => Entry::Create { config: Vec::new(), clock: None },
    };
    let client = MatchClient::start(MatchEndpoint::new(args.url.clone(), APP_ID, TicTacToe::VERSION), ticket, entry);

    // Bot randomness only; the host's randomness is its own.
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&(std::process::id() as u64 ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)).to_le_bytes());
    let mut rng = Rng::from_seed(seed);
    let mut my_seat = 0u8;

    let mut maybe_move = |client: &MatchClient, view: &View, seat: u8| {
        let moves = TicTacToe::legal_moves(view, Seat(seat));
        if let Some(mv) = rng.pick(&moves) {
            std::thread::sleep(Duration::from_millis(args.delay_ms));
            println!("playing {mv:?}");
            let _ = client.send_typed::<TicTacToe>(mv);
        }
    };

    loop {
        let Some(ev) = client.wait_event(Duration::from_secs(600)) else {
            eprintln!("nothing happened for ten minutes; leaving");
            std::process::exit(1);
        };
        match ev {
            MatchEvent::Seated(info) => {
                my_seat = info.seat;
                println!("seated: match {} seat {}", info.match_id, info.seat);
                if let Some(code) = &info.join_code {
                    println!("JOIN CODE: {code}   (other player: --join {code})");
                }
            }
            MatchEvent::Welcome(w) => {
                println!("[{}] seq {} to_move {:?}", w.status, w.seq, w.to_move);
                if let Some(Ok(view)) = w.view_as::<TicTacToe>() {
                    draw(&view);
                    if w.to_move.contains(&my_seat) {
                        maybe_move(&client, &view, my_seat);
                    }
                }
            }
            MatchEvent::Update(u) => {
                println!("seq {} by seat {} ({})", u.seq, u.by, u.kind);
                if let Some(Ok(view)) = u.view_as::<TicTacToe>() {
                    draw(&view);
                    if u.to_move.contains(&my_seat) {
                        maybe_move(&client, &view, my_seat);
                    }
                }
            }
            MatchEvent::Rejected { code, message, .. } => println!("rejected: {code} {message}"),
            MatchEvent::Seats { status, seats } => {
                let who: Vec<String> = seats.iter().map(|s| format!("{}{}", s.username, if s.connected { "" } else { " (away)" })).collect();
                println!("[{status}] seats: {}", who.join(", "));
            }
            MatchEvent::Over { outcome } => {
                if outcome.draw {
                    println!("DRAW ({})", outcome.reason);
                } else if outcome.winners.contains(&Seat(my_seat)) {
                    println!("I WON ({})", outcome.reason);
                } else {
                    println!("I lost ({})", outcome.reason);
                }
                return;
            }
            MatchEvent::Disconnected { reconnecting: true } => println!("connection lost, reconnecting…"),
            MatchEvent::Disconnected { reconnecting: false } => {
                println!("disconnected");
                std::process::exit(1);
            }
            MatchEvent::Error { code, detail } => println!("host error: {code} {detail}"),
            MatchEvent::Failed { code, message } => {
                eprintln!("could not get a seat: {code} {message}");
                std::process::exit(1);
            }
        }
    }
}
