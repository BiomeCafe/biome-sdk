//! `validate-rules --dir cloudflare/biome-match/rules`
//!
//! The deploy-time gate for the match host's bundle: every
//! `<dir>/<app_id>/<rules_version>.wasm` must load in the reference host
//! (`biome_rules_host::validate`: no imports, the ABI v1 exports with the
//! right signatures, sane meta, bounded memory) and must say about itself
//! the `rules_version` its file name claims. The TypeScript host cannot
//! check export signatures, so nothing reaches `wrangler deploy` without
//! passing this. Exit code 0 when all pass, 1 otherwise.
//!
//! Also accepts plain file arguments: `validate-rules a.wasm b.wasm`.
//!
//! `--quarantine LIST`: LIST holds `app/version` lines (what sync-rules
//! fetched from approved releases this run). A listed module that fails is
//! deleted and reported with a `::error::` line instead of failing the run,
//! so one bad release does not block every deploy. Modules not listed (the
//! ones committed to the repository) still fail the run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn app_id_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (3..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// `(app_id, version from the file name, path)` for every module under `dir`.
fn modules_in(dir: &Path) -> Result<Vec<(String, String, PathBuf)>, String> {
    let mut out = Vec::new();
    let apps = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for app in apps.flatten() {
        if !app.path().is_dir() {
            continue;
        }
        let app_id = app.file_name().to_string_lossy().to_string();
        for file in std::fs::read_dir(app.path()).map_err(|e| format!("{}: {e}", app.path().display()))?.flatten() {
            let name = file.file_name().to_string_lossy().to_string();
            if let Some(version) = name.strip_suffix(".wasm") {
                out.push((app_id.clone(), version.to_string(), file.path()));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn check(app: Option<&str>, version: Option<&str>, path: &Path) -> Result<String, String> {
    if let Some(app) = app {
        if !app_id_ok(app) {
            return Err(format!("folder name {app:?} is not a valid app id"));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let meta = biome_rules_host::validate(&bytes).map_err(|e| e.to_string())?;
    if let Some(v) = version {
        if meta.rules_version != v {
            return Err(format!("the file says {v} but the module says rules_version {}", meta.rules_version));
        }
    }
    Ok(format!(
        "{} {} (seats {}..={}, {} bytes)",
        meta.name, meta.rules_version, meta.min_seats, meta.max_seats, bytes.len()
    ))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut targets: Vec<(Option<String>, Option<String>, PathBuf)> = Vec::new();
    let mut quarantine: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--quarantine" {
            let Some(list) = args.get(i + 1) else {
                eprintln!("--quarantine needs a file");
                return ExitCode::from(1);
            };
            // A missing list means nothing was fetched.
            if let Ok(text) = std::fs::read_to_string(list) {
                quarantine.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string));
            }
            i += 2;
        } else if args[i] == "--dir" {
            let Some(dir) = args.get(i + 1) else {
                eprintln!("--dir needs a folder");
                return ExitCode::from(1);
            };
            match modules_in(Path::new(dir)) {
                Ok(found) => targets.extend(found.into_iter().map(|(a, v, p)| (Some(a), Some(v), p))),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(1);
                }
            }
            i += 2;
        } else if args[i] == "-h" || args[i] == "--help" {
            println!("usage: validate-rules [--dir RULES_DIR] [FILE.wasm ...]");
            return ExitCode::SUCCESS;
        } else {
            targets.push((None, None, PathBuf::from(&args[i])));
            i += 1;
        }
    }
    if targets.is_empty() {
        eprintln!("nothing to validate (usage: validate-rules [--dir RULES_DIR] [FILE.wasm ...])");
        return ExitCode::from(1);
    }
    let mut failed = 0;
    for (app, version, path) in &targets {
        match check(app.as_deref(), version.as_deref(), path) {
            Ok(summary) => println!("ok    {}  {summary}", path.display()),
            Err(e) => {
                let key = match (app, version) {
                    (Some(a), Some(v)) => format!("{a}/{v}"),
                    _ => String::new(),
                };
                if !key.is_empty() && quarantine.contains(&key) {
                    // A module that failed must not stay in the bundle: if it
                    // can't be removed, the run fails.
                    match std::fs::remove_file(path) {
                        Ok(()) if !path.exists() => {
                            println!("::error::rules {key} quarantined (removed from the bundle): {e}");
                        }
                        Ok(()) => {
                            failed += 1;
                            println!("FAIL  {}  {e}; and it is still there after removal", path.display());
                        }
                        Err(rm) => {
                            failed += 1;
                            println!("FAIL  {}  {e}; and it could not be removed: {rm}", path.display());
                        }
                    }
                } else {
                    failed += 1;
                    println!("FAIL  {}  {e}", path.display());
                }
            }
        }
    }
    if failed > 0 {
        eprintln!("{failed} of {} rules module(s) failed validation", targets.len());
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
