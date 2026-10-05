//! Command line: `check`, `upload`, `status`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::api::{self, Api, ReleaseSummary};
use crate::error::{Error, EXIT_INVALID, EXIT_OK};
use crate::manifest::Manifest;
use crate::prepare::{self, Prepared};

#[derive(Parser, Debug)]
#[command(
    name = "biome-publish",
    version,
    about = "Publish a game release to Biome: validate it, upload it, submit it for review.",
    after_help = "The publish token is read from BIOME_PUBLISH_TOKEN.\n\
                  Exit codes: 0 ok, 1 validation failed, 2 server refused, 3 network or I/O error."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Validate the manifest, the rules module and the client builds; print sizes and hashes. No network.
    Check(BuildArgs),
    /// Check, then create the release, upload every file and submit it for review.
    Upload {
        #[command(flatten)]
        build: BuildArgs,
        #[command(flatten)]
        remote: RemoteArgs,
    },
    /// The game's recent releases and their review state.
    Status {
        /// Path to the manifest.
        #[arg(long, default_value = "biome.toml")]
        manifest: PathBuf,
        #[command(flatten)]
        remote: RemoteArgs,
    },
}

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Path to the manifest.
    #[arg(long, default_value = "biome.toml")]
    pub manifest: PathBuf,
    /// Folder for the client zips the CLI builds [default: <temp>/biome-publish/<app_id>/<version>].
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RemoteArgs {
    /// URL of the game-publish function [default: BIOME_PUBLISH_ENDPOINT, else <SUPABASE_URL>/functions/v1/game-publish].
    #[arg(long)]
    pub endpoint: Option<String>,
}

/// What the process environment provides. Tests build one by hand instead
/// of setting variables.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `BIOME_PUBLISH_TOKEN`.
    pub token: Option<String>,
    /// `BIOME_PUBLISH_ENDPOINT`.
    pub endpoint: Option<String>,
    /// Print upload percentages to stderr.
    pub progress: bool,
}

impl Env {
    pub fn from_process() -> Env {
        Env {
            token: std::env::var(api::TOKEN_ENV).ok(),
            endpoint: std::env::var(api::ENDPOINT_ENV).ok(),
            progress: true,
        }
    }
}

/// Run the CLI with `args` (including the program name). Normal output goes
/// to `out`, errors to `err`. Returns the exit code.
pub fn run<I, T>(args: I, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            let text = e.render().to_string();
            return match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    let _ = write!(out, "{text}");
                    EXIT_OK
                }
                _ => {
                    let _ = write!(err, "{text}");
                    EXIT_INVALID
                }
            };
        }
    };
    let result = match cli.command {
        Command::Check(build) => check(&build, out).map(|_| ()),
        Command::Upload { build, remote } => upload(&build, &remote, env, out),
        Command::Status { manifest, remote } => status(&manifest, &remote, env, out),
    };
    match result {
        Ok(()) => EXIT_OK,
        Err(e) => {
            let _ = out.flush();
            let _ = writeln!(err, "error: {e}");
            e.exit_code()
        }
    }
}

fn check(args: &BuildArgs, out: &mut dyn Write) -> Result<Prepared, Error> {
    let manifest = Manifest::load(&args.manifest)?;
    let out_dir = args.out_dir.clone().unwrap_or_else(|| prepare::default_out_dir(&manifest));
    let _ = writeln!(out, "manifest  {}", args.manifest.display());
    let _ = writeln!(out, "game      {}", manifest.app_id);
    let _ = writeln!(out, "release   {} ({})", manifest.version, manifest.channel.as_str());
    let prepared = prepare::prepare(&manifest, &out_dir, out)?;
    print_prepared(&manifest, &prepared, out);
    let _ = writeln!(out, "check ok");
    Ok(prepared)
}

fn print_prepared(m: &Manifest, p: &Prepared, out: &mut dyn Write) {
    match &m.notes {
        Some(path) => {
            let _ = writeln!(out, "notes     {} characters from {}", p.notes.chars().count(), shown(&m.dir, path));
        }
        None => {
            let _ = writeln!(out, "notes     (none)");
        }
    }
    match &p.rules {
        Some(r) => {
            let meta = &r.meta;
            let _ = writeln!(out, "rules     {}", shown(&m.dir, &r.path));
            let _ = writeln!(
                out,
                "          module {:?}, rules_version {}, abi {}, seats {}-{}",
                meta.name, meta.rules_version, meta.abi, meta.min_seats, meta.max_seats
            );
            let _ = writeln!(out, "          {}  sha256 {}", size(r.size), r.sha256);
        }
        None => {
            let _ = writeln!(out, "rules     (none: not a hosted game)");
        }
    }
    for c in &p.clients {
        let _ = writeln!(out, "client    {}", c.platform);
        match c.zipped_files {
            Some(n) => {
                let _ = writeln!(out, "          zipped {} ({n} files) -> {}", shown(&m.dir, &c.source), c.archive.display());
            }
            None => {
                let _ = writeln!(out, "          zip {}", shown(&m.dir, &c.source));
            }
        }
        let _ = writeln!(out, "          upload as {}, entrypoint {}", c.file_name, c.entrypoint);
        let _ = writeln!(out, "          {}  sha256 {}", size(c.size), c.sha256);
    }
}

fn upload(build: &BuildArgs, remote: &RemoteArgs, env: &Env, out: &mut dyn Write) -> Result<(), Error> {
    // Configuration problems first: no point zipping gigabytes without a token.
    let endpoint = api::resolve_endpoint(remote.endpoint.as_deref(), env.endpoint.as_deref())?;
    let token = api::check_token(env.token.as_deref())?;
    let prepared = check(build, out)?;
    let api = Api::new(endpoint, token, env.progress)?;

    let _ = writeln!(out, "creating release {} {} at {}", prepared.app_id, prepared.version, api.endpoint());
    let (status, created) = api.create_release(&prepared.request())?;
    if status == 200 {
        let _ = writeln!(out, "resuming release {} (still uploading)", created.release_id);
    } else {
        let _ = writeln!(out, "created release {}", created.release_id);
    }

    // What we have, by (kind, platform).
    let mut files: HashMap<(String, String), (&Path, u64, String)> = HashMap::new();
    if let Some(r) = &prepared.rules {
        files.insert(("rules".into(), "any".into()), (r.path.as_path(), r.size, "rules".to_string()));
    }
    for c in &prepared.clients {
        files.insert(("client".into(), c.platform.clone()), (c.archive.as_path(), c.size, format!("client {}", c.platform)));
    }
    let mut sent = Vec::new();
    for up in &created.uploads {
        let target = up.target();
        let Some((path, len, label)) = files.get(&target) else {
            return Err(Error::bad_response(format!("the server asked for an upload of {} {} that this release does not have", target.0, target.1)));
        };
        if sent.contains(&target) {
            return Err(Error::bad_response(format!("the server asked for {label} twice")));
        }
        let _ = writeln!(out, "uploading {label} ({}) ...", size(*len));
        let _ = out.flush();
        api.upload(up, path, *len, label, out)?;
        sent.push(target);
    }
    for (target, (_, _, label)) in &files {
        if !sent.contains(target) {
            let _ = writeln!(out, "note: the server did not ask for {label}; submitting without uploading it");
        }
    }

    let _ = writeln!(out, "submitting release {} ...", created.release_id);
    let submitted = api.submit(&created.release_id)?;
    let _ = writeln!(
        out,
        "{} {} is {}: a site admin reviews it next (`biome-publish status` shows the result)",
        prepared.app_id, prepared.version, submitted.status
    );
    Ok(())
}

fn status(manifest_path: &Path, remote: &RemoteArgs, env: &Env, out: &mut dyn Write) -> Result<(), Error> {
    let endpoint = api::resolve_endpoint(remote.endpoint.as_deref(), env.endpoint.as_deref())?;
    let token = api::check_token(env.token.as_deref())?;
    let manifest = Manifest::load(manifest_path)?;
    let api = Api::new(endpoint, token, false)?;
    let releases = api.list(&manifest.app_id)?;
    if releases.is_empty() {
        let _ = writeln!(out, "{}: no releases yet", manifest.app_id);
        return Ok(());
    }
    let _ = writeln!(out, "{}: {} most recent releases", manifest.app_id, releases.len());
    let _ = write!(out, "{}", status_table(&releases));
    Ok(())
}

/// Releases as an aligned text table.
pub fn status_table(releases: &[ReleaseSummary]) -> String {
    let dash = |s: &Option<String>| s.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "-".into());
    let mut rows = vec![["VERSION", "CHANNEL", "STATUS", "CREATED", "SUBMITTED", "REVIEWED", "REVIEW NOTE"].map(String::from)];
    for r in releases {
        rows.push([
            r.version.clone(),
            r.channel.clone(),
            r.status.clone(),
            dash(&r.created_at),
            dash(&r.submitted_at),
            dash(&r.reviewed_at),
            dash(&r.review_note).replace(['\r', '\n'], " "),
        ]);
    }
    let mut widths = [0usize; 7];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in &rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<w$}  ", w = widths[i]));
            }
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

fn shown(base: &Path, path: &Path) -> String {
    path.strip_prefix(base).unwrap_or(path).display().to_string().replace('\\', "/")
}

/// `412337664 bytes (393.2 MiB)`.
pub fn size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{bytes} bytes ({:.1} MiB)", bytes as f64 / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_capture(args: &[&str]) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(args.iter().copied(), &Env::default(), &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn help_and_usage_errors() {
        let (code, out, _) = run_capture(&["biome-publish", "--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("upload") && out.contains("BIOME_PUBLISH_TOKEN"), "{out}");
        let (code, _, err) = run_capture(&["biome-publish", "launch"]);
        assert_eq!(code, EXIT_INVALID, "{err}");
        let (code, _, _) = run_capture(&["biome-publish"]);
        assert_eq!(code, EXIT_INVALID);
    }

    #[test]
    fn upload_without_a_token_fails_before_any_work() {
        let (code, _, err) = run_capture(&["biome-publish", "upload", "--manifest", "does-not-matter.toml", "--endpoint", "http://127.0.0.1:9/x"]);
        assert_eq!(code, EXIT_INVALID);
        assert!(err.contains("BIOME_PUBLISH_TOKEN is not set"), "{err}");
    }

    #[test]
    fn table_is_aligned() {
        let rows = vec![
            ReleaseSummary {
                version: "1.10.0".into(),
                channel: "stable".into(),
                status: "approved".into(),
                created_at: Some("2026-10-01".into()),
                review_note: Some("looks good\nship it".into()),
                ..Default::default()
            },
            ReleaseSummary { version: "1.9.0".into(), channel: "beta".into(), status: "rejected".into(), ..Default::default() },
        ];
        let t = status_table(&rows);
        let lines: Vec<_> = t.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("VERSION  CHANNEL  STATUS    CREATED"), "{t}");
        assert!(lines[1].contains("looks good ship it"), "{t}");
        assert_eq!(lines[1].find("approved"), lines[2].find("rejected"), "{t}");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(10), "10 bytes");
        assert_eq!(size(412_337_664), "412337664 bytes (393.2 MiB)");
    }
}
