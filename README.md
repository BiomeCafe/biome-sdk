# Biome game SDK

Build turn-based online games for [Biome](https://biome.cafe). Players sign in through the Biome desktop app, so your game never handles passwords. Matches run on Biome's servers, and every move is checked by your game's own rules. Releases ship to players through the Biome desktop library.

This SDK is **invite-only**. Access to this repository is how you got here. Publishing on Biome needs a second invite: a Biome admin grants your guild publishing rights. See [NOTICE.md](NOTICE.md).

| You get | Crate |
|---|---|
| Sign-in through the desktop app, short-lived signed game tickets | `biome-online-services` |
| A client for hosted matches: create, join, matchmaking, open matches, play, reconnect | `biome-online-services` (feature `match-client`) |
| Public Biome player cards and existing game/mode standings (requires a host with the new public read routes) | `biome-online-services::public_players` (feature `match-client`) |
| The rules SDK: one Rust crate runs in your client and on the server | `biome-rules` |
| A local match host for development | `biome-match-dev` |
| The publishing CLI for your CI | `biome-publish` |
| A rules validator, the same check the server runs | `biome-rules-host` (`validate-rules`) |
| Two complete example games | `rules-tictactoe` (start here), `rules-highcard` (hidden cards) |

Everything is Rust and engine-agnostic. It works with Bevy, macroquad, or a plain window.

Not making a game? Blender add-ons, plugins and tools publish through the same CLI and the same guild page. See [§9 Software products](#9-software-products-add-ons-plugins-tools).

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

1. Ask for **publishing rights**: on your guild's GAMES tab, press "request publishing rights" (docs/GUILD_REQUESTS.md). A Biome admin answers on the request.
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

Queues, open matches and ranked ratings work locally too, with ratings kept in memory:

- `--rating <id>=<rating>` (repeatable) starts a dev user at that ranked rating. The id is the one in `dev:<id>:<name>`.
- `--widen-every-ms 1000` makes ranked search windows widen ten times faster than production.

```sh
biome-match-dev --rules my-game=… --rating a=1800 --rating b=1450 --widen-every-ms 1000
```

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

## 7. Matchmaking, open matches and ranked

Besides private matches by code, players can find each other three ways. Each one ends in the same `Seated` → `Welcome` → `Update` flow as section 6.

**Quick match (queue).** Players wait until the server pairs them. Only players with the same `rules_version`, mode, config bytes and clock are paired. `"ranked"` pairs by rating and changes ratings; any other mode (`"casual"`, `"blitz"`, …) is first come, first served. A mode is 1 to 32 characters of `a-z`, `0-9` and `-`.

```rust
use biome_online_services::match_client::{Entry, MatchClient, MatchEndpoint, MatchEvent};

let client = MatchClient::start(
    MatchEndpoint::new(MATCH_HOST, "my-game", MyGame::VERSION),
    ticket,
    Entry::Queue { mode: "ranked".into(), config: biome_rules::codec::encode(&my_config)?, clock: None },
);

for ev in client.drain() {
    match ev {
        MatchEvent::Queued { mode, waiting, rating } => {
            // "Searching… 3 players in the queue". rating is Some(Rating { rating, deviation, matches }) in ranked.
        }
        MatchEvent::Seated(info) => { /* paired: info.join_code is None; save info.seat_token for Resume */ }
        MatchEvent::Welcome(w) => { /* the match has started; play as usual */ }
        MatchEvent::Failed { code, .. } => { /* queue-timeout, rate-limited, unauthorized, replaced, … */ }
        _ => {}
    }
}
// Cancel: drop the client. It leaves the queue.
drop(client);
```

- Ranked search starts at ±50 rating points, widens by 25 every 10 seconds, and stops at ±400. Both players' windows must include the other.
- Seats are assigned at random.
- A queue lasts up to 10 minutes, then `Failed { code: "queue-timeout" }`. Ask for a new ticket and queue again.
- At most 20 queue joins per player per game every 5 minutes (`rate-limited`).
- Queuing again elsewhere with the same account replaces the first entry (`replaced`).
- Each queue join spends a ticket. If the queue connection drops, get a new ticket and queue again.

**Open matches (lobby browser).** One player opens a listed match and others pick it from a list.

```rust
use biome_online_services::match_client::{list_open, Entry, MatchClient, OpenListing};

// Host: listed for everyone, and it also gets a join code.
let host = MatchClient::start(endpoint.clone(), ticket, Entry::Open { config: cfg_bytes, clock: None });

// Browser: fetch the list in the background, poll each frame.
let listing: OpenListing = list_open(&endpoint);         // public, no ticket
if let Some(result) = listing.poll() {                    // Some exactly once
    for m in result? {                                    // newest first, at most 50
        // m.match_id, m.host (username), m.created_at_ms, m.rules_version, m.config
    }
}
// Join one (needs a ticket):
let client = MatchClient::start(endpoint, ticket, Entry::JoinOpen { match_id: chosen.match_id.clone() });
```

- The list only shows matches of your build's `rules_version`.
- A match leaves the list when it starts, after 30 minutes without an opponent, or when the same host opens another one.
- If someone else got the seat first, the join fails with `match-started` or `match-full`, and with `no-such-match` if the match is gone. Fetch the list again.
- Listed configs are limited to 8 KiB.

**Ratings and leaderboards.** Ranked results update both players' Glicko-2 ratings (start 1500, deviation 350) when the match ends:

- A win scores 1, a draw ½, a loss 0. Resigning or running out of time is a loss.
- Casual matches never change ratings.
- The leaderboard is public. Read it with the Supabase REST API. It needs no sign-in, only Biome's public anon key in an `apikey` header (ask your Biome contact):

```
GET https://peigjeheuktsgfyxeajv.supabase.co/rest/v1/game_leaderboard?app_id=eq.my-game&mode=eq.ranked&order=rank&limit=100
→ [{rank, user_id, username, display_name, avatar_url, rating, deviation, matches, wins, losses, draws, …}]
```

A player's own rating also arrives in `Queued` each time they join the ranked queue.

## 8. Publish a release

**`biome.toml`** at your repo root:

```toml
[game]
app_id = "my-game"

[release]
version = "1.0.0"            # your release version, semver
channel = "stable"           # "internal", "beta" or "stable" (RELEASE)
notes = "release-notes.md"   # optional: a file whose text players see

[rules]                      # for games with hosted matches
wasm = "target/wasm32-unknown-unknown/release/rules_my_game.wasm"

[client.windows-x64]         # one per platform: windows-x64, macos-arm64, macos-x64, linux-x64, web
path = "dist/windows"        # a folder (sent file by file) or a .zip (sent whole)
entrypoint = "MyGame.exe"    # the executable inside, '/'-separated
```

A browser build goes in `[client.web]`: see [Web builds](#web-builds-play-in-the-browser) below.

**Updates download only changed files; ship assets as files, not packed into one archive, to benefit.** A client folder is sent file by file: Biome stores each distinct file of your game once, so a new version uploads only the files that changed (`biome-publish upload` prints "312 of 331 files already on Biome; uploading 19 (34.2 MB)"), and players' desktop libraries download only those when they update. A `.zip` given as `path` is still sent and downloaded whole, and `delivery = "zip"` in a `[client.*]` table zips a folder as before. A client folder holds at most 20,000 files and 4 GiB; file names must be valid on Windows too (no `: * ? " < > |` or `\`, no names like `CON` or `NUL`, no name ending in a dot or a space, no two names that differ only in case).

**Publish token:** on the GAMES tab, open MANAGE, then **publish tokens**, then create. The token is shown once. Store it as a CI secret named `BIOME_PUBLISH_TOKEN`.

```sh
export BIOME_PUBLISH_ENDPOINT=https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-publish
biome-publish check     # offline: manifest, rules validation, file lists, hashes
biome-publish upload    # create the release, upload, submit for review
biome-publish status    # review state of recent releases
```

**Then:**
1. Its files pass **BioShield**, Biome's checks for malicious files (`biome-publish status` shows "BioShield: checking", then "BioShield: checked"; a file BioShield flags rejects the release with "BioShield flagged <file>"). Biome doesn't say which tools BioShield runs or what they found, not even to the publisher; if you think a flag is a mistake, send a bug report naming the release and the file, and a Biome admin takes a second look (`/bioshield`). Then it is approved or rejected with a note: on `beta`, by you or your guild's admins, from the release's row under the game's MANAGE (APPROVE / DECLINE); on `stable` (RELEASE, everyone), by a Biome admin. You see it on the GAMES tab and in `biome-publish status`. An `internal` release skips the review: it goes live for you and your testers as soon as BioShield has checked it. Nobody but a Biome admin approves a release BioShield hasn't passed.
2. Once it's approved, players can install and update your game from the Biome desktop library, as long as the game is **listed**. A listed game also shows up in the website's shop under **GAMES**, with a store page at `https://biome.cafe/products/<app id>` that sends people to the desktop app.

**Release tracks: test with chosen people first.** Every release is on one track:

| track | `channel` | who gets it | review |
|---|---|---|---|
| INTERNAL | `"internal"` | you, your guild's admins, internal testers | none: live once BioShield has checked it |
| BETA | `"beta"` | beta and internal testers | you or your guild's admins, once BioShield has checked it |
| RELEASE | `"stable"` | everyone, once the game is **listed** | a Biome admin |

- While your game isn't listed, nobody but you and your testers gets any build of it, or can play it online.
- **Testers:** on the GAMES tab, MANAGE, **TRACKS & TESTERS**. Add someone who has a Biome account by username, or **invite by email** at a level (internal or beta): they get an email with a link that works once, for 14 days; it asks them to sign in or sign up, then they're in. RESEND sends a fresh link (the old one stops working), REVOKE kills it. Up to 100 internal and 5,000 beta testers, 50 invite emails a day.
- **Promote** a release instead of uploading it again: under TRACKS, → BETA or → RELEASE. → BETA moves a live release at once (your move is the approval). → RELEASE moves at once only a release a Biome admin approved; any other waits for an admin (it stays live where it is meanwhile). Your rules module runs on the match host once the release is approved, on any track: by BioShield's check on INTERNAL, by you or your guild's admins on BETA, by a Biome admin on RELEASE.
- Players get the newest build of their track; a tester can pick a lower track on the game's store page (TRACK card), and the desktop app installs that one. The desktop library shows an INTERNAL or BETA badge on testing builds.
3. Your rules module goes live on the match host by itself, about 10 to 20 minutes after the release is approved: Biome checks for newly approved rules every 10 minutes and redeploys the match host. A module that fails Biome's checks there is left out (the rest of the host keeps running); fix it and publish it under a new `rules_version`. Players in a match when the host redeploys reconnect by themselves.
4. The match host keeps your newest 3 rules versions (by when they were approved) plus the one of each track's current release, so matches in flight and a rollback keep working; older ones drop out. A rules module is at most 4 MiB, and very large modules share a size budget with every other game's (one that doesn't fit is left out and listed as refused). Once a release with a rules module is approved, that `rules_version` keeps that module for good, even if you withdraw the release: a different module under it is refused, so bump the version.

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

### Web builds: play in the browser

Your game can also ship a build that plays in the browser at `https://biome.cafe/play/<app id>` (a Bevy game built for `wasm32-unknown-unknown`, with trunk or wasm-bindgen). Add a table:

```toml
[client.web]                 # next to your desktop builds, or on its own
path = "dist/web"            # the folder trunk / wasm-bindgen wrote
entrypoint = "index.html"    # the default
```

- It is always sent file by file, like a desktop folder; a `.zip` is refused. Same limits (20,000 files, 4 GiB) and the same tracks: a `"beta"` release's web build is for your beta testers, so an invite-only BETA works out of the box.
- The page's query string reaches your game: `biome.cafe/play/<app id>?autoplay` opens your `index.html?autoplay=`.
- Your game only runs inside Biome's page: opening its URL on its own gives "open this game from biome.cafe", and the frame can't open popups or submit forms. It may use `navigator.clipboard` (copy and paste: `clipboard-write` and `clipboard-read` are allowed), `prompt`/`alert`/`confirm`, fullscreen, pointer lock and sound.
- **Use relative URLs** inside it (`./pkg/my_game.js`, `assets/cards.png`; trunk: `--public-url ./`). Biome serves the build under a per-player path, so an absolute `/assets/...` misses.
- The store page shows **PLAY IN BROWSER** to everyone who may play it. The game runs in an iframe on **its own origin, `https://<app id>.biome.news`**, not on biome.cafe and never shared with another game.
- **Your app id must be a host label** for that: 1-63 lowercase letters, digits and `-`, not starting or ending with `-`. `biome-publish check` tells you if it isn't ("can't be played in a browser").
- Files are served compressed; the browser revalidates them (a cheap 304 when unchanged), so a new version never sticks stale.
- `localStorage` and IndexedDB are your game's own (its origin is), and they persist across versions. Don't rely on cookies: every game shares the `biome.news` site, so another game's page could set one for the whole domain.

**Sign-in and tickets in the browser: the ticket bridge.** There is no desktop app in the browser, so the page that frames your game answers instead, over `postMessage`. Send to `window.parent`; the replies come back from it:

```js
// game → page (they carry nothing secret, so "*" as the target origin is fine)
window.parent.postMessage({ biome: "hello", app_id: "my-game" }, "*");
window.parent.postMessage({ biome: "ticket-request", app_id: "my-game", id: 1 }, "*");

// page → game
window.addEventListener("message", (e) => {
  if (e.source !== window.parent || !e.data || typeof e.data.biome !== "string") return;
  // {biome: "identity", name: "mandy", user_id: "<public account UUID>"}
  // or {biome: "identity", error: "signed-out"}; older hosts omit user_id
  // {biome: "ticket", id: 1, ticket: "<signed token>"} or {biome: "ticket", id: 1, error: "not-invited"}
});
```

- Post each message as an object or as the same object in a JSON string (what `kernel_link` sends); both are accepted. Replies come as objects.
- Send `hello` every second until the `identity` reply arrives: the page may still be starting when your game is.
- `app_id` must be your game's; anything else is ignored.
- `id` is your own counter (a whole number); the reply carries it back.
- A ticket is the same signed token the desktop app hands out (five minutes, one match): pass it to the match client exactly as on desktop. At most 10 ticket requests a minute.
- Your game never sees the player's Biome login. In Rust with `wasm-bindgen`/`web-sys`, post with `window.parent()?.post_message(&msg, "*")` and listen with a `message` event listener.

## 9. Software products (add-ons, plugins, tools)

A software product is anything people download and use with their own tools: a Blender add-on, a plugin, a script pack, a tool. A release is 1 to 10 files of any type (`.zip`, `.py`, `.blend`, `.pdf`, …). Your guild needs the same publishing rights as for games.

1. On your guild page, open **SOFTWARE**, then **+ NEW PRODUCT**. Pick an app id (same rules as games; it becomes the page `https://biome.cafe/products/<app id>`).
2. **MANAGE**, then **EDIT** the store page: name, summary, description, cover, price (leave it blank for free), license, requirements (e.g. "Blender 4.2+"), tags, homepage. Tick **listed in the shop**.
3. Upload a release. Either:
   - **From the website:** MANAGE, then **+ NEW RELEASE**. Version, channel, what's new, **+ ADD FILES**. Each file gets a published name (fix it if your file name has spaces), a platform (usually *any system*) and a label ("Add-on", "Manual"). **UPLOAD & SUBMIT** hashes the files in your browser, uploads them straight to Biome's storage and submits the release.
   - **From CI** with `biome-publish` and a `[product]` manifest (below).
4. A Biome admin reviews the release. Once it's approved, it shows up in the shop under **SOFTWARE**, and signed-in users download it from the product page. Free products download for anyone signed in. **Checkout isn't live yet:** a paid product shows its price and a "coming soon" buy button; meanwhile give people access by hand under MANAGE, **ACCESS** (comps, testers, early backers).

**`biome.toml` for a Blender add-on** (Blender 4.2+ extension):

```toml
[product]
app_id = "node-wrangler"

[release]
version = "1.2.0"
channel = "stable"            # or "beta"
notes = "CHANGELOG.md"        # optional

# The zip `blender --command extension build` made, uploaded as is.
[[download]]
path = "dist/node_wrangler-1.2.0.zip"
label = "Add-on (Blender 4.2+)"

# Or point at the extension's folder and let biome-publish zip it
# (deterministically, folder contents at the zip's root, as Blender 4.2 wants):
# [[download]]
# path = "node_wrangler"
# file_name = "node_wrangler-1.2.0.zip"
# label = "Add-on (Blender 4.2+)"

[[download]]
path = "docs/manual.pdf"
label = "Manual"
```

- `[[download]]` keys: `path` (a file, uploaded as is, or a folder, zipped), `platform` (`any` by default, or `windows-x64`, `macos-arm64`, `macos-x64`, `linux-x64` for platform-specific files), `label` (optional), `file_name` (optional: publish under another name; a folder's must end in `.zip`).
- Published names are letters, digits, `.`, `_` and `-`, starting with a letter or digit. `biome-publish check` suggests a `file_name` when yours doesn't fit.
- A legacy (pre-4.2) add-on zip must contain its package folder (`my_addon/__init__.py`). Zip it yourself, or point `path` at a folder that *contains* `my_addon/`.
- No `[rules]` or `[client.*]` in a `[product]` manifest. Same token, `check`, `upload` and `status` as for games.
- A `[product]` manifest also publishes a **shop file listing** (a guild shop item with files: print files, documents, pictures, audio, video). The server knows which kind your app id is. A file listing takes plain files and zips of them (a folder `[[download]]` is zipped for you). It is live as soon as it is submitted when everything in it is a plain file; a zip holding a program, a script, another archive or an unsafe path waits for a site admin, and `upload` prints why. Any other file is refused with a note to publish it as software.

```sh
export BIOME_PUBLISH_TOKEN=bpt_...    # MANAGE, then publish tokens
biome-publish check
biome-publish upload
biome-publish status
```

## 10. Reference

| | |
|---|---|
| Match host | `https://biome-match.aerodeskio.workers.dev` |
| Publish endpoint | `https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-publish` |
| Ticket public keys (to verify tickets yourself) | `https://peigjeheuktsgfyxeajv.supabase.co/functions/v1/game-ticket/jwks` |
| Tickets | ES256 JWT, `aud` = your app id, 5 minutes, one create / join / rejoin each |
| Join codes | 6 characters, valid 2 hours. 10 wrong guesses per player per 5 minutes, then a pause |
| Lobbies | a match nobody joined is abandoned after 30 minutes |
| Queue | 10 minutes at most; 20 joins per player per game per 5 minutes |
| Ranked | Glicko-2, start 1500 ± 350; search ±50, +25 per 10 s, up to ±400 |
| Open matches | at most 50 listed per rules version, newest first; config ≤ 8 KiB; one listing per host |
| Release files | client zip ≤ 4 GiB per platform, rules module ≤ 4 MiB, at most 5 releases uploading at once |
| Testers | 100 internal, 5,000 beta per game; invite links work once, for 14 days; 50 invite emails a day per game |
| Software releases | 1 to 10 files of any type, each ≤ 4 GiB |

**Sign-in errors** (`BosError`):
- `PlatformNotInstalled`: Biome isn't installed.
- `PlatformUnavailable`: Biome is installed but not running or not reachable.
- `NotSignedIn`: Biome is running but nobody is signed in.
- `ConsentDenied` or `ConsentTimeout`: the player said no, or didn't answer.
- `UnknownApp`: the app id isn't registered, or the game is turned off.
- `Request("… not invited …")`: the game isn't listed and this account is neither one of its makers nor an invited tester.
- `HandshakeRejected(code)`: any other refusal, for example `backend-unavailable` when Biome couldn't reach its servers.

**Match errors:**
- From your rules: the codes you return in `Reject`.
- From the host: `not-started`, `game-over`, `bad-move`, `out-of-time` and `rules-error`, plus `rules-version-mismatch`, `bad-seat-token`, `replaced` (a newer connection took the seat), `abandoned` and `match-fault` (the server gave the match up).
- From the queue (as `Failed`): `queue-timeout`, `rate-limited`, `replaced`, `unauthorized`, `bad-config`, `bad-mode`, `unknown-rules`, `match-failed`, `backend-unavailable`.

## 11. Help

Ask your Biome contact. Include your app id, the `rules_version`, and the match id if one is involved. `cargo test --workspace` in this repo runs every SDK test. The examples in `crates/biome-online-services/examples` show sign-in (`attach_demo`) and a full match bot (`tictactoe_bot`).
