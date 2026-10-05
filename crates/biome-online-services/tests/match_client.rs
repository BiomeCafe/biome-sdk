//! `MatchClient` against a real (local) match host running the compiled
//! example games. Everything a game would do, through the public API only:
//! create, join by code, play, get rejected, see hidden information stay
//! hidden, lose the connection and carry on.
//!
//! The host is `biome-match-dev`, which speaks the production protocol and
//! runs the same WebAssembly modules the production host bundles.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use biome_match_dev::Host;
use biome_online_services::match_client::{ClockConfig, Entry, MatchClient, MatchEndpoint, MatchEvent, SeatedInfo, Update, Welcome};
use biome_rules::{codec, Rules};
use biome_rules_host::RulesModule;
use rules_highcard::HighCard;
use rules_tictactoe::TicTacToe;

const WAIT: Duration = Duration::from_secs(10);

fn bundled(app: &str, version: &str) -> RulesModule {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..").join("..").join("cloudflare").join("biome-match").join("rules").join(app).join(format!("{version}.wasm"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    RulesModule::load(&bytes).expect("bundled module validates")
}

/// A dev host on a free port, alive until dropped.
struct Server {
    base: String,
    host: Arc<Host>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn server() -> Server {
    // highcard allows self-play here (as both demo games do in production);
    // tictactoe does not, so both behaviours are covered.
    let host = Arc::new(
        Host::new()
            .add_rules("tictactoe", bundled("tictactoe", TicTacToe::VERSION))
            .add_rules("highcard", bundled("highcard", HighCard::VERSION))
            .allow_self_play("highcard"),
    );
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let host_for_thread = host.clone();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(async move {
            let (addr, task) = biome_match_dev::serve(host_for_thread, "127.0.0.1:0".parse().unwrap()).await.unwrap();
            addr_tx.send(addr).unwrap();
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
            task.abort();
        });
    });
    let addr = addr_rx.recv_timeout(Duration::from_secs(10)).expect("server started");
    Server { base: format!("http://{addr}"), host, stop: Some(stop_tx), thread: Some(thread) }
}

fn endpoint(s: &Server, app: &str, version: &str) -> MatchEndpoint {
    MatchEndpoint::new(s.base.clone(), app, version)
}

/// Wait for the first event `pick` accepts, skipping the others.
fn wait_for<T>(client: &MatchClient, what: &str, mut pick: impl FnMut(MatchEvent) -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match client.wait_event(left) {
            Some(ev) => {
                if let Some(v) = pick(ev) {
                    return v;
                }
            }
            None => panic!("timed out waiting for {what}"),
        }
    }
}

fn seated(c: &MatchClient) -> SeatedInfo {
    wait_for(c, "Seated", |e| match e { MatchEvent::Seated(i) => Some(i), MatchEvent::Failed { code, message } => panic!("failed: {code} {message}"), _ => None })
}

fn welcome(c: &MatchClient, status: &str) -> Welcome {
    wait_for(c, &format!("Welcome({status})"), |e| match e { MatchEvent::Welcome(w) if w.status == status => Some(w), _ => None })
}

fn update(c: &MatchClient, seq: u64) -> Update {
    wait_for(c, &format!("Update(seq {seq})"), |e| match e { MatchEvent::Update(u) if u.seq == seq => Some(u), _ => None })
}

/// Wait until the client has seen the match at `seq`, by either route.
fn reaches_seq(c: &MatchClient, seq: u64) {
    wait_for(c, &format!("seq {seq}"), |e| match e {
        MatchEvent::Welcome(w) if w.seq >= seq => Some(()),
        MatchEvent::Update(u) if u.seq >= seq => Some(()),
        _ => None,
    })
}

/// Two seated, connected, started tic-tac-toe clients.
fn tictactoe_pair(s: &Server, clock: Option<ClockConfig>) -> (MatchClient, MatchClient, SeatedInfo) {
    let ep = endpoint(s, "tictactoe", TicTacToe::VERSION);
    let a = MatchClient::start(ep.clone(), "dev:user-a:mandy", Entry::Create { config: Vec::new(), clock });
    let info = seated(&a);
    assert_eq!(info.seat, 0);
    let code = info.join_code.clone().expect("creator gets a join code");
    welcome(&a, "waiting");
    let b = MatchClient::start(ep, "dev:user-b:rex", Entry::Join { code });
    assert_eq!(seated(&b).seat, 1);
    welcome(&a, "active");
    welcome(&b, "active");
    (a, b, info)
}

fn place(c: &MatchClient, cell: u8) -> u64 {
    c.send_typed::<TicTacToe>(&rules_tictactoe::Move::Place { cell }).unwrap()
}

#[test]
fn two_clients_play_tictactoe_to_a_win() {
    let s = server();
    let (a, b, info) = tictactoe_pair(&s, None);

    let mut seq = 0;
    for (who, cell) in [(&a, 0u8), (&b, 3), (&a, 1), (&b, 4), (&a, 2)] {
        place(who, cell);
        seq += 1;
        for c in [&a, &b] {
            let u = update(c, seq);
            assert_eq!(u.kind, "move");
            let events = u.events_as::<TicTacToe>().unwrap();
            assert_eq!(events, vec![rules_tictactoe::Event::Placed { seat: u.by, cell }]);
        }
    }
    for c in [&a, &b] {
        let outcome = wait_for(c, "Over", |e| match e { MatchEvent::Over { outcome } => Some(outcome), _ => None });
        assert_eq!(outcome.winners, vec![biome_rules::Seat(0)]);
        assert_eq!(outcome.reason, "three-in-a-row");
    }
    // The host logged every move with its seed.
    let log = s.host.log("tictactoe", &info.match_id).unwrap();
    assert_eq!(log.len(), 5);
    assert!(log.windows(2).all(|w| w[0].seed != w[1].seed));
}

#[test]
fn views_are_typed_and_per_seat() {
    let s = server();
    let ep = endpoint(&s, "tictactoe", TicTacToe::VERSION);
    let a = MatchClient::start(ep.clone(), "dev:user-a:mandy", Entry::Create { config: Vec::new(), clock: None });
    let info = seated(&a);
    let b = MatchClient::start(ep, "dev:user-b:rex", Entry::Join { code: info.join_code.clone().unwrap() });
    seated(&b);
    let wa = welcome(&a, "active");
    let wb = welcome(&b, "active");
    let va = wa.view_as::<TicTacToe>().unwrap().unwrap();
    let vb = wb.view_as::<TicTacToe>().unwrap().unwrap();
    assert_eq!((va.you, vb.you), (Some(0), Some(1)));
    assert_eq!(wa.to_move, vec![0]);
    assert_eq!(wa.seats.iter().map(|s| s.username.as_str()).collect::<Vec<_>>(), vec!["mandy", "rex"]);
}

#[test]
fn rejected_moves_come_back_with_their_client_seq() {
    let s = server();
    let (a, b, _) = tictactoe_pair(&s, None);

    let seq_b = place(&b, 0); // not B's turn
    let (client_seq, code) = wait_for(&b, "Rejected", |e| match e { MatchEvent::Rejected { client_seq, code, .. } => Some((client_seq, code)), _ => None });
    assert_eq!((client_seq, code.as_str()), (Some(seq_b), "not-your-turn"));

    place(&a, 4);
    update(&a, 1);
    let seq_taken = place(&b, 4); // taken
    let (client_seq, code) = wait_for(&b, "Rejected", |e| match e { MatchEvent::Rejected { client_seq, code, .. } => Some((client_seq, code)), _ => None });
    assert_eq!((client_seq, code.as_str()), (Some(seq_taken), "illegal-move"));

    // A rejected move does not block the next legal one.
    place(&b, 0);
    assert_eq!(update(&b, 2).by, 1);
}

#[test]
fn highcard_hands_and_draws_stay_private() {
    let s = server();
    let ep = endpoint(&s, "highcard", HighCard::VERSION);
    let cfg = codec::encode(&rules_highcard::Config { hand_size: 3, rounds: 2 }).unwrap();
    let a = MatchClient::start(ep.clone(), "dev:user-a:mandy", Entry::Create { config: cfg, clock: None });
    let info = seated(&a);
    let b = MatchClient::start(ep, "dev:user-b:rex", Entry::Join { code: info.join_code.unwrap() });
    seated(&b);
    let va = welcome(&a, "active").view_as::<HighCard>().unwrap().unwrap();
    let vb = welcome(&b, "active").view_as::<HighCard>().unwrap().unwrap();
    assert_eq!(va.hand.len(), 3);
    assert!(va.hand.iter().all(|c| !vb.hand.contains(c)));
    assert_eq!(va.hand_sizes, [3, 3]);

    a.send_typed::<HighCard>(&rules_highcard::Move::Play { card: va.hand[0] }).unwrap();
    update(&a, 1);
    update(&b, 1);
    b.send_typed::<HighCard>(&rules_highcard::Move::Play { card: vb.hand[0] }).unwrap();
    let ua = update(&a, 2);
    let ub = update(&b, 2);

    use rules_highcard::Event;
    let ea = ua.events_as::<HighCard>().unwrap();
    let eb = ub.events_as::<HighCard>().unwrap();
    let drew = |evs: &[Event]| evs.iter().filter_map(|e| if let Event::Drew { card } = e { Some(*card) } else { None }).collect::<Vec<_>>();
    assert_eq!(drew(&ea).len(), 1);
    assert_eq!(drew(&eb).len(), 1);
    assert!(ea.contains(&Event::OpponentDrew { seat: 1 }));
    assert!(eb.contains(&Event::OpponentDrew { seat: 0 }));
    // What A drew is in A's hand and nowhere in what B received.
    let card_a = drew(&ea)[0];
    assert!(ua.view_as::<HighCard>().unwrap().unwrap().hand.contains(&card_a));
    assert!(!ub.view_as::<HighCard>().unwrap().unwrap().hand.contains(&card_a));
    assert!(!drew(&eb).contains(&card_a));
}

#[test]
fn a_dropped_connection_reconnects_and_the_match_continues() {
    let s = server();
    let (a, b, info) = tictactoe_pair(&s, None);
    place(&a, 0);
    update(&a, 1);
    update(&b, 1);

    s.host.kick_all("tictactoe", &info.match_id);
    for c in [&a, &b] {
        wait_for(c, "Disconnected", |e| matches!(e, MatchEvent::Disconnected { reconnecting: true }).then_some(()));
    }
    // A move made while offline is queued and sent after the reconnect.
    place(&b, 4);
    // Both clients come back and end up at seq 2. Whether a client learns of
    // the queued move from its reconnect `welcome` or from an `update`
    // depends on which of the two reconnected first; both are correct.
    for c in [&a, &b] {
        reaches_seq(c, 2);
    }

    // And play goes on normally.
    place(&a, 1);
    assert_eq!(update(&b, 3).by, 0);
    assert_eq!(s.host.log("tictactoe", &info.match_id).unwrap().len(), 3, "the queued move was applied exactly once");
}

#[test]
fn a_saved_seat_token_resumes_without_a_ticket() {
    let s = server();
    let (a, b, info) = tictactoe_pair(&s, None);
    place(&a, 8);
    update(&b, 1);
    drop(a); // the game was closed

    let again = MatchClient::start(
        endpoint(&s, "tictactoe", TicTacToe::VERSION),
        "", // no ticket needed
        Entry::Resume { match_id: info.match_id.clone(), seat_token: info.seat_token.clone() },
    );
    let w = welcome(&again, "active");
    assert_eq!((w.seat, w.seq), (0, 1));
    assert_eq!(w.view_as::<TicTacToe>().unwrap().unwrap().board[8], 1);
}

fn rejected(c: &MatchClient) -> (Option<u64>, String) {
    wait_for(c, "Rejected", |e| match e { MatchEvent::Rejected { client_seq, code, .. } => Some((client_seq, code)), _ => None })
}

/// A restarted game is a new `MatchClient`: its move numbers start at 1
/// again while the host remembers the old client's. Its moves must be
/// played (and its rejections reported), not mistaken for retries.
#[test]
fn a_resumed_client_can_move_after_the_old_one_already_did() {
    let s = server();
    let (a, b, info) = tictactoe_pair(&s, None);
    // The first client makes two moves, so the host has counted 2 for seat 0.
    place(&a, 0);
    update(&b, 1);
    place(&b, 3);
    update(&a, 2);
    place(&a, 1);
    update(&b, 3);
    place(&b, 4);
    update(&a, 4);
    drop(a);

    let again = MatchClient::start(
        endpoint(&s, "tictactoe", TicTacToe::VERSION),
        "",
        Entry::Resume { match_id: info.match_id.clone(), seat_token: info.seat_token.clone() },
    );
    assert_eq!(welcome(&again, "active").seq, 4);

    // A rejection comes back under the number send_move returned.
    let taken = place(&again, 4);
    assert_eq!(taken, 1, "a new client numbers its moves from 1");
    assert_eq!(rejected(&again), (Some(taken), "illegal-move".to_string()));

    // And a legal move is applied: 0, 1, 2 wins the top row for seat 0.
    assert_eq!(place(&again, 2), 2);
    let u = update(&again, 5);
    assert_eq!((u.by, u.kind.as_str()), (0, "move"));
    assert_eq!(update(&b, 5).by, 0);
    for c in [&again, &b] {
        wait_for(c, "Over", |e| matches!(e, MatchEvent::Over { .. }).then_some(()));
    }
}

#[test]
fn a_rejoined_client_can_move_after_the_old_one_already_did() {
    let s = server();
    let (a, b, info) = tictactoe_pair(&s, None);
    place(&a, 0);
    update(&b, 1);
    place(&b, 3);
    update(&a, 2);
    drop(a);

    // The seat token was lost; a fresh ticket gets the seat back.
    let again = MatchClient::start(endpoint(&s, "tictactoe", TicTacToe::VERSION), "dev:user-a:mandy", Entry::Rejoin { match_id: info.match_id.clone() });
    assert_eq!(seated(&again).seat, 0);
    assert_eq!(welcome(&again, "active").seq, 2);

    let first = place(&again, 3); // taken
    assert_eq!(rejected(&again), (Some(first), "illegal-move".to_string()));
    place(&again, 1);
    assert_eq!(update(&again, 3).by, 0);
    assert_eq!(update(&b, 3).by, 0);

    // A reconnect of the resumed client keeps the same numbering: the move
    // made while offline is sent once and applied once.
    place(&b, 4);
    update(&again, 4);
    s.host.kick_all("tictactoe", &info.match_id);
    place(&again, 2);
    reaches_seq(&again, 5);
    reaches_seq(&b, 5);
    for c in [&again, &b] {
        wait_for(c, "Over", |e| matches!(e, MatchEvent::Over { .. }).then_some(()));
    }
}

/// The opponent wins while this client is offline: the welcome after the
/// reconnect carries the outcome, and the client reports Over exactly once.
#[test]
fn a_match_that_ended_while_offline_still_reports_over_once() {
    let s = server();
    let (a, b, info) = tictactoe_pair(&s, None);
    // a: 0, 1, 8   b: 3, 4   then b plays 5 for the middle row.
    for (c, cell, seq) in [(&a, 0, 1), (&b, 3, 2), (&a, 1, 3), (&b, 4, 4), (&a, 8, 5)] {
        place(c, cell);
        reaches_seq(&a, seq);
        reaches_seq(&b, seq);
    }
    drop(a); // a's game is closed
    place(&b, 5);
    wait_for(&b, "Over(b)", |e| matches!(e, MatchEvent::Over { .. }).then_some(()));

    let again = MatchClient::start(
        endpoint(&s, "tictactoe", TicTacToe::VERSION),
        "",
        Entry::Resume { match_id: info.match_id.clone(), seat_token: info.seat_token.clone() },
    );
    let w = welcome(&again, "over");
    assert!(w.outcome.is_some());
    let outcome = wait_for(&again, "Over(again)", |e| match e { MatchEvent::Over { outcome } => Some(outcome), _ => None });
    assert_eq!(outcome.winners.iter().map(|w| w.0).collect::<Vec<_>>(), vec![1]);
    // Only once.
    let deadline = Instant::now() + Duration::from_millis(500);
    while let Some(ev) = again.wait_event(deadline.saturating_duration_since(Instant::now())) {
        assert!(!matches!(ev, MatchEvent::Over { .. }), "Over reported twice");
    }
}

#[test]
fn rejoin_issues_a_new_token_and_retires_the_old_socket() {
    let s = server();
    let (a, _b, info) = tictactoe_pair(&s, None);
    let again = MatchClient::start(endpoint(&s, "tictactoe", TicTacToe::VERSION), "dev:user-a:mandy", Entry::Rejoin { match_id: info.match_id.clone() });
    let new_info = seated(&again);
    assert_eq!(new_info.seat, 0);
    assert_ne!(new_info.seat_token, info.seat_token);
    welcome(&again, "active");
    // The first client was replaced; its old token no longer works, so it stops.
    wait_for(&a, "Disconnected(final)", |e| matches!(e, MatchEvent::Disconnected { reconnecting: false }).then_some(()));
}

#[test]
fn failures_to_get_a_seat_are_reported_not_retried() {
    let s = server();
    let ep = endpoint(&s, "tictactoe", TicTacToe::VERSION);

    let no_code = MatchClient::start(ep.clone(), "dev:user-a:mandy", Entry::Join { code: "ZZZZZZ".into() });
    let code = wait_for(&no_code, "Failed", |e| match e { MatchEvent::Failed { code, .. } => Some(code), _ => None });
    assert_eq!(code, "unknown-code");
    wait_for(&no_code, "Disconnected(final)", |e| matches!(e, MatchEvent::Disconnected { reconnecting: false }).then_some(()));

    let no_ticket = MatchClient::start(ep.clone(), "not-a-dev-ticket", Entry::Create { config: Vec::new(), clock: None });
    let code = wait_for(&no_ticket, "Failed", |e| match e { MatchEvent::Failed { code, .. } => Some(code), _ => None });
    assert_eq!(code, "unauthorized");

    let wrong_version = MatchClient::start(endpoint(&s, "tictactoe", "9.9.9"), "dev:user-a:mandy", Entry::Create { config: Vec::new(), clock: None });
    let code = wait_for(&wrong_version, "Failed", |e| match e { MatchEvent::Failed { code, .. } => Some(code), _ => None });
    assert_eq!(code, "unknown-rules");

    let down = MatchClient::start(MatchEndpoint::new("http://127.0.0.1:9", "tictactoe", "1.0.0"), "dev:user-a:mandy", Entry::Create { config: Vec::new(), clock: None });
    let code = wait_for(&down, "Failed", |e| match e { MatchEvent::Failed { code, .. } => Some(code), _ => None });
    assert_eq!(code, "network");

    // A third player cannot join a started two-seat match.
    let (_a, _b, info) = tictactoe_pair(&s, None);
    let late = MatchClient::start(ep, "dev:user-c:late", Entry::Join { code: info.join_code.unwrap() });
    let code = wait_for(&late, "Failed", |e| match e { MatchEvent::Failed { code, .. } => Some(code), _ => None });
    assert_eq!(code, "match-started");
}

#[test]
fn one_account_can_play_both_seats_where_self_play_is_allowed() {
    let s = server();
    let ep = endpoint(&s, "highcard", HighCard::VERSION);
    let cfg = codec::encode(&rules_highcard::Config { hand_size: 2, rounds: 2 }).unwrap();
    let first = MatchClient::start(ep.clone(), "dev:solo:mandy", Entry::Create { config: cfg, clock: None });
    let info = seated(&first);
    let second = MatchClient::start(ep, "dev:solo:mandy", Entry::Join { code: info.join_code.clone().unwrap() });
    assert_eq!(seated(&second).seat, 1, "the same user takes the next seat");
    let mut hands = [
        welcome(&first, "active").view_as::<HighCard>().unwrap().unwrap().hand,
        welcome(&second, "active").view_as::<HighCard>().unwrap().unwrap().hand,
    ];
    // Two rounds; the lead alternates (seat 0 leads round 1, seat 1 round 2).
    let mut seq = 0;
    for (lead, reply) in [(0usize, 1usize), (1, 0)] {
        for who in [lead, reply] {
            let client = if who == 0 { &first } else { &second };
            client.send_typed::<HighCard>(&rules_highcard::Move::Play { card: hands[who][0] }).unwrap();
            seq += 1;
            let u0 = update(&first, seq);
            let u1 = update(&second, seq);
            hands = [u0.view_as::<HighCard>().unwrap().unwrap().hand, u1.view_as::<HighCard>().unwrap().unwrap().hand];
        }
    }
    for c in [&first, &second] {
        wait_for(c, "Over", |e| matches!(e, MatchEvent::Over { .. }).then_some(()));
    }

    // Where it is not allowed, the second join is a rejoin of seat 0.
    let ep = endpoint(&s, "tictactoe", TicTacToe::VERSION);
    let a = MatchClient::start(ep.clone(), "dev:solo:mandy", Entry::Create { config: Vec::new(), clock: None });
    let code = seated(&a).join_code.unwrap();
    let again = MatchClient::start(ep, "dev:solo:mandy", Entry::Join { code });
    assert_eq!(seated(&again).seat, 0);
}

#[test]
fn a_bad_config_is_refused_at_creation() {
    let s = server();
    let cfg = codec::encode(&rules_highcard::Config { hand_size: 0, rounds: 3 }).unwrap();
    let c = MatchClient::start(endpoint(&s, "highcard", HighCard::VERSION), "dev:user-a:mandy", Entry::Create { config: cfg, clock: None });
    let (code, message) = wait_for(&c, "Failed", |e| match e { MatchEvent::Failed { code, message } => Some((code, message)), _ => None });
    assert_eq!(code, "bad-config");
    assert!(message.contains("hand_size"), "{message}");
}

#[test]
fn the_clock_forfeits_a_player_who_never_moves() {
    let s = server();
    let (a, b, _) = tictactoe_pair(&s, Some(ClockConfig { initial_ms: 1000, increment_ms: 0 }));
    let w = welcome_or_clock(&a);
    assert!(w.is_some());
    // Nobody moves; seat 0's one second runs out and the host forfeits it.
    for c in [&a, &b] {
        let outcome = wait_for(c, "Over", |e| match e { MatchEvent::Over { outcome } => Some(outcome), _ => None });
        assert_eq!(outcome.winners, vec![biome_rules::Seat(1)]);
        assert_eq!(outcome.reason, "forfeit");
    }
}

/// `tictactoe_pair` already consumed the active welcomes; ask for a fresh one
/// to look at the clock.
fn welcome_or_clock(c: &MatchClient) -> Option<u64> {
    c.sync();
    let w = wait_for(c, "Welcome", |e| match e { MatchEvent::Welcome(w) => Some(w), _ => None });
    w.clock.and_then(|m| m.get(&0).copied())
}
