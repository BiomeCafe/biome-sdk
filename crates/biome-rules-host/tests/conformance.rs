//! Conformance: the compiled WebAssembly of each toy game must behave
//! byte-for-byte like the same crate running natively.
//!
//! The tests build the two example crates for `wasm32-unknown-unknown` (into
//! `target/rules-wasm`, separate from the test build's own target dir), play
//! scripted and random matches natively with `biome_rules::sim`, and replay
//! every step through the interpreter, comparing state bytes, turn, outcome,
//! views and scoped events. That is the determinism guarantee the match host
//! relies on: a deck shuffled on x86 and one shuffled in wasm are the same
//! deck.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use biome_rules::engine::events_for;
use biome_rules::sim::{self, SimResult};
use biome_rules::{codec, Audience, Engine, LegalMoves, Reject, Rules, Scoped, Seat, Turn};
use biome_rules_host::{HostError, RulesModule};
use rules_highcard::HighCard;
use rules_tictactoe::TicTacToe;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Build both example crates to wasm once per test binary.
fn wasm(name: &str) -> Vec<u8> {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    let dir = BUILT.get_or_init(|| {
        let root = workspace_root();
        let target_dir = root.join("target").join("rules-wasm");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&root)
            .args(["build", "--release", "--target", "wasm32-unknown-unknown", "-p", "rules-tictactoe", "-p", "rules-highcard", "--target-dir"])
            .arg(&target_dir)
            // The outer `cargo test` exports flags meant for the host
            // target; they must not leak into the wasm build.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("run cargo build for wasm32 (is the wasm32-unknown-unknown target installed?)");
        assert!(status.success(), "building the example rules crates for wasm32 failed");
        target_dir.join("wasm32-unknown-unknown").join("release")
    });
    std::fs::read(dir.join(format!("{name}.wasm"))).unwrap_or_else(|e| panic!("read {name}.wasm: {e}"))
}

fn tictactoe() -> RulesModule {
    RulesModule::load(&wasm("rules_tictactoe")).expect("load tictactoe")
}

fn highcard() -> RulesModule {
    RulesModule::load(&wasm("rules_highcard")).expect("load highcard")
}

/// Replay a natively simulated match through the wasm module and compare
/// everything observable at every step.
fn replay_matches<R: Rules + LegalMoves>(module: &RulesModule, sim: &SimResult)
where
    R::View: PartialEq + core::fmt::Debug,
    R::Event: PartialEq + core::fmt::Debug + Clone,
{
    let state = module.init(&sim.config, &sim.seats, &sim.init_seed).unwrap().expect("init ok");
    assert_eq!(state, sim.states[0], "initial state differs between native and wasm");

    let mut native = Engine::<R>::from_state_bytes(&sim.states[0]).unwrap();
    let mut state = state;
    check_observables::<R>(module, &state, &native);

    for (i, entry) in sim.log.iter().enumerate() {
        let mv: R::Move = codec::decode(&entry.mv).unwrap();
        let native_events = native.apply(entry.seat, mv, entry.seed).expect("native apply");

        let ok = module.apply(&state, entry.seat, &entry.mv, &entry.seed).unwrap().expect("wasm apply");
        assert_eq!(ok.state, sim.states[i + 1], "state after move {i} differs");
        assert_eq!(ok.state, native.state_bytes().unwrap());

        // Same events, same audiences, same order.
        let expected: Vec<(Audience, Vec<u8>)> =
            native_events.iter().map(|e: &Scoped<R::Event>| (e.to.clone(), codec::encode(&e.event).unwrap())).collect();
        assert_eq!(normalize(ok.events), normalize(expected), "events after move {i} differ");

        state = ok.state;
        check_observables::<R>(module, &state, &native);
    }

    assert_eq!(module.outcome(&state).unwrap().unwrap(), sim.outcome);
}

/// `Seats([x])` and `Seat(x)` are the same audience on the wire.
fn normalize(events: Vec<(Audience, Vec<u8>)>) -> Vec<(Audience, Vec<u8>)> {
    events
        .into_iter()
        .map(|(a, b)| {
            let a = match a {
                Audience::Seats(v) if v.len() == 1 => Audience::Seat(v[0]),
                other => other,
            };
            (a, b)
        })
        .collect()
}

fn check_observables<R: Rules>(module: &RulesModule, state: &[u8], native: &Engine<R>)
where
    R::View: PartialEq + core::fmt::Debug,
{
    assert_eq!(module.to_move(state).unwrap().unwrap(), native.to_move());
    assert_eq!(module.outcome(state).unwrap().unwrap(), native.outcome());
    for seat in [Seat(0), Seat(1), Seat::SPECTATOR] {
        let bytes = module.view(state, seat).unwrap().unwrap();
        let view: R::View = codec::decode(&bytes).unwrap();
        assert_eq!(view, native.view(seat), "view for seat {} differs", seat.0);
    }
}

// ── module validation ───────────────────────────────────────────────────────

#[test]
fn modules_load_and_describe_themselves() {
    let t = tictactoe();
    assert_eq!(t.meta().name, "tictactoe");
    assert_eq!(t.meta().rules_version, TicTacToe::VERSION);
    assert_eq!((t.meta().abi, t.meta().min_seats, t.meta().max_seats), (1, 2, 2));
    let h = highcard();
    assert_eq!(h.meta().name, "highcard");
    assert_eq!(biome_rules_host::validate(&wasm("rules_highcard")).unwrap(), h.meta().clone());
}

#[test]
fn garbage_is_not_a_module() {
    assert!(matches!(RulesModule::load(b"not wasm at all"), Err(HostError::Invalid(_))));
    assert!(matches!(RulesModule::load(&[]), Err(HostError::Invalid(_))));
}

#[test]
fn a_module_with_an_import_is_refused() {
    // Hand-assembled: (module (type (func)) (import "env" "f" (func (type 0))))
    let wasm: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: one () -> ()
        0x02, 0x09, 0x01, 0x03, b'e', b'n', b'v', 0x01, b'f', 0x00, 0x00, // import env.f
    ];
    match RulesModule::load(wasm) {
        Err(HostError::Invalid(msg)) => assert!(msg.contains("import"), "{msg}"),
        other => panic!("expected an import refusal, got {other:?}"),
    }
}

#[test]
fn an_empty_module_is_missing_its_exports() {
    let wasm: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    match RulesModule::load(wasm) {
        Err(HostError::Invalid(msg)) => assert!(msg.contains("missing export"), "{msg}"),
        other => panic!("expected a missing-export refusal, got {other:?}"),
    }
}

/// A syntactically valid module that exports `memory` and all ten ABI names,
/// every function typed `() -> ()`, with `initial_pages` of linear memory.
fn stub_module(initial_pages: u32) -> Vec<u8> {
    fn leb(mut v: u32, out: &mut Vec<u8>) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }
    fn section(id: u8, body: Vec<u8>, out: &mut Vec<u8>) {
        out.push(id);
        leb(body.len() as u32, out);
        out.extend(body);
    }
    const NAMES: [&str; 10] = [
        "br_abi", "br_alloc", "br_free", "br_meta", "br_init", "br_apply", "br_timeout", "br_to_move",
        "br_view", "br_outcome",
    ];
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    section(1, vec![0x01, 0x60, 0x00, 0x00], &mut m); // one type: () -> ()
    let mut funcs = vec![NAMES.len() as u8];
    funcs.extend(std::iter::repeat(0x00).take(NAMES.len()));
    section(3, funcs, &mut m);
    let mut mem = vec![0x01, 0x00];
    leb(initial_pages, &mut mem);
    section(5, mem, &mut m);
    let mut exports = vec![NAMES.len() as u8 + 1];
    for (i, name) in NAMES.iter().enumerate() {
        exports.push(name.len() as u8);
        exports.extend(name.as_bytes());
        exports.extend([0x00, i as u8]);
    }
    exports.push(6);
    exports.extend(b"memory");
    exports.extend([0x02, 0x00]);
    section(7, exports, &mut m);
    let mut code = vec![NAMES.len() as u8];
    for _ in NAMES {
        code.extend([0x02, 0x00, 0x0b]); // body: no locals, end
    }
    section(10, code, &mut m);
    m
}

#[test]
fn exports_with_the_wrong_signatures_are_refused_at_load() {
    // All ten names are present, so a names-only check would pass this.
    match RulesModule::load(&stub_module(1)) {
        Err(HostError::Invalid(msg)) => assert!(msg.contains("wrong signature"), "{msg}"),
        other => panic!("expected a signature refusal, got {other:?}"),
    }
    assert!(biome_rules_host::validate(&stub_module(1)).is_err());
}

#[test]
fn a_module_that_wants_more_memory_than_the_cap_is_refused() {
    // 2000 pages = 125 MiB up front, against a 64 MiB cap.
    assert!(2000 * 65_536 > biome_rules_host::MAX_MEMORY_BYTES);
    match RulesModule::load(&stub_module(2000)) {
        Err(HostError::Invalid(msg)) => assert!(msg.contains("instantiate"), "{msg}"),
        other => panic!("expected the memory cap to refuse instantiation, got {other:?}"),
    }
    // The same stub with one page gets past instantiation (and is then
    // refused for its signatures), so it is the size that was refused above.
    match RulesModule::load(&stub_module(1)) {
        Err(HostError::Invalid(msg)) => assert!(!msg.contains("instantiate"), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_starved_call_runs_out_of_fuel_instead_of_hanging() {
    // Enough fuel to pass validation is more than a few instructions, so
    // load normally and then re-load with a budget too small for init.
    let bytes = wasm("rules_highcard");
    match RulesModule::load_with_fuel(&bytes, 50) {
        Err(HostError::OutOfFuel) => {}
        Err(HostError::Trap(msg)) => panic!("fuel exhaustion should be reported as OutOfFuel, got Trap({msg})"),
        other => panic!("expected OutOfFuel, got {other:?}"),
    }
}

// ── the files the match host actually ships ─────────────────────────────────

/// `cloudflare/biome-match/rules/<app>/<version>.wasm` is what production
/// runs. It is checked in (Workers can only run bundled modules), so it can
/// go stale when a rules crate changes. This replays native matches through
/// the checked-in bytes: if the crate and the bundle disagree about anything
/// observable, refresh the bundle with `scripts/build-rules-wasm.sh`.
fn bundled(app: &str, version: &str) -> Vec<u8> {
    let path = workspace_root().join("cloudflare").join("biome-match").join("rules").join(app).join(format!("{version}.wasm"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn bundled_tictactoe_is_the_current_rules() {
    let module = RulesModule::load(&bundled("tictactoe", TicTacToe::VERSION)).expect("bundled tictactoe passes validation");
    assert_eq!(module.meta().rules_version, TicTacToe::VERSION);
    for i in 0..10u8 {
        let sim = sim::play_random::<TicTacToe>(&rules_tictactoe::Config {}, 2, [i.wrapping_add(100); 32], 20).unwrap();
        replay_matches::<TicTacToe>(&module, &sim);
    }
}

#[test]
fn bundled_highcard_is_the_current_rules() {
    let module = RulesModule::load(&bundled("highcard", HighCard::VERSION)).expect("bundled highcard passes validation");
    assert_eq!(module.meta().rules_version, HighCard::VERSION);
    for i in 0..10u8 {
        let cfg = rules_highcard::Config { hand_size: 1 + i % 4, rounds: 1 + i % 6 };
        let sim = sim::play_random::<HighCard>(&cfg, 2, [i.wrapping_add(200); 32], 100).unwrap();
        replay_matches::<HighCard>(&module, &sim);
    }
}

// ── native ≡ wasm ───────────────────────────────────────────────────────────

#[test]
fn tictactoe_wasm_matches_native_over_random_games() {
    let module = tictactoe();
    for i in 0..25u8 {
        let sim = sim::play_random::<TicTacToe>(&rules_tictactoe::Config {}, 2, [i; 32], 20).unwrap();
        replay_matches::<TicTacToe>(&module, &sim);
    }
}

#[test]
fn highcard_wasm_matches_native_including_the_shuffle() {
    let module = highcard();
    for i in 0..25u8 {
        let cfg = rules_highcard::Config { hand_size: 1 + i % 5, rounds: 1 + i % 7 };
        let sim = sim::play_random::<HighCard>(&cfg, 2, [i.wrapping_mul(37); 32], 100).unwrap();
        assert!(sim.outcome.is_some());
        // Every round is played out: two plays per round, nothing stranded.
        // (hand_size 1 used to end the match after a single play.)
        assert_eq!(sim.log.len(), 2 * cfg.rounds as usize, "hand {} rounds {}", cfg.hand_size, cfg.rounds);
        let last: rules_highcard::State = codec::decode(sim.states.last().unwrap()).unwrap();
        assert_eq!(last.table, [None, None]);
        replay_matches::<HighCard>(&module, &sim);
    }
}

// ── hidden information through the module ───────────────────────────────────

#[test]
fn highcard_views_and_events_keep_hands_private_through_wasm() {
    let module = highcard();
    let cfg = codec::encode(&rules_highcard::Config::default()).unwrap();
    let seats = sim::seats(2);
    let state = module.init(&cfg, &seats, &[5; 32]).unwrap().unwrap();
    let truth: rules_highcard::State = codec::decode(&state).unwrap();

    let v0: rules_highcard::View = codec::decode(&module.view(&state, Seat(0)).unwrap().unwrap()).unwrap();
    let v1: rules_highcard::View = codec::decode(&module.view(&state, Seat(1)).unwrap().unwrap()).unwrap();
    let vs: rules_highcard::View = codec::decode(&module.view(&state, Seat::SPECTATOR).unwrap().unwrap()).unwrap();
    assert_eq!(v0.hand, truth.hands[0]);
    assert_eq!(v1.hand, truth.hands[1]);
    assert!(vs.hand.is_empty());
    assert!(truth.hands[1].iter().all(|c| !v0.hand.contains(c)));

    // Play one full round; the draw each seat receives is scoped to it.
    let m0 = codec::encode(&rules_highcard::Move::Play { card: truth.hands[0][0] }).unwrap();
    let s1 = module.apply(&state, Seat(0), &m0, &[1; 32]).unwrap().unwrap();
    let m1 = codec::encode(&rules_highcard::Move::Play { card: truth.hands[1][0] }).unwrap();
    let s2 = module.apply(&s1.state, Seat(1), &m1, &[2; 32]).unwrap().unwrap();

    let events: Vec<Scoped<rules_highcard::Event>> = s2
        .events
        .iter()
        .map(|(a, b)| Scoped { to: a.clone(), event: codec::decode(b).unwrap() })
        .collect();
    let for0 = events_for(&events, Seat(0));
    let for1 = events_for(&events, Seat(1));
    let drew = |evs: &[rules_highcard::Event]| evs.iter().filter(|e| matches!(e, rules_highcard::Event::Drew { .. })).count();
    assert_eq!((drew(&for0), drew(&for1)), (1, 1));
    assert_eq!(drew(&events_for(&events, Seat::SPECTATOR)), 0);
    assert_ne!(for0, for1);
}

// ── rejects and hostile input ───────────────────────────────────────────────

#[test]
fn rejects_carry_stable_codes_and_no_state() {
    let module = tictactoe();
    let cfg = codec::encode(&rules_tictactoe::Config {}).unwrap();
    let state = module.init(&cfg, &sim::seats(2), &[0; 32]).unwrap().unwrap();
    let place = |cell: u8| codec::encode(&rules_tictactoe::Move::Place { cell }).unwrap();

    let code = |r: Result<biome_rules::wire::ApplyOk, Reject>| r.err().map(|r| r.code);
    assert_eq!(code(module.apply(&state, Seat(1), &place(0), &[0; 32]).unwrap()).as_deref(), Some(Reject::NOT_YOUR_TURN));
    assert_eq!(code(module.apply(&state, Seat(0), &place(9), &[0; 32]).unwrap()).as_deref(), Some(Reject::ILLEGAL));
    assert_eq!(code(module.apply(&state, Seat(0), &[0xff, 0xff, 0xff], &[0; 32]).unwrap()).as_deref(), Some(Reject::BAD_MOVE));
    assert_eq!(code(module.apply(&[0xde, 0xad], Seat(0), &place(0), &[0; 32]).unwrap()).as_deref(), Some(Reject::BAD_STATE));
    assert_eq!(code(module.apply(&state, Seat::SPECTATOR, &place(0), &[0; 32]).unwrap()).as_deref(), Some(Reject::NOT_YOUR_TURN));

    // A legal move still works afterwards: rejected calls left nothing behind.
    let ok = module.apply(&state, Seat(0), &place(4), &[0; 32]).unwrap().unwrap();
    assert_eq!(module.to_move(&ok.state).unwrap().unwrap(), Turn::Seat(Seat(1)));
}

#[test]
fn bad_setup_is_rejected_not_trapped() {
    let module = highcard();
    let good = codec::encode(&rules_highcard::Config::default()).unwrap();
    let code = |r: Result<Vec<u8>, Reject>| r.err().map(|r| r.code);
    assert_eq!(code(module.init(&good, &sim::seats(1), &[0; 32]).unwrap()).as_deref(), Some(Reject::BAD_SEATS));
    assert_eq!(code(module.init(&good, &sim::seats(3), &[0; 32]).unwrap()).as_deref(), Some(Reject::BAD_SEATS));
    assert_eq!(code(module.init(&[], &sim::seats(2), &[0; 32]).unwrap()).as_deref(), Some(Reject::BAD_CONFIG));
    let zero_hand = codec::encode(&rules_highcard::Config { hand_size: 0, rounds: 3 }).unwrap();
    assert_eq!(code(module.init(&zero_hand, &sim::seats(2), &[0; 32]).unwrap()).as_deref(), Some(Reject::BAD_CONFIG));

    // Garbage state on the read-only calls is a reject too.
    assert_eq!(module.to_move(&[1, 2, 3]).unwrap().unwrap_err().code, Reject::BAD_STATE);
    assert_eq!(module.view(&[1, 2, 3], Seat(0)).unwrap().unwrap_err().code, Reject::BAD_STATE);
    assert_eq!(module.outcome(&[]).unwrap().unwrap_err().code, Reject::BAD_STATE);
}

#[test]
fn timeout_through_wasm_forfeits_the_seat_on_the_clock() {
    let module = tictactoe();
    let cfg = codec::encode(&rules_tictactoe::Config {}).unwrap();
    let state = module.init(&cfg, &sim::seats(2), &[0; 32]).unwrap().unwrap();
    assert_eq!(module.timeout(&state, Seat(1), &[0; 32]).unwrap().unwrap_err().code, Reject::NOT_YOUR_TURN);
    let ok = module.timeout(&state, Seat(0), &[0; 32]).unwrap().unwrap();
    let outcome = module.outcome(&ok.state).unwrap().unwrap().unwrap();
    assert_eq!(outcome.winners, vec![Seat(1)]);
    assert_eq!(outcome.reason, "forfeit");
    assert_eq!(module.timeout(&ok.state, Seat(1), &[0; 32]).unwrap().unwrap_err().code, Reject::GAME_OVER);
}
