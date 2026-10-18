//! # biome-publish
//!
//! Publishes a release to Biome from the product's own repository, usually
//! from its CI: a game, or a software product (an add-on, a plugin, a
//! tool). Contract: `docs/GAME_PUBLISHING_SPEC.md` §3 (the `game-publish`
//! API), §4 (storage) and §7 (this tool); `docs/SOFTWARE_PRODUCTS_SPEC.md`
//! for software.
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
//! channel = "beta"           # internal | beta | stable (RELEASE), default stable
//! notes = "release-notes.md" # optional: a file whose text becomes the notes (max 4000 chars)
//!
//! [rules]                    # optional, for games the match host runs
//! wasm = "target/wasm32-unknown-unknown/release/mecha_rules.wasm"
//!
//! [client.windows-x64]       # one table per platform: windows-x64, macos-arm64, macos-x64, linux-x64, web
//! path = "dist/windows"      # a folder (sent file by file) or a .zip (uploaded as is)
//! entrypoint = "MechaAether.exe"  # the executable inside the build, / separated
//! # delivery = "zip"         # optional: zip the folder and send it whole instead
//!
//! [client.web]               # optional: plays in the browser at biome.cafe/play/<app_id>
//! path = "dist/web"          # a folder (trunk / wasm-bindgen output), always file by file
//! entrypoint = "index.html"  # default; use relative URLs inside it (./pkg/game.js)
//! ```
//!
//! A client folder is sent file by file (spec §10): each distinct file is
//! an object the game stores once on Biome, so a new version uploads only the
//! files that changed ("312 of 331 files already on Biome; uploading 19
//! (34.2 MB)") and players' updates download only those. Ship assets as
//! files, not packed into one archive, to benefit.
//!
//! A software product, or a shop file listing, has `[product]` instead of
//! `[game]`, no `[rules]` or `[client.*]`, and 1 to 10 downloads. The
//! server knows which kind the app id is: software takes any file type and
//! is reviewed; a file listing takes plain files (documents, pictures, 3D
//! and print files, audio, video) and zips of them, and goes live once
//! BioShield has checked its files when the server's look inside finds
//! nothing that can run (else it waits for a site admin, and `upload` prints
//! why). Every release's files pass BioShield; `status` shows its check:
//!
//! ```toml
//! [product]
//! app_id = "node-wrangler"
//!
//! [release]
//! version = "1.2.0"
//!
//! [[download]]
//! path = "dist/node_wrangler-1.2.0.zip"  # a file (uploaded as is) or a folder (zipped, contents at the root)
//! platform = "any"                       # default; or windows-x64, macos-arm64, macos-x64, linux-x64
//! label = "Add-on"                       # optional, shown next to the file
//! # file_name = "..."                    # optional: publish under another name
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
//!   meta, hashes each client folder's files and checks their names (or,
//!   with `delivery = "zip"`, zips it deterministically: sorted entries,
//!   fixed timestamps, so the same folder always gives the same sha256),
//!   checks the entrypoint is in the build, and prints sizes, hashes and
//!   build ids. No network. Zips go to `--out-dir`, default
//!   `<temp>/biome-publish/<app_id>/<version>/`, named
//!   `<app_id>-<version>-<platform>.zip`.
//! - `upload` runs `check`, creates the release, streams every file to the
//!   presigned URL the server returns (with exactly the headers it
//!   returns), then submits the release for review. For a per-file client
//!   it compresses (deterministic gzip, in `--out-dir/objects`) and uploads
//!   only the files Biome doesn't have, 8 at a time. Re-running it after a
//!   failed upload resumes the release while it is still `uploading`; files
//!   that made it are not sent again.
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
