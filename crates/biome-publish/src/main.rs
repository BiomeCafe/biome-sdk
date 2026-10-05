//! # biome-publish
//!
//! Publishes a game release to Biome from the game's own repository,
//! usually from its CI. Contract: `docs/GAME_PUBLISHING_SPEC.md` §3 (the
//! `game-publish` API), §4 (storage) and §7 (this tool).
//!
//! ## Manifest
//!
//! `biome.toml` next to the game's workspace; paths are relative to it:
//!
//! ```toml
//! [game]
//! app_id = "mecha-aether"
//!
//! [release]
//! version = "0.3.0"          # semver MAJOR.MINOR.PATCH[-pre], required
//! channel = "beta"           # stable | beta, default stable
//! notes = "release-notes.md" # optional: a file whose text becomes the notes (max 4000 chars)
//!
//! [rules]                    # optional, for games the match host runs
//! wasm = "target/wasm32-unknown-unknown/release/mecha_rules.wasm"
//!
//! [client.windows-x64]       # one table per platform: windows-x64, macos-arm64, macos-x64, linux-x64
//! path = "dist/windows"      # a folder (zipped by the CLI) or a .zip (uploaded as is)
//! entrypoint = "MechaAether.exe"  # the executable inside the archive, / separated
//! ```
//!
//! ## Commands
//!
//! ```text
//! biome-publish check  [--manifest biome.toml] [--out-dir DIR]
//! biome-publish upload [--manifest biome.toml] [--out-dir DIR] [--endpoint URL]
//! biome-publish status [--manifest biome.toml] [--endpoint URL]
//! ```
//!
//! - `check` validates the manifest, loads the rules module with the match
//!   host's own validator (`biome_rules_host::validate`) and prints its
//!   meta, zips each client folder deterministically (sorted entries, fixed
//!   timestamps, so the same folder always gives the same sha256), checks
//!   the entrypoint is in the archive, and prints sizes and hashes. No
//!   network. Zips go to `--out-dir`, default
//!   `<temp>/biome-publish/<app_id>/<version>/`, named
//!   `<app_id>-<version>-<platform>.zip`.
//! - `upload` runs `check`, creates the release, streams every file to the
//!   presigned URL the server returns (with exactly the headers it
//!   returns), then submits the release for review. Re-running it after a
//!   failed upload resumes the release while it is still `uploading`.
//! - `status` lists the game's 20 most recent releases and their review
//!   state.
//!
//! The publish token comes from `BIOME_PUBLISH_TOKEN` (a guild admin mints
//! it for one game; keep it in a CI secret). It is never printed. The
//! endpoint is `--endpoint`, else `BIOME_PUBLISH_ENDPOINT`, else
//! `<SUPABASE_URL>/functions/v1/game-publish` with the `SUPABASE_URL` this
//! binary was built with (the local stack, `http://127.0.0.1:54321`, if
//! none). Plain `http` is only accepted for loopback hosts.
//!
//! ## CI example
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown -p mecha-rules
//! ./build-clients.sh                       # fills dist/windows, dist/linux, ...
//! BIOME_PUBLISH_TOKEN=${{ secrets.BIOME_PUBLISH_TOKEN }} biome-publish upload
//! ```
//!
//! ## Exit codes
//!
//! | code | meaning |
//! |---|---|
//! | 0 | ok |
//! | 1 | validation failed (manifest, files, module, token or endpoint missing) |
//! | 2 | the server or storage refused (its `error` and `message` are printed) |
//! | 3 | network or local I/O error |

fn main() {
    let env = biome_publish::Env::from_process();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = biome_publish::run(std::env::args_os(), &env, &mut out, &mut err);
    std::process::exit(code);
}
