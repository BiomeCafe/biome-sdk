# Biome game SDK

Build turn-based online games for [Biome](https://biome.cafe). Players sign in through the Biome desktop app, so your game never handles passwords. Matches run on Biome's servers, and every move is checked by your game's own rules. Releases ship to players through the Biome desktop library.

This SDK is **invite-only**. Access to this repository is how you got here. Publishing on Biome needs a second invite: a Biome admin grants your guild publishing rights. See [NOTICE.md](NOTICE.md).

| You get | Crate |
|---|---|
| Sign-in through the desktop app, short-lived signed game tickets | `biome-online-services` |
| A client for hosted matches: create, join, play, reconnect | `biome-online-services` (feature `match-client`) |
| The rules SDK: one Rust crate runs in your client and on the server | `biome-rules` |
| A local match host for development | `biome-match-dev` |
| The publishing CLI for your CI | `biome-publish` |
| A rules validator, the same check the server runs | `biome-rules-host` (`validate-rules`) |
| Two complete example games | `rules-tictactoe` (start here), `rules-highcard` (hidden cards) |

Everything is Rust and engine-agnostic. It works with Bevy, macroquad, or a plain window.

---

## 1. Add the SDK

The repository is private, so cargo has to fetch it with your git login. In your game's repo, add `.cargo/config.toml`:

```toml
[net]
git-fetch-with-cli = true
```

`git ls-remote https://github.com/BiomeCafe/biome-sdk` must work in your shell. If it doesn't, run `gh auth setup-git` or sign in with Git Credential Manager.

Pin a release tag; `main` moves:

```toml
[dependencies]
biome-online-services = { git = "https://github.com/BiomeCafe/biome-sdk", tag = "v0.1.3", features = ["match-client"] }
biome-rules = { git = "https://github.com/BiomeCafe/biome-sdk", tag = "v0.1.3" }
```

Tools:

```sh
export CARGO_NET_GIT_FETCH_WITH_CLI=true
cargo install --git https://github.com/BiomeCafe/biome-sdk --tag v0.1.3 biome-match-dev
cargo install --git https://github.com/BiomeCafe/biome-sdk --tag v0.1.3 biome-publish
cargo install --git https://github.com/BiomeCafe/biome-sdk --tag v0.1.3 biome-rules-host --bin validate-rules
```

## 2. Register your game

1. Ask a Biome admin to grant your guild **publishing rights**.
2. On your guild page, open **GAMES**, then **+ NEW GAME**. Pick an **app id**: lowercase letters, digits and `-`, 3 to 64 characters, for example `mecha-aether`. It is permanent. Tickets, matches and releases are all keyed by it.
3. Fill in the store page (name, summary, description, cover). Leave **listed** off until you have a release.

## 3. Sign players in through Biome

Your game talks to the Biome desktop app on the same machine. It gets the player's id and username, and short-lived signed tickets for your game. It never gets their password or login tokens.

```rust
use std::time::Duration;
use biome_online_services::{detect, BiomeClient, BosConfig, BosError, ConnectOptions};

let cfg = BosConfig::new("my-game", "My Game");            // app id, display name

match detect::ensure_running("my-game", Duration::from_secs(60)) {
    Err(BosError::PlatformNotInstalled) => show_download_link(),   // https://biome.cafe/download
    Err(e) => show_error(e),
    Ok(_) => {
        // Non-blocking: poll once per frame.
        let mut pending = BiomeClient::connect_background(cfg, ConnectOptions {
            client_version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        });
        // In your update loop:
        if let Some(result) = pending.poll() {
            match result {
                Ok(bos) => { let me = bos.session().unwrap(); /* me.user_id, me.username */ }
                Err(BosError::NotSignedIn) => ask_player_to_sign_in_to_biome(),
                Err(BosError::ConsentDenied) => offer_offline_play(),
                Err(e) => show_error(e),
            }
        }
    }
}
```

- `ensure_running` starts Biome if it's installed but not running.
- **The first time**, Biome asks the player "My Game wants to sign in as mandy. Allow?" and remembers the answer. While the prompt is open, `pending.status()` is `AwaitingConsent`, so show "allow My Game in Biome…" instead of freezing.
- **Launched from the Biome library**, your game gets a one-time `BIOME_SESSION_NONCE` in its environment. `connect_background` picks it up and signs in with no prompt.
- **Bevy:** `src/bevy.rs` in `biome-online-services` is about 100 lines of glue. Copy it into your game. The crate's own `bevy` feature tracks an older Bevy.

When you need a ticket, for example to join a match:

```rust
let handle = bos.game_ticket();          // poll handle.poll() each frame
// Some(Ok(t)) => t.ticket is a signed token, good for 5 minutes, single use
```

## 4. Write your rules

Your rules are one Rust crate. It compiles natively into your client, which uses it for previews, legal-move hints and AI. It also compiles to WebAssembly for the server, which runs it for every move. Start by copying `crates/rules-tictactoe`. It is the template.

```toml
# rules-my-game/Cargo.toml
[lib]
crate-type = ["cdylib", "rlib"]     # cdylib = the server's .wasm, rlib = your client

[dependencies]
biome-rules = { git = "https://github.com/BiomeCafe/biome-sdk", tag = "v0.1.3" }
serde = { version = "1", features = ["derive"] }
```

```rust
use biome_rules::{Outcome, Reject, Rng, Rules, Scoped, Seat, SeatInfo, Turn};

pub struct MyGame;

impl Rules for MyGame {
    const NAME: &'static str = "my-game";
    const VERSION: &'static str = "1.0.0";      // the rules_version; bump it on ANY rules change
    const MIN_SEATS: u8 = 2;
    const MAX_SEATS: u8 = 2;

    type Config = Config;   // match options chosen at create
    type State = State;     // everything, including hidden information
    type Move = Move;       // what a player sends
    type View = View;       // what ONE seat may see
    type Event = Event;     // things to animate, sent to chosen seats

    fn init(config: &Config, seats: &[SeatInfo], rng: &mut Rng) -> Result<State, Reject> { /* … */ }
    fn apply(state: &mut State, seat: Seat, mv: Move, rng: &mut Rng) -> Result<Vec<Scoped<Event>>, Reject> { /* … */ }
    fn to_move(state: &State) -> Turn { /* … */ }
    fn view(state: &State, seat: Seat) -> View { /* … */ }
    fn outcome(state: &State) -> Option<Outcome> { /* … */ }
    fn on_timeout(state: &mut State, seat: Seat, rng: &mut Rng) -> Vec<Scoped<Event>> { /* … */ }
}

biome_rules::export!(MyGame);   // the WebAssembly exports; does nothing on native targets
```

**Rules for rules:**
- **Deterministic.** Use only the `Rng` you're given for randomness. No clock, no `std::time`, no threads, no file or network access.
- **No imports in the .wasm.** Crates that pull in `wasm-bindgen`, `getrandom` or `js-sys` won't validate. Plain `serde` data types are fine.
- **Hidden information goes through `view`.** The server sends each seat only its own view. `Scoped` events go only to the seats you name. `rules-highcard` shows how.
- **`apply` returns `Err(Reject)` for an illegal move.** The state is rolled back and only the mover hears about it.
- **Limits:** state ≤ 1 MiB, a move ≤ 64 KiB, config ≤ 64 KiB.
- Implement the optional `LegalMoves` trait to get greyed-out illegal moves and a free random bot.

Test it like any Rust code. `biome_rules::sim::play` and `sim::play_random` run whole matches natively. Then build and validate the server module:

```sh
cargo build --release --target wasm32-unknown-unknown -p rules-my-game
validate-rules target/wasm32-unknown-unknown/release/rules_my_game.wasm
```

## 5. Test matches locally

```sh
biome-match-dev --rules my-game=target/wasm32-unknown-unknown/release/rules_my_game.wasm --self-play
```

This serves `http://127.0.0.1:8787`. It is the same protocol as production, with two differences: tickets are not verified, and results stay in memory. Use dev tickets of the form `dev:<any id>:<name>`. `--self-play` lets one account take both seats.

## 6. Play hosted matches

```rust
use biome_online_services::match_client::{Entry, MatchClient, MatchEndpoint, MatchEvent};

const MATCH_HOST: &str = "https://biome-match.aerodeskio.workers.dev";   // local: http://127.0.0.1:8787

let endpoint = MatchEndpoint::new(MATCH_HOST, "my-game", MyGame::VERSION);
let client = MatchClient::start(endpoint, ticket, Entry::Create { config: biome_rules::codec::encode(&my_config)?, clock: None });
// Join instead: Entry::Join { code }. After a restart: Entry::Resume { match_id, seat_token } (no ticket needed),
// or Entry::Rejoin { match_id } with a fresh ticket if you lost the seat token.

// Each frame:
for ev in client.drain() {
    match ev {
        MatchEvent::Seated(info) => { /* info.join_code to share; save info.seat_token for Resume */ }
        MatchEvent::Welcome(w) => { let view = w.view_as::<MyGame>(); /* full state for your seat */ }
        MatchEvent::Update(u) => { let view = u.view_as::<MyGame>(); let events = u.events_as::<MyGame>(); }
        MatchEvent::Rejected { code, message, .. } => { /* not-your-turn, illegal-move, … */ }
        MatchEvent::Over { outcome } => { /* winners, draw, reason */ }
        MatchEvent::Disconnected { reconnecting } => { /* the client reconnects by itself while true */ }
        _ => {}
    }
}
client.send_typed::<MyGame>(&Move::Play { card })?;
```

- The client reconnects with backoff and resends unconfirmed moves. A reconnect arrives as a fresh `Welcome`.
- `Over` is reported once, even if the match ended while you were offline.
- **Clocks** are optional: `clock: Some(ClockConfig { initial_ms, increment_ms })`. When a seat runs out of time, `on_timeout` decides what happens.
- Results are recorded to each player's Biome match history.
- Your client must send the same `rules_version` the server runs. A mismatch is refused with `rules-version-mismatch`, which usually means "update the game".

## 7. Publish a release

**`biome.toml`** at your repo root:

```toml
[game]
app_id = "my-game"

[release]
version = "1.0.0"            # your release version, semver
channel = "stable"           # or "beta"
notes = "release-notes.md"   # optional: a file whose text players see

[rules]                      # for games with hosted matches
wasm = "target/wasm32-unknown-unknown/release/rules_my_game.wasm"

[client.windows-x64]         # one per platform: windows-x64, macos-arm64, macos-x64, linux-x64
path = "dist/windows"        # a folder (zipped for you) or a .zip
entrypoint = "MyGame.exe"    # the executable inside, '/'-separated
```

**Publish token:** on the GAMES tab, open MANAGE, then **publish tokens**, then create. The token is shown once. Store it as a CI secret named `BIOME_PUBLISH_TOKEN`.

```sh
export BIOME_PUBLISH_ENDPOINT=https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-publish
biome-publish check     # offline: manifest, rules validation, zips, hashes
biome-publish upload    # create the release, upload, submit for review
biome-publish status    # review state of recent releases
```

**Then:**
1. A Biome admin reviews the release and approves or rejects it with a note. You see it on the GAMES tab and in `biome-publish status`.
2. Once it's approved, players can install and update your game from the Biome desktop library, as long as the game is **listed**.
3. Your rules module goes live on the match host with the next Biome deploy. Tell your Biome contact when you need it sooner.
4. Old rules versions stay available while matches still use them. A different module under a `rules_version` that's already approved is refused, so bump the version.

**CI example** for GitHub Actions in your game repo. `SDK_READ_TOKEN` is a fine-grained token with read access to `BiomeCafe/biome-sdk`:

```yaml
- run: git config --global url."https://x-access-token:${{ secrets.SDK_READ_TOKEN }}@github.com/BiomeCafe/biome-sdk".insteadOf "https://github.com/BiomeCafe/biome-sdk"
- run: cargo install --git https://github.com/BiomeCafe/biome-sdk --tag v0.1.3 biome-publish
  env: { CARGO_NET_GIT_FETCH_WITH_CLI: "true" }
- run: cargo build --release --target wasm32-unknown-unknown -p rules-my-game
- run: biome-publish upload
  env:
    BIOME_PUBLISH_TOKEN: ${{ secrets.BIOME_PUBLISH_TOKEN }}
    BIOME_PUBLISH_ENDPOINT: https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-publish
```

## 8. Reference

| | |
|---|---|
| Match host | `https://biome-match.aerodeskio.workers.dev` |
| Publish endpoint | `https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-publish` |
| Ticket public keys (to verify tickets yourself) | `https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-ticket/jwks` |
| Tickets | ES256 JWT, `aud` = your app id, 5 minutes, one create / join / rejoin each |
| Join codes | 6 characters, valid 2 hours. 10 wrong guesses per player per 5 minutes, then a pause |
| Lobbies | a match nobody joined is abandoned after 30 minutes |
| Release files | client zip ≤ 4 GiB per platform, rules module ≤ 8 MiB, at most 5 releases uploading at once |

**Sign-in errors** (`BosError`):
- `PlatformNotInstalled`: Biome isn't installed.
- `PlatformUnavailable`: Biome is installed but not running or not reachable.
- `NotSignedIn`: Biome is running but nobody is signed in.
- `ConsentDenied` or `ConsentTimeout`: the player said no, or didn't answer.
- `UnknownApp`: the app id isn't registered, or the game is turned off.
- `HandshakeRejected(code)`: any other refusal, for example `backend-unavailable` when Biome couldn't reach its servers.

**Match errors:**
- From your rules: the codes you return in `Reject`.
- From the host: `not-started`, `game-over`, `bad-move`, `out-of-time` and `rules-error`, plus `rules-version-mismatch`, `bad-seat-token`, `replaced` (a newer connection took the seat), `abandoned` and `match-fault` (the server gave the match up).

## 9. Help

Ask your Biome contact. Include your app id, the `rules_version`, and the match id if one is involved. `cargo test --workspace` in this repo runs every SDK test. The examples in `crates/biome-online-services/examples` show sign-in (`attach_demo`) and a full match bot (`tictactoe_bot`).
