//! The deploy gate (`validate-rules`) against the checked-in bundle and
//! against modules that must be refused.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_validate-rules"))
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("validate-rules-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_checked_in_bundle_passes() {
    let out = bin().arg("--dir").arg(repo().join("cloudflare/biome-match/rules")).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("tictactoe 1.0.0"), "{stdout}");
    assert!(stdout.contains("highcard 1.0.0"), "{stdout}");
}

#[test]
fn a_misnamed_or_broken_module_fails_the_run() {
    let dir = temp("bad");
    let good = std::fs::read(repo().join("cloudflare/biome-match/rules/tictactoe/1.0.0.wasm")).unwrap();
    // Named as a version it is not.
    std::fs::create_dir_all(dir.join("tictactoe")).unwrap();
    std::fs::write(dir.join("tictactoe/2.0.0.wasm"), &good).unwrap();
    // Not a module at all.
    std::fs::create_dir_all(dir.join("junk-game")).unwrap();
    std::fs::write(dir.join("junk-game/1.0.0.wasm"), b"\0asm nope").unwrap();
    // A folder that is not an app id.
    std::fs::create_dir_all(dir.join("Bad_Name")).unwrap();
    std::fs::write(dir.join("Bad_Name/1.0.0.wasm"), &good).unwrap();

    let out = bin().arg("--dir").arg(&dir).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success());
    assert!(stdout.contains("the module says rules_version 1.0.0"), "{stdout}");
    assert_eq!(stdout.matches("FAIL").count(), 3, "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetched_modules_that_fail_are_quarantined_committed_ones_fail_the_run() {
    let dir = temp("quarantine");
    let good = std::fs::read(repo().join("cloudflare/biome-match/rules/tictactoe/1.0.0.wasm")).unwrap();
    std::fs::create_dir_all(dir.join("tictactoe")).unwrap();
    std::fs::write(dir.join("tictactoe/1.0.0.wasm"), &good).unwrap();
    std::fs::create_dir_all(dir.join("fetched-game")).unwrap();
    std::fs::write(dir.join("fetched-game/1.0.0.wasm"), b"\0asm nope").unwrap();
    let list = dir.join("fetched.txt");
    std::fs::write(&list, "fetched-game/1.0.0\n").unwrap();

    let out = bin().arg("--dir").arg(&dir).arg("--quarantine").arg(&list).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("::error::rules fetched-game/1.0.0 quarantined"), "{stdout}");
    assert!(!dir.join("fetched-game/1.0.0.wasm").exists(), "the bad module is removed");
    assert!(dir.join("tictactoe/1.0.0.wasm").exists());

    // A quarantined module that can't be removed fails the run (a folder in
    // its place: validation fails, and remove_file refuses a directory).
    std::fs::create_dir_all(dir.join("fetched-game/1.0.0.wasm")).unwrap();
    let out = bin().arg("--dir").arg(&dir).arg("--quarantine").arg(&list).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("could not be removed"), "{stdout}");
    std::fs::remove_dir_all(dir.join("fetched-game/1.0.0.wasm")).unwrap();

    // The same failure in a module that was NOT fetched fails the run.
    std::fs::write(dir.join("fetched-game/1.0.0.wasm"), b"\0asm nope").unwrap();
    let out = bin().arg("--dir").arg(&dir).arg("--quarantine").arg(dir.join("empty.txt")).output().unwrap();
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_to_check_is_an_error() {
    let dir = temp("empty");
    assert!(!bin().arg("--dir").arg(&dir).output().unwrap().status.success());
    assert!(!bin().output().unwrap().status.success());
    let _ = std::fs::remove_dir_all(&dir);
}
