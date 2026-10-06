//! Command line: `check`, `upload`, `status`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use clap::{Args, Parser, Subcommand};

use crate::api::{self, Api, ObjectMeta, ReleaseSummary, Upload};
use crate::error::{Error, EXIT_INVALID, EXIT_OK};
use crate::files;
use crate::manifest::{Kind, Manifest};
use crate::prepare::{self, ClientBuild, Prepared};

#[derive(Parser, Debug)]
#[command(
    name = "biome-publish",
    version,
    about = "Publish a release to Biome: a game ([game] in biome.toml), or a software product such as an add-on or plugin, or a shop file listing ([product]; the server knows which). Validate it, upload it, submit it (for review, or live after its virus scan for a file listing of plain files and zips of them).",
    after_help = "biome.toml: [game] + [release] + [client.<platform>] (+ [rules]), or [product] + [release] + 1-10 [[download]].\n\
                  A client folder is sent file by file: a new version uploads only the files that changed.\n\
                  The publish token is read from BIOME_PUBLISH_TOKEN.\n\
                  Exit codes: 0 ok, 1 validation failed, 2 server refused, 3 network or I/O error."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Validate the manifest and its files (rules module and client builds, or downloads); print sizes and hashes. No network.
    Check(BuildArgs),
    /// Check, then create the release, upload every file and submit it for review.
    Upload {
        #[command(flatten)]
        build: BuildArgs,
        #[command(flatten)]
        remote: RemoteArgs,
    },
    /// The game's or product's recent releases and their review state.
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
    /// Folder for the zips the CLI builds and the files it compresses for upload [default: <temp>/biome-publish/<app_id>/<version>].
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

/// What `check` built, and the folder it used for its zips (where `upload`
/// puts the compressed objects too).
fn check(args: &BuildArgs, out: &mut dyn Write) -> Result<(Prepared, PathBuf), Error> {
    let manifest = Manifest::load(&args.manifest)?;
    let out_dir = args.out_dir.clone().unwrap_or_else(|| prepare::default_out_dir(&manifest));
    let _ = writeln!(out, "manifest  {}", args.manifest.display());
    match manifest.kind {
        Kind::Game => {
            let _ = writeln!(out, "game      {}", manifest.app_id);
        }
        Kind::Software => {
            let _ = writeln!(out, "product   {} (software or file listing: the server knows which)", manifest.app_id);
        }
    }
    let _ = writeln!(out, "release   {} ({})", manifest.version, manifest.channel.as_str());
    let prepared = prepare::prepare(&manifest, &out_dir, out)?;
    print_prepared(&manifest, &prepared, out);
    let _ = writeln!(out, "check ok");
    Ok((prepared, out_dir))
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
    match (&p.rules, m.kind) {
        (Some(r), _) => {
            let meta = &r.meta;
            let _ = writeln!(out, "rules     {}", shown(&m.dir, &r.path));
            let _ = writeln!(
                out,
                "          module {:?}, rules_version {}, abi {}, seats {}-{}",
                meta.name, meta.rules_version, meta.abi, meta.min_seats, meta.max_seats
            );
            let _ = writeln!(out, "          {}  sha256 {}", size(r.size), r.sha256);
        }
        (None, Kind::Game) => {
            let _ = writeln!(out, "rules     (none: not a hosted game)");
        }
        (None, Kind::Software) => {}
    }
    for d in &p.downloads {
        match &d.label {
            Some(label) => {
                let _ = writeln!(out, "download  {} ({label})", d.platform);
            }
            None => {
                let _ = writeln!(out, "download  {}", d.platform);
            }
        }
        match d.zipped_files {
            Some(n) => {
                let _ = writeln!(out, "          zipped {} ({n} files) -> {}", shown(&m.dir, &d.source), d.file.display());
            }
            None => {
                let _ = writeln!(out, "          file {}", shown(&m.dir, &d.source));
            }
        }
        let _ = writeln!(out, "          upload as {}", d.file_name);
        let _ = writeln!(out, "          {}  sha256 {}", size(d.size), d.sha256);
    }
    for c in &p.clients {
        let _ = writeln!(out, "client    {}", c.platform);
        match &c.build {
            ClientBuild::Zip(z) => {
                match z.zipped_files {
                    Some(n) => {
                        let _ = writeln!(out, "          zipped {} ({n} files) -> {}", shown(&m.dir, &c.source), z.archive.display());
                    }
                    None => {
                        let _ = writeln!(out, "          zip {}", shown(&m.dir, &c.source));
                    }
                }
                let _ = writeln!(out, "          upload as {}, entrypoint {}", z.file_name, c.entrypoint);
                let _ = writeln!(out, "          {}  sha256 {}", size(z.size), z.sha256);
            }
            ClientBuild::Files(list) => {
                let _ = writeln!(
                    out,
                    "          {} ({} files), sent file by file; entrypoint {}",
                    shown(&m.dir, &c.source),
                    list.files.len(),
                    c.entrypoint
                );
                let _ = writeln!(out, "          {}  build id {}", size(list.total), list.build_id);
            }
        }
    }
}

fn upload(build: &BuildArgs, remote: &RemoteArgs, env: &Env, out: &mut dyn Write) -> Result<(), Error> {
    // Configuration problems first: no point zipping gigabytes without a token.
    let endpoint = api::resolve_endpoint(remote.endpoint.as_deref(), env.endpoint.as_deref())?;
    let token = api::check_token(env.token.as_deref())?;
    let (prepared, out_dir) = check(build, out)?;
    let api = Api::new(endpoint, token, env.progress)?;

    let _ = writeln!(out, "creating release {} {} at {}", prepared.app_id, prepared.version, api.endpoint());
    let mut body = prepared.request();
    let (status, created) = api.create_release(&body)?;
    if status == 200 {
        let _ = writeln!(out, "resuming release {} (still uploading)", created.release_id);
    } else {
        let _ = writeln!(out, "created release {}", created.release_id);
    }

    // Whole files we have, by what the server calls them (`Upload::target`).
    let mut files: HashMap<(String, String), (&Path, u64, String)> = HashMap::new();
    if let Some(r) = &prepared.rules {
        files.insert(("rules".into(), "any".into()), (r.path.as_path(), r.size, "rules".to_string()));
    }
    for c in &prepared.clients {
        if let Some(z) = c.zip() {
            files.insert(("client".into(), c.platform.clone()), (z.archive.as_path(), z.size, format!("client {}", c.platform)));
        }
    }
    for d in &prepared.downloads {
        files.insert(("download".into(), d.file_name.clone()), (d.file.as_path(), d.size, format!("download {}", d.file_name)));
    }
    let mut sent = Vec::new();
    for up in created.uploads.iter().filter(|u| u.kind != "object") {
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

    // Per-file clients: only the objects Biome doesn't have yet.
    let objects_dir = out_dir.join("objects");
    if prepared.clients.iter().any(|c| c.files().is_some()) {
        let summary = created.objects.clone().ok_or_else(|| {
            Error::bad_response("the server did not answer with the release's objects; it may be too old for per-file clients (set delivery = \"zip\")")
        })?;
        upload_objects(&api, &prepared, &mut body, &summary, &objects_dir, out)?;
    }

    let _ = writeln!(out, "submitting release {} ...", created.release_id);
    let submitted = api.submit(&created.release_id)?;
    let _ = std::fs::remove_dir_all(&objects_dir);
    if submitted.status == "approved" {
        // A file listing (plain files, or zips of them) goes live on submit.
        let _ = writeln!(out, "{} {} is approved: a file listing is live at once", prepared.app_id, prepared.version);
    } else if !submitted.reasons.is_empty() {
        // A file listing whose files were looked at and held.
        let _ = writeln!(out, "{} {} is waiting for a site admin's review:", prepared.app_id, prepared.version);
        for reason in &submitted.reasons {
            let _ = writeln!(out, "  - {reason}");
        }
        if submitted.scan.as_deref() == Some("pending") {
            let _ = writeln!(out, "Its files are being scanned for viruses too.");
        }
        let _ = writeln!(out, "(`biome-publish status` shows the result)");
    } else if submitted.after_scan.as_deref() == Some("live") {
        let _ = writeln!(
            out,
            "{} {} is being scanned for viruses: a file listing goes live once its files scan clean (`biome-publish status` shows the result)",
            prepared.app_id, prepared.version
        );
    } else if submitted.scan.as_deref() == Some("pending") {
        let _ = writeln!(
            out,
            "{} {} is {}: its files are being scanned for viruses, then a site admin reviews it (`biome-publish status` shows the result)",
            prepared.app_id, prepared.version, submitted.status
        );
    } else {
        let _ = writeln!(
            out,
            "{} {} is {}: a site admin reviews it next (`biome-publish status` shows the result)",
            prepared.app_id, prepared.version, submitted.status
        );
    }
    Ok(())
}

/// One object ready to PUT: its gzip on disk.
struct Compressed {
    path: PathBuf,
    stored_size: u64,
    stored_sha256: String,
    /// A path of a file that holds it, for messages.
    label: String,
}

/// Upload threads at once.
const PUT_THREADS: usize = 8;

/// The per-file part of `upload`: compress only the objects the server
/// says it is missing, describe them (a resume of the same release), PUT
/// what it signs, in parallel, and ask again while it has more to sign.
fn upload_objects(
    api: &Api,
    prepared: &Prepared,
    body: &mut api::CreateRelease,
    summary: &api::ObjectsSummary,
    dir: &Path,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let objects = prepared.objects();
    let missing: BTreeSet<String> = summary.missing.iter().map(|m| m.sha256.clone()).collect();
    if let Some(stray) = missing.iter().find(|s| !objects.contains_key(*s)) {
        return Err(Error::bad_response(format!("the server asked for an object {stray} that this release does not have")));
    }
    let todo: Vec<(&String, &files::ContentFile)> = missing.iter().filter_map(|s| objects.get(s).map(|(_, f)| (s, *f))).collect();
    if !todo.is_empty() {
        let _ = writeln!(out, "compressing {} new {} ...", todo.len(), if todo.len() == 1 { "file" } else { "files" });
    }
    let compressed = compress_all(&todo, dir)?;
    let bytes: u64 = compressed.values().map(|c| c.stored_size).sum();
    print_have_line(prepared, &missing, compressed.len(), bytes, out);
    if compressed.is_empty() {
        return Ok(());
    }

    body.objects = compressed
        .iter()
        .map(|(sha, c)| ObjectMeta { sha256: sha.clone(), stored_size: c.stored_size, stored_sha256: c.stored_sha256.clone() })
        .collect();
    body.uploaded.clear();
    let mut done = 0usize;
    loop {
        let (_, again) = api.create_release(body)?;
        let batch: Vec<&Upload> = again.uploads.iter().filter(|u| u.kind == "object").collect();
        put_objects(api, &batch, &compressed, out)?;
        done += batch.len();
        let rest = again.objects.unwrap_or_default();
        if rest.unsigned == 0 {
            break;
        }
        if batch.is_empty() {
            return Err(Error::bad_response(format!("the server signed no uploads for the {} objects it is missing", rest.missing.len())));
        }
        let _ = writeln!(out, "uploaded {done} of {}; asking for the rest ...", compressed.len());
        body.uploaded = batch.iter().filter_map(|u| u.sha256.clone()).collect();
    }
    let _ = writeln!(out, "uploaded {done} {} ({})", if done == 1 { "file" } else { "files" }, mb(bytes));
    Ok(())
}

/// Compress each object once, on a few threads, into `dir/<sha256>.gz`,
/// checking the file still has the content it was listed with.
fn compress_all(todo: &[(&String, &files::ContentFile)], dir: &Path) -> Result<BTreeMap<String, Compressed>, Error> {
    if todo.is_empty() {
        return Ok(BTreeMap::new());
    }
    std::fs::create_dir_all(dir).map_err(|e| Error::io(format!("cannot create {}: {e}", dir.display())))?;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    let chunk = todo.len().div_ceil(threads).max(1);
    let parts: Vec<Result<Vec<(String, Compressed)>, Error>> = std::thread::scope(|s| {
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|(sha, f)| {
                            let path = dir.join(format!("{sha}.gz"));
                            let g = files::gzip_file(&f.source, &path)?;
                            if g.sha256 != **sha || g.size != f.size {
                                return Err(Error::invalid(format!("{} changed while it was being published; run upload again", f.source.display())));
                            }
                            Ok(((*sha).clone(), Compressed { path, stored_size: g.stored_size, stored_sha256: g.stored_sha256, label: f.path.clone() }))
                        })
                        .collect::<Result<Vec<_>, Error>>()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(Error::io("a compression thread stopped")))).collect()
    });
    let mut out = BTreeMap::new();
    for p in parts {
        out.extend(p?);
    }
    Ok(out)
}

/// PUT a batch of objects, [`PUT_THREADS`] at a time. The first failure
/// stops the rest from starting and is returned.
fn put_objects(api: &Api, batch: &[&Upload], compressed: &BTreeMap<String, Compressed>, out: &mut dyn Write) -> Result<(), Error> {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let verbose = batch.len() <= 50;
    let workers = batch.len().min(PUT_THREADS);
    let results: Vec<(Vec<u8>, Result<(), Error>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut log = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        let Some(u) = batch.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                        let target = u.target().1;
                        let Some(c) = compressed.get(&target) else {
                            stop.store(true, Ordering::Relaxed);
                            return (log, Err(Error::bad_response(format!("the server asked for an object {target} that this release does not have"))));
                        };
                        if verbose {
                            let _ = writeln!(log, "uploading {} ({}) ...", c.label, size(c.stored_size));
                        }
                        if let Err(e) = api.upload(u, &c.path, c.stored_size, &c.label, &mut log) {
                            stop.store(true, Ordering::Relaxed);
                            return (log, Err(e));
                        }
                    }
                    (log, Ok(()))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| (Vec::new(), Err(Error::io("an upload thread stopped"))))).collect()
    });
    let mut first = None;
    for (log, r) in results {
        let _ = out.write_all(&log);
        if let Err(e) = r {
            if first.is_none() {
                first = Some(e);
            }
        }
    }
    let _ = out.flush();
    first.map_or(Ok(()), Err)
}

/// "312 of 331 files already on Biome; uploading 19 (34.2 MB)", one line per
/// per-file client when there are several.
fn print_have_line(prepared: &Prepared, missing: &BTreeSet<String>, n: usize, bytes: u64, out: &mut dyn Write) {
    let lists: Vec<(&str, &files::FileList)> = prepared.clients.iter().filter_map(|c| c.files().map(|f| (c.platform.as_str(), f))).collect();
    let plan = if n == 0 { "nothing to upload".to_string() } else { format!("uploading {n} ({})", mb(bytes)) };
    let have = |l: &files::FileList| l.files.iter().filter(|f| !missing.contains(&f.sha256)).count();
    if let [(_, l)] = lists.as_slice() {
        let _ = writeln!(out, "{} of {} files already on Biome; {plan}", have(l), l.files.len());
    } else {
        for (platform, l) in &lists {
            let _ = writeln!(out, "{platform}: {} of {} files already on Biome", have(l), l.files.len());
        }
        let _ = writeln!(out, "{plan}");
    }
}

/// `34.2 MB` (binary megabytes, as the desktop library shows them).
pub fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
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
    let mut rows = vec![["VERSION", "CHANNEL", "STATUS", "VIRUS SCAN", "CREATED", "SUBMITTED", "REVIEWED", "REVIEW NOTE"].map(String::from)];
    for r in releases {
        rows.push([
            r.version.clone(),
            r.channel.clone(),
            r.status.clone(),
            dash(&r.scan_note.clone().or_else(|| r.scan.clone())).replace(['\r', '\n'], " "),
            dash(&r.created_at),
            dash(&r.submitted_at),
            dash(&r.reviewed_at),
            dash(&r.review_note).replace(['\r', '\n'], " "),
        ]);
    }
    let mut widths = [0usize; 8];
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
        assert!(out.contains("[product]") && out.contains("[[download]]"), "software is in the help: {out}");
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
        assert!(lines[0].starts_with("VERSION  CHANNEL  STATUS    VIRUS SCAN  CREATED"), "{t}");
        assert!(lines[1].contains("looks good ship it"), "{t}");
        assert_eq!(lines[1].find("approved"), lines[2].find("rejected"), "{t}");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(10), "10 bytes");
        assert_eq!(size(412_337_664), "412337664 bytes (393.2 MiB)");
    }
}
