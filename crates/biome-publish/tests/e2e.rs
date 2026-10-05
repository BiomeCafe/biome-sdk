//! End to end: the real CLI (`biome_publish::run`) against an in-process
//! mock of the `game-publish` edge function (spec §3) and of the storage
//! behind its presigned PUT URLs.
//!
//! The mock checks what the real services would: the bearer token on API
//! calls (and its absence on presigned PUTs), the signed `content-length`
//! and `x-amz-checksum-sha256` headers against the body actually sent,
//! every other returned header echoed exactly, and at submit time that each
//! stored object has the size and sha256 declared at create time.

use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use base64::Engine as _;
use biome_publish::{run, Env, EXIT_INVALID, EXIT_IO, EXIT_OK, EXIT_REFUSED};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TOKEN: &str = "bpt_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const APP: &str = "tic-tac-toe";
const VERSION: &str = "1.2.0-beta.1";

// ------------------------------------------------------------------- mock

#[derive(Default)]
struct Mock {
    base: String,
    releases: Vec<Release>,
    objects: HashMap<String, Vec<u8>>,
    create_statuses: Vec<u16>,
    puts: Vec<PutRecord>,
    /// Refuse the next PUT whose key contains this (as an expired URL).
    fail_next_put: Option<String>,
    /// Answer PUTs with 200 but keep nothing.
    drop_puts: bool,
}

#[derive(Debug, Clone)]
struct PutRecord {
    key: String,
    sha256: String,
    had_authorization: bool,
}

struct Release {
    id: String,
    seq: usize,
    version: String,
    channel: String,
    notes: String,
    rules: Option<Value>,
    clients: Value,
    status: String,
    files: Vec<FileSpec>,
}

struct FileSpec {
    kind: &'static str,
    platform: String,
    key: String,
    size: u64,
    sha256: String,
}

type Shared = Arc<Mutex<Mock>>;

fn json_error(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

fn xml_error(status: u16, code: &str, message: &str) -> Response {
    (StatusCode::from_u16(status).unwrap(), format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>{message}</Message></Error>"))
        .into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(|v| v == format!("Bearer {TOKEN}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn b64_of_hex(hex: &str) -> String {
    let raw: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap()).collect();
    base64::engine::general_purpose::STANDARD.encode(raw)
}

async fn create(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers) {
        return json_error(401, json!({"error": "unauthorized"}));
    }
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return json_error(400, json!({"error": "bad-request", "message": "body is not JSON"}));
    };
    if req["app_id"] != APP {
        return json_error(403, json!({"error": "wrong-app"}));
    }
    let version = req["version"].as_str().unwrap_or_default().to_string();
    let channel = req["channel"].as_str().unwrap_or_default().to_string();
    let Some(notes) = req["notes"].as_str().map(str::to_string) else {
        return json_error(400, json!({"error": "bad-request", "message": "notes must be a string"}));
    };
    let clients = req["clients"].clone();
    let Some(list) = clients.as_array().filter(|a| !a.is_empty()) else {
        return json_error(400, json!({"error": "bad-request", "message": "clients needs at least one entry"}));
    };
    let rules = req.get("rules").cloned();

    let mut files = Vec::new();
    if let Some(r) = &rules {
        if !r["rules_version"].is_string() {
            return json_error(400, json!({"error": "bad-request", "message": "rules.rules_version"}));
        }
        files.push(FileSpec {
            kind: "rules",
            platform: "any".into(),
            key: format!("{APP}/{version}/rules.wasm"),
            size: r["size"].as_u64().unwrap(),
            sha256: r["sha256"].as_str().unwrap().into(),
        });
    }
    for c in list {
        let platform = c["platform"].as_str().unwrap().to_string();
        files.push(FileSpec {
            kind: "client",
            key: format!("{APP}/{version}/{platform}/{}", c["file_name"].as_str().unwrap()),
            platform,
            size: c["size"].as_u64().unwrap(),
            sha256: c["sha256"].as_str().unwrap().into(),
        });
        assert!(c["entrypoint"].is_string());
    }

    let mut m = s.lock().unwrap();
    let base = m.base.clone();
    let idx = match m.releases.iter().position(|r| r.version == version) {
        Some(i) => {
            let r = &m.releases[i];
            if r.status != "uploading" || r.rules != rules || r.clients != clients {
                let status = r.status.clone();
                m.create_statuses.push(409);
                return json_error(409, json!({"error": "version-exists", "status": status}));
            }
            m.create_statuses.push(200);
            i
        }
        None => {
            let seq = m.releases.len() + 1;
            m.releases.push(Release {
                id: format!("00000000-0000-4000-8000-{seq:012}"),
                seq,
                version,
                channel,
                notes,
                rules,
                clients,
                status: "uploading".into(),
                files,
            });
            m.create_statuses.push(201);
            m.releases.len() - 1
        }
    };
    let status = if *m.create_statuses.last().unwrap() == 201 { StatusCode::CREATED } else { StatusCode::OK };
    let r = &m.releases[idx];
    let uploads: Vec<Value> = r
        .files
        .iter()
        .map(|f| {
            json!({
                "kind": f.kind,
                "platform": f.platform,
                "method": "PUT",
                "url": format!("{base}/r2/{}?X-Amz-Expires=3600&X-Amz-Signature=mock", f.key),
                "headers": {
                    "content-length": f.size.to_string(),
                    "x-amz-checksum-sha256": b64_of_hex(&f.sha256),
                    "x-mock-signed": "yes",
                },
                "expires_at": 1_791_162_000u64,
            })
        })
        .collect();
    (status, Json(json!({"release_id": r.id, "status": "uploading", "uploads": uploads}))).into_response()
}

async fn put_object(State(s): State<Shared>, UrlPath(key): UrlPath<String>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let mut m = s.lock().unwrap();
    if !uri.query().unwrap_or_default().contains("X-Amz-Signature=mock") {
        return xml_error(403, "AccessDenied", "missing signature");
    }
    let sha = sha256_hex(&body);
    let had_authorization = headers.contains_key(header::AUTHORIZATION);
    m.puts.push(PutRecord { key: key.clone(), sha256: sha.clone(), had_authorization });
    if had_authorization {
        return xml_error(400, "InvalidArgument", "Only one auth mechanism allowed");
    }
    let lengths: Vec<_> = headers.get_all(header::CONTENT_LENGTH).iter().filter_map(|v| v.to_str().ok()).collect();
    if lengths != [body.len().to_string()] {
        return xml_error(400, "IncompleteBody", &format!("content-length {lengths:?} for {} bytes", body.len()));
    }
    if headers.get("x-amz-checksum-sha256").and_then(|v| v.to_str().ok()) != Some(b64_of_hex(&sha).as_str()) {
        return xml_error(400, "BadDigest", "The sha256 you specified did not match the calculated checksum.");
    }
    if headers.get("x-mock-signed").and_then(|v| v.to_str().ok()) != Some("yes") {
        return xml_error(403, "SignatureDoesNotMatch", "a signed header is missing");
    }
    if m.fail_next_put.as_deref().is_some_and(|f| key.contains(f)) {
        m.fail_next_put = None;
        return xml_error(403, "AccessDenied", "Request has expired");
    }
    if !m.drop_puts {
        m.objects.insert(key, body.to_vec());
    }
    StatusCode::OK.into_response()
}

async fn submit(State(s): State<Shared>, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return json_error(401, json!({"error": "unauthorized"}));
    }
    let mut guard = s.lock().unwrap();
    let m = &mut *guard;
    let Some(r) = m.releases.iter_mut().find(|r| r.id == id) else {
        return json_error(404, json!({"error": "not-found"}));
    };
    if r.status != "uploading" {
        return json_error(409, json!({"error": "not-uploading", "status": r.status}));
    }
    let missing: Vec<Value> = r
        .files
        .iter()
        .filter(|f| match m.objects.get(&f.key) {
            Some(b) => b.len() as u64 != f.size || sha256_hex(b) != f.sha256,
            None => true,
        })
        .map(|f| json!({"kind": f.kind, "platform": f.platform}))
        .collect();
    if !missing.is_empty() {
        return json_error(409, json!({"error": "upload-incomplete", "missing": missing}));
    }
    r.status = "submitted".into();
    Json(json!({"status": "submitted"})).into_response()
}

async fn list(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return json_error(401, json!({"error": "unauthorized"}));
    }
    if q.get("app_id").map(String::as_str) != Some(APP) {
        return json_error(403, json!({"error": "wrong-app"}));
    }
    let m = s.lock().unwrap();
    let mut rows: Vec<&Release> = m.releases.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.seq));
    let rows: Vec<Value> = rows
        .into_iter()
        .take(20)
        .map(|r| {
            json!({
                "release_id": r.id,
                "version": r.version,
                "channel": r.channel,
                "status": r.status,
                "review_note": null,
                "created_at": format!("2026-10-04T12:00:{:02}Z", r.seq),
                "submitted_at": (r.status == "submitted").then(|| format!("2026-10-04T12:01:{:02}Z", r.seq)),
                "reviewed_at": null,
            })
        })
        .collect();
    Json(Value::Array(rows)).into_response()
}

struct Server {
    state: Shared,
    endpoint: String,
}

impl Server {
    fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state: Shared = Arc::new(Mutex::new(Mock { base: base.clone(), ..Default::default() }));
        let app = Router::new()
            .route("/functions/v1/game-publish/releases", post(create).get(list))
            .route("/functions/v1/game-publish/releases/{id}/submit", post(submit))
            .route("/r2/{*key}", put(put_object))
            .layer(DefaultBodyLimit::disable())
            .with_state(state.clone());
        // Its own thread and runtime: the CLI under test uses a blocking
        // client, which must not run inside an async context.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        Server { state, endpoint: format!("{base}/functions/v1/game-publish") }
    }

    fn mock(&self) -> MutexGuard<'_, Mock> {
        self.state.lock().unwrap()
    }
}

// ---------------------------------------------------------------- fixture

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    manifest: PathBuf,
    out_dir: PathBuf,
}

fn demo_rules() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cloudflare/biome-match/rules/tictactoe/1.0.0.wasm")
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A game repo: rules module, a Windows build folder, a ready-made Linux
/// zip, release notes and the manifest.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write(&root.join("rules/tictactoe.wasm"), &fs::read(demo_rules()).unwrap());
    write(&root.join("dist/windows/TicTacToe.exe"), b"MZ not really an exe");
    write(&root.join("dist/windows/data/board.json"), br#"{"size":3}"#);
    write(&root.join("build/linux/bin/tictactoe"), b"\x7fELF not really an elf");
    write(&root.join("build/linux/share/board.json"), br#"{"size":3}"#);
    let entries = biome_publish::pack::list_folder(&root.join("build/linux")).unwrap();
    biome_publish::pack::zip_folder(&entries, "bin/tictactoe", &root.join("dist/linux.zip")).unwrap();
    write(&root.join("NOTES.md"), b"\nBigger board.\n");
    let manifest = root.join("biome.toml");
    write(
        &manifest,
        format!(
            r#"
[game]
app_id = "{APP}"

[release]
version = "{VERSION}"
channel = "beta"
notes = "NOTES.md"

[rules]
wasm = "rules/tictactoe.wasm"

[client.windows-x64]
path = "dist/windows"
entrypoint = "TicTacToe.exe"

[client.linux-x64]
path = "dist/linux.zip"
entrypoint = "bin/tictactoe"
"#
        )
        .as_bytes(),
    );
    let out_dir = root.join("out");
    Fixture { _dir: dir, root, manifest, out_dir }
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn cli(fx: &Fixture, endpoint: &str, token: Option<&str>, command: &str) -> Run {
    let env = Env { token: token.map(str::to_string), endpoint: None, progress: false };
    let mut args = vec!["biome-publish".to_string(), command.to_string(), "--manifest".into(), fx.manifest.display().to_string()];
    if command != "check" {
        args.extend(["--endpoint".to_string(), endpoint.to_string()]);
    }
    if command != "status" {
        args.extend(["--out-dir".to_string(), fx.out_dir.display().to_string()]);
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(args, &env, &mut out, &mut err);
    let run = Run { code, out: String::from_utf8(out).unwrap(), err: String::from_utf8(err).unwrap() };
    if let Some(token) = token {
        assert!(!run.out.contains(token) && !run.err.contains(token), "the token was printed:\n{}\n{}", run.out, run.err);
    }
    run
}

fn key(platform: &str) -> String {
    format!("{APP}/{VERSION}/{platform}/{APP}-{VERSION}-{platform}.zip")
}

fn rules_key() -> String {
    format!("{APP}/{VERSION}/rules.wasm")
}

// ------------------------------------------------------------------ tests

#[test]
fn check_needs_no_network_and_prints_hashes() {
    let fx = fixture();
    // No token, unroutable endpoint: check must not care.
    let r = cli(&fx, "http://127.0.0.1:9/never", None, "check");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    let linux_sha = sha256_hex(&fs::read(fx.root.join("dist/linux.zip")).unwrap());
    let windows_sha = sha256_hex(&fs::read(fx.out_dir.join(format!("{APP}-{VERSION}-windows-x64.zip"))).unwrap());
    let rules_sha = sha256_hex(&fs::read(demo_rules()).unwrap());
    for needle in [&linux_sha, &windows_sha, &rules_sha, &"rules_version 1.0.0".to_string(), &"check ok".to_string()] {
        assert!(r.out.contains(needle.as_str()), "missing {needle}:\n{}", r.out);
    }

    // Same folder, same zip: a second check reproduces the hash.
    let again = cli(&fx, "", None, "check");
    assert_eq!(again.code, EXIT_OK);
    assert!(again.out.contains(&windows_sha));

    // A broken manifest is a validation failure.
    fs::write(&fx.manifest, "[game]\napp_id = \"x\"\n[release]\nversion = \"1\"\n").unwrap();
    let bad = cli(&fx, "", None, "check");
    assert_eq!(bad.code, EXIT_INVALID, "{}", bad.err);
    assert!(bad.err.contains("app_id") && bad.err.contains("version"), "{}", bad.err);
}

#[test]
fn upload_creates_puts_every_file_and_submits() {
    let server = Server::start();
    let fx = fixture();
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    assert!(r.out.contains("created release") && r.out.contains("is submitted"), "{}", r.out);

    let m = server.mock();
    assert_eq!(m.create_statuses, [201]);
    let mut keys: Vec<_> = m.puts.iter().map(|p| p.key.clone()).collect();
    keys.sort();
    let mut expected = vec![rules_key(), key("linux-x64"), key("windows-x64")];
    expected.sort();
    assert_eq!(keys, expected);
    assert!(m.puts.iter().all(|p| !p.had_authorization), "the token went to storage");

    // What arrived is byte for byte what is on disk.
    assert_eq!(m.objects[&rules_key()], fs::read(demo_rules()).unwrap());
    assert_eq!(m.objects[&key("linux-x64")], fs::read(fx.root.join("dist/linux.zip")).unwrap(), "a given zip is uploaded as is");
    let built = fx.out_dir.join(format!("{APP}-{VERSION}-windows-x64.zip"));
    assert_eq!(m.objects[&key("windows-x64")], fs::read(&built).unwrap());
    let names = biome_publish::pack::zip_file_names(&built).unwrap();
    assert_eq!(names, ["TicTacToe.exe", "data/board.json"]);

    // And the release carries the module's own rules_version and the notes file.
    let rel = &m.releases[0];
    assert_eq!(rel.status, "submitted");
    assert_eq!(rel.channel, "beta");
    assert_eq!(rel.notes, "Bigger board.");
    assert_eq!(rel.rules.as_ref().unwrap()["rules_version"], "1.0.0");
    let entrypoints: Vec<_> = rel.clients.as_array().unwrap().iter().map(|c| c["entrypoint"].as_str().unwrap().to_string()).collect();
    assert_eq!(entrypoints, ["bin/tictactoe", "TicTacToe.exe"]);
    let shas: HashMap<_, _> = m.puts.iter().map(|p| (p.key.clone(), p.sha256.clone())).collect();
    assert_eq!(shas[&key("windows-x64")], sha256_hex(&fs::read(&built).unwrap()));
}

#[test]
fn a_failed_upload_resumes_on_the_next_run() {
    let server = Server::start();
    let fx = fixture();
    server.mock().fail_next_put = Some("windows-x64".into());

    let first = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(first.code, EXIT_REFUSED, "{}\n{}", first.out, first.err);
    assert!(first.err.contains("AccessDenied") && first.err.contains("Request has expired"), "{}", first.err);
    assert!(!first.err.contains("X-Amz-Signature"), "presigned URL leaked into the error: {}", first.err);
    assert_eq!(server.mock().releases[0].status, "uploading");

    let second = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(second.code, EXIT_OK, "{}\n{}", second.out, second.err);
    assert!(second.out.contains("resuming release 00000000-0000-4000-8000-000000000001"), "{}", second.out);
    let m = server.mock();
    assert_eq!(m.create_statuses, [201, 200]);
    assert_eq!(m.releases.len(), 1);
    assert_eq!(m.releases[0].status, "submitted");
}

#[test]
fn an_existing_version_is_refused_with_exit_2() {
    let server = Server::start();
    let fx = fixture();
    assert_eq!(cli(&fx, &server.endpoint, Some(TOKEN), "upload").code, EXIT_OK);
    let puts_before = server.mock().puts.len();

    let again = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(again.code, EXIT_REFUSED, "{}", again.err);
    assert!(again.err.contains("409 version-exists"), "{}", again.err);
    assert!(again.err.contains("existing release status: submitted"), "{}", again.err);
    let m = server.mock();
    assert_eq!(m.create_statuses, [201, 409]);
    assert_eq!(m.puts.len(), puts_before, "nothing uploaded after a refusal");
}

#[test]
fn a_bad_token_is_refused_with_exit_2() {
    let server = Server::start();
    let fx = fixture();
    let wrong = "bpt_this-token-was-revoked-long-ago-0000000000";
    let r = cli(&fx, &server.endpoint, Some(wrong), "upload");
    assert_eq!(r.code, EXIT_REFUSED, "{}", r.err);
    assert!(r.err.contains("401 unauthorized"), "{}", r.err);
    assert!(server.mock().puts.is_empty());

    let s = cli(&fx, &server.endpoint, Some(wrong), "status");
    assert_eq!(s.code, EXIT_REFUSED, "{}", s.err);
}

#[test]
fn missing_objects_are_refused_at_submit() {
    let server = Server::start();
    let fx = fixture();
    server.mock().drop_puts = true;
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_REFUSED, "{}", r.err);
    assert!(r.err.contains("409 upload-incomplete"), "{}", r.err);
    assert!(r.err.contains("missing upload: client windows-x64") && r.err.contains("missing upload: rules any"), "{}", r.err);
}

#[test]
fn status_lists_recent_releases() {
    let server = Server::start();
    let fx = fixture();
    let empty = cli(&fx, &server.endpoint, Some(TOKEN), "status");
    assert_eq!(empty.code, EXIT_OK, "{}", empty.err);
    assert!(empty.out.contains("no releases yet"), "{}", empty.out);

    assert_eq!(cli(&fx, &server.endpoint, Some(TOKEN), "upload").code, EXIT_OK);
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "status");
    assert_eq!(r.code, EXIT_OK, "{}", r.err);
    let row = r.out.lines().find(|l| l.starts_with(VERSION)).unwrap_or_else(|| panic!("{}", r.out));
    assert!(row.contains("beta") && row.contains("submitted") && row.contains("2026-10-04T12:01:01Z"), "{row}");
    assert!(r.out.contains("VERSION") && r.out.contains("REVIEW NOTE"), "{}", r.out);
}

#[test]
fn an_unreachable_server_is_exit_3() {
    let fx = fixture();
    // A port that was free a moment ago and has nothing listening now.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let endpoint = format!("http://127.0.0.1:{port}/functions/v1/game-publish");
    let r = cli(&fx, &endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_IO, "{}\n{}", r.out, r.err);
    assert!(r.err.contains("network error"), "{}", r.err);
}

#[test]
fn upload_refuses_plain_http_to_remote_hosts() {
    let fx = fixture();
    let r = cli(&fx, "http://example.com/functions/v1/game-publish", Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_INVALID, "{}", r.err);
    assert!(r.err.contains("plain http"), "{}", r.err);
}
