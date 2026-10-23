//! End to end: the real CLI (`biome_publish::run`) against an in-process
//! mock of the `game-publish` edge function (spec §3) and of the storage
//! behind its presigned PUT URLs.
//!
//! The mock checks what the real services would: the bearer token on API
//! calls (and its absence on presigned PUTs), the signed `content-length`
//! and `x-amz-checksum-sha256` headers against the body actually sent,
//! every other returned header echoed exactly, and at submit time that each
//! stored object has the size and sha256 declared at create time.
//!
//! Per-file clients (spec §10) as game-publish does them: the objects a game
//! stores (once, across versions and platforms), `objects.missing`, a PUT
//! for each described missing object up to a signing cap, the `uploaded`
//! hint checked against storage, and at submit every unverified object in
//! storage at its described size, a gzip that unpacks to its sha256.

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
/// A software product (the same token publishes it, in this mock).
const SW_APP: &str = "node-wrangler";
/// A file listing: plain files only, approved on submit.
const FILE_APP: &str = "benchy-v2-k3x9qa";

/// The database's plain-file list (is_plain_file), for the mock.
fn is_plain(name: &str) -> bool {
    const PLAIN: [&str; 28] = [
        "pdf", "txt", "md", "csv", "epub", "png", "jpg", "jpeg", "webp", "gif", "tif", "tiff", "stl", "3mf", "obj", "ply",
        "glb", "gltf", "fbx", "step", "stp", "gcode", "mp3", "wav", "ogg", "flac", "mp4", "webm",
    ];
    name.rsplit_once('.').is_some_and(|(_, ext)| PLAIN.contains(&ext.to_ascii_lowercase().as_str()))
}

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
    /// Per-file delivery: what each game stores, by (app, sha256).
    store: HashMap<(String, String), Stored>,
    /// Object PUTs per answer (game-publish's SIGN_CAP); 0 = 2000.
    sign_cap: usize,
    /// The `uploaded` hint of each create.
    hints: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
struct Stored {
    size: u64,
    stored_size: Option<u64>,
    stored_sha256: Option<String>,
    uploaded: bool,
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
    app: String,
    version: String,
    channel: String,
    notes: String,
    rules: Option<Value>,
    clients: Value,
    downloads: Value,
    status: String,
    files: Vec<FileSpec>,
    /// Per-file clients' files: (platform, path, size, sha256).
    contents: Vec<(String, String, u64, String)>,
}

struct FileSpec {
    kind: &'static str,
    platform: String,
    file_name: String,
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
    let app = req["app_id"].as_str().unwrap_or_default().to_string();
    if app != APP && app != SW_APP && app != FILE_APP {
        return json_error(403, json!({"error": "wrong-app"}));
    }
    let version = req["version"].as_str().unwrap_or_default().to_string();
    let channel = req["channel"].as_str().unwrap_or_default().to_string();
    let Some(notes) = req["notes"].as_str().map(str::to_string) else {
        return json_error(400, json!({"error": "bad-request", "message": "notes must be a string"}));
    };
    let clients = req.get("clients").cloned().unwrap_or(Value::Null);
    let downloads = req.get("downloads").cloned().unwrap_or(Value::Null);
    let rules = req.get("rules").cloned();
    // Like the database: a game sends clients, a file listing downloads, a
    // software product downloads and/or desktop clients (never rules or web).
    let software = app == SW_APP || app == FILE_APP;
    let web_client = clients.as_array().is_some_and(|l| l.iter().any(|c| c["platform"] == "web"));
    let fits = if app == FILE_APP {
        downloads.is_array() && !clients.is_array() && rules.is_none()
    } else if app == SW_APP {
        (downloads.is_array() || clients.is_array()) && rules.is_none() && !web_client
    } else {
        clients.is_array() && !downloads.is_array()
    };
    if !fits {
        return json_error(400, json!({"error": "wrong-kind", "message": "body does not fit the product"}));
    }

    let mut files = Vec::new();
    if let Some(r) = &rules {
        if !r["rules_version"].is_string() {
            return json_error(400, json!({"error": "bad-request", "message": "rules.rules_version"}));
        }
        files.push(FileSpec {
            kind: "rules",
            platform: "any".into(),
            file_name: "rules.wasm".into(),
            key: format!("{app}/{version}/rules.wasm"),
            size: r["size"].as_u64().unwrap(),
            sha256: r["sha256"].as_str().unwrap().into(),
        });
    }
    if software && downloads.is_array() {
        let Some(list) = downloads.as_array().filter(|a| !a.is_empty() && a.len() <= 10) else {
            return json_error(400, json!({"error": "bad-request", "message": "downloads needs 1-10 entries"}));
        };
        if app == FILE_APP {
            // Plain files, or zips (looked inside at submit).
            let bad: Vec<&str> = list
                .iter()
                .filter_map(|d| d["file_name"].as_str())
                .filter(|n| !is_plain(n) && !n.to_ascii_lowercase().ends_with(".zip"))
                .collect();
            if !bad.is_empty() {
                return json_error(
                    400,
                    json!({"error": "not-plain-file", "message": "a file listing takes plain files only: publish it as a software product", "files": bad}),
                );
            }
        }
        for d in list {
            let file_name = d["file_name"].as_str().unwrap().to_string();
            files.push(FileSpec {
                kind: "download",
                platform: d["platform"].as_str().unwrap().to_string(),
                key: format!("{app}/{version}/files/{file_name}"),
                file_name,
                size: d["size"].as_u64().unwrap(),
                sha256: d["sha256"].as_str().unwrap().into(),
            });
            assert!(d.get("entrypoint").is_none(), "downloads have no entrypoint");
        }
    }
    let mut contents: Vec<(String, String, u64, String)> = Vec::new();
    if clients.is_array() {
        let Some(list) = clients.as_array().filter(|a| !a.is_empty()) else {
            return json_error(400, json!({"error": "bad-request", "message": "clients needs at least one entry"}));
        };
        if !software {
            assert!(req.get("downloads").is_none(), "a game body has no downloads key");
        }
        for c in list {
            let platform = c["platform"].as_str().unwrap().to_string();
            assert!(c["entrypoint"].is_string());
            if let Some(list) = c.get("contents").and_then(|v| v.as_array()) {
                // File by file: no zip fields, the entrypoint among the files.
                assert!(c.get("file_name").is_none() && c.get("size").is_none(), "a per-file client sends no zip fields");
                if !list.iter().any(|f| f["path"] == c["entrypoint"]) {
                    return json_error(400, json!({"error": "bad-request", "message": "the entrypoint is not one of its files"}));
                }
                for f in list {
                    contents.push((
                        platform.clone(),
                        f["path"].as_str().unwrap().to_string(),
                        f["size"].as_u64().unwrap(),
                        f["sha256"].as_str().unwrap().to_string(),
                    ));
                    assert!(f["executable"].is_boolean());
                }
                continue;
            }
            let file_name = c["file_name"].as_str().unwrap().to_string();
            files.push(FileSpec {
                kind: "client",
                key: format!("{app}/{version}/{platform}/{file_name}"),
                platform,
                file_name,
                size: c["size"].as_u64().unwrap(),
                sha256: c["sha256"].as_str().unwrap().into(),
            });
        }
    }
    if contents.is_empty() {
        assert!(req.get("objects").is_none() && req.get("uploaded").is_none(), "objects only with per-file clients");
    }

    let mut m = s.lock().unwrap();
    let base = m.base.clone();
    let idx = match m.releases.iter().position(|r| r.app == app && r.version == version) {
        Some(i) => {
            let r = &m.releases[i];
            if r.status != "uploading" || r.rules != rules || r.clients != clients || r.downloads != downloads {
                let status = r.status.clone();
                m.create_statuses.push(409);
                return json_error(409, json!({"error": "version-exists", "status": status}));
            }
            m.create_statuses.push(200);
            i
        }
        None => {
            // One module per (game, rules_version) among approved releases.
            let clash = rules.as_ref().and_then(|mine| {
                m.releases.iter().filter(|r| r.app == app && r.status == "approved").find_map(|r| {
                    let theirs = r.rules.as_ref()?;
                    (theirs["rules_version"] == mine["rules_version"] && theirs["sha256"] != mine["sha256"]).then(|| theirs.clone())
                })
            });
            if let (Some(mine), Some(theirs)) = (&rules, clash) {
                m.create_statuses.push(409);
                let approved = theirs["sha256"].as_str().unwrap();
                return json_error(
                    409,
                    json!({
                        "error": "rules-conflict",
                        "message": format!("rules {} is already approved with a different module (sha256 {}…); ship that exact module, or bump rules_version if the rules changed", mine["rules_version"].as_str().unwrap(), &approved[..12]),
                        "rules_version": mine["rules_version"],
                        "sha256": mine["sha256"],
                        "approved_sha256": approved,
                    }),
                );
            }
            let seq = m.releases.len() + 1;
            m.releases.push(Release {
                id: format!("00000000-0000-4000-8000-{seq:012}"),
                seq,
                app: app.clone(),
                version,
                channel,
                notes,
                rules,
                clients,
                downloads,
                status: "uploading".into(),
                files,
                contents: contents.clone(),
            });
            m.create_statuses.push(201);
            m.releases.len() - 1
        }
    };
    let status = if *m.create_statuses.last().unwrap() == 201 { StatusCode::CREATED } else { StatusCode::OK };
    let r = &m.releases[idx];
    let release_id = r.id.clone();
    let mut uploads: Vec<Value> = r
        .files
        .iter()
        .map(|f| {
            json!({
                "kind": f.kind,
                "platform": f.platform,
                "file_name": f.file_name,
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
    if contents.is_empty() {
        return (status, Json(json!({"release_id": release_id, "status": "uploading", "uploads": uploads}))).into_response();
    }

    // Per-file: the game's objects, stored once whatever the release.
    let shas: std::collections::BTreeSet<String> = contents.iter().map(|c| c.3.clone()).collect();
    for c in &contents {
        m.store.entry((app.clone(), c.3.clone())).or_insert_with(|| Stored { size: c.2, ..Default::default() });
    }
    let hints: Vec<String> = req.get("uploaded").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
    m.hints.push(hints.clone());
    let cap = if m.sign_cap == 0 { 2000 } else { m.sign_cap };
    // 1. Earlier described objects may be in storage already: hinted first.
    let mut candidates: Vec<String> = shas
        .iter()
        .filter(|s| m.store[&(app.clone(), (*s).clone())].stored_size.is_some() && !m.store[&(app.clone(), (*s).clone())].uploaded)
        .cloned()
        .collect();
    candidates.sort_by_key(|s| !hints.contains(s));
    for sha in candidates.into_iter().take(cap) {
        let st = m.store[&(app.clone(), sha.clone())].clone();
        if m.objects.get(&format!("{app}/objects/{sha}")).is_some_and(|b| Some(b.len() as u64) == st.stored_size) {
            m.store.get_mut(&(app.clone(), sha)).unwrap().uploaded = true;
        }
    }
    // 2. What this request describes, for what is still missing.
    for o in req.get("objects").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let sha = o["sha256"].as_str().unwrap().to_string();
        assert!(shas.contains(&sha), "an object of another release described");
        let st = m.store.get_mut(&(app.clone(), sha.clone())).unwrap();
        if !st.uploaded {
            st.stored_size = o["stored_size"].as_u64();
            st.stored_sha256 = o["stored_sha256"].as_str().map(String::from);
        }
    }
    let described: std::collections::BTreeSet<String> =
        req.get("objects").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|o| o["sha256"].as_str().map(String::from)).collect()).unwrap_or_default();
    let missing: Vec<String> = shas.iter().filter(|s| !m.store[&(app.clone(), (*s).clone())].uploaded).cloned().collect();
    // 3. A PUT for each described missing object, up to the cap.
    let signed: Vec<&String> = missing.iter().filter(|s| described.contains(*s)).take(cap).collect();
    for sha in &signed {
        let st = &m.store[&(app.clone(), (*sha).clone())];
        uploads.push(json!({
            "kind": "object",
            "platform": "any",
            "sha256": sha,
            "method": "PUT",
            "url": format!("{base}/r2/{app}/objects/{sha}?X-Amz-Expires=3600&X-Amz-Signature=mock"),
            "headers": {
                "content-length": st.stored_size.unwrap().to_string(),
                "x-amz-checksum-sha256": b64_of_hex(st.stored_sha256.as_deref().unwrap()),
                "x-mock-signed": "yes",
            },
            "expires_at": 1_791_162_000u64,
        }));
    }
    let summary = json!({
        "total": shas.len(),
        "stored": shas.len() - missing.len(),
        "missing": missing.iter().map(|s| json!({"sha256": s, "size": m.store[&(app.clone(), s.clone())].size})).collect::<Vec<_>>(),
        "unsigned": missing.len() - signed.len(),
    });
    (status, Json(json!({"release_id": release_id, "status": "uploading", "uploads": uploads, "objects": summary}))).into_response()
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
    let mut missing: Vec<Value> = r
        .files
        .iter()
        .filter(|f| match m.objects.get(&f.key) {
            Some(b) => b.len() as u64 != f.size || sha256_hex(b) != f.sha256,
            None => true,
        })
        .map(|f| match f.kind {
            "download" => json!({"kind": f.kind, "platform": f.platform, "file_name": f.file_name}),
            _ => json!({"kind": f.kind, "platform": f.platform}),
        })
        .collect();
    // Per-file: every object not yet seen in storage, at its described
    // size, a gzip that unpacks to the file it stands for.
    let mut seen = std::collections::BTreeSet::new();
    for (_, path, _, sha) in r.contents.clone() {
        if !seen.insert(sha.clone()) {
            continue;
        }
        let st = m.store.get_mut(&(r.app.clone(), sha.clone())).expect("an object of the release");
        if st.uploaded {
            continue;
        }
        let ok = match (m.objects.get(&format!("{}/objects/{sha}", r.app)), st.stored_size, st.stored_sha256.as_deref()) {
            (Some(b), Some(n), Some(want)) => {
                b.len() as u64 == n
                    && sha256_hex(b) == want
                    && biome_publish::files::gunzip_bytes(b).is_ok_and(|raw| sha256_hex(&raw) == sha)
            }
            _ => false,
        };
        if ok {
            st.uploaded = true;
        } else {
            missing.push(json!({"kind": "object", "platform": "any", "sha256": sha, "path": path}));
        }
    }
    if !missing.is_empty() {
        return json_error(409, json!({"error": "upload-incomplete", "missing": missing}));
    }
    // A file listing goes live on submit when its zips hold plain files
    // only (a simple stand-in for the edge function's look); the rest wait.
    let mut reasons = Vec::new();
    if r.app == FILE_APP {
        for f in r.files.iter().filter(|f| f.file_name.to_ascii_lowercase().ends_with(".zip")) {
            let bytes = m.objects[&f.key].clone();
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
            for i in 0..zip.len() {
                let name = zip.by_index(i).unwrap().name().to_string();
                if !name.ends_with('/') && !is_plain(&name) {
                    reasons.push(format!("{}: contains {name}", f.file_name));
                }
            }
        }
    }
    // Every release waits for its virus scan; a clean file listing goes
    // live after it, everything else to review.
    r.status = "submitted".into();
    let after = if r.app == FILE_APP && reasons.is_empty() { "live" } else { "review" };
    if reasons.is_empty() {
        Json(json!({"status": r.status, "scan": "pending", "after_scan": after})).into_response()
    } else {
        Json(json!({"status": r.status, "scan": "pending", "after_scan": after, "reasons": reasons})).into_response()
    }
}

async fn list(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return json_error(401, json!({"error": "unauthorized"}));
    }
    let Some(app) = q.get("app_id").filter(|a| *a == APP || *a == SW_APP || *a == FILE_APP) else {
        return json_error(403, json!({"error": "wrong-app"}));
    };
    let m = s.lock().unwrap();
    let mut rows: Vec<&Release> = m.releases.iter().filter(|r| &r.app == app).collect();
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
                "scan": if r.status == "submitted" { "pending" } else { "clean" },
                "scan_note": if r.status == "submitted" { "BioShield: checking" } else { "BioShield: checked" },
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

fn object_key(sha: &str) -> String {
    format!("{APP}/objects/{sha}")
}

/// The fixture's Windows folder, as objects: (path, sha256 of its bytes).
fn windows_files(fx: &Fixture) -> Vec<(String, String)> {
    biome_publish::pack::list_folder(&fx.root.join("dist/windows"))
        .unwrap()
        .into_iter()
        .map(|e| (e.name, sha256_hex(&fs::read(&e.source).unwrap())))
        .collect()
}

fn set_version(fx: &Fixture, version: &str) {
    let text = fs::read_to_string(&fx.manifest).unwrap();
    let start = text.find("version = \"").unwrap() + "version = \"".len();
    let end = start + text[start..].find('"').unwrap();
    fs::write(&fx.manifest, format!("{}{version}{}", &text[..start], &text[end..])).unwrap();
}

// ------------------------------------------------------------------ tests

#[test]
fn check_needs_no_network_and_prints_hashes() {
    let fx = fixture();
    // No token, unroutable endpoint: check must not care.
    let r = cli(&fx, "http://127.0.0.1:9/never", None, "check");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    let linux_sha = sha256_hex(&fs::read(fx.root.join("dist/linux.zip")).unwrap());
    let rules_sha = sha256_hex(&fs::read(demo_rules()).unwrap());
    // The Windows folder goes file by file: its build id, not a zip.
    let entries = biome_publish::pack::list_folder(&fx.root.join("dist/windows")).unwrap();
    let list = biome_publish::files::file_list(&entries, "TicTacToe.exe", "t").unwrap();
    for needle in [
        &linux_sha,
        &rules_sha,
        &format!("build id {}", list.build_id),
        &"(2 files), sent file by file; entrypoint TicTacToe.exe".to_string(),
        &"rules_version 1.0.0".to_string(),
        &"check ok".to_string(),
    ] {
        assert!(r.out.contains(needle.as_str()), "missing {needle}:\n{}", r.out);
    }
    assert!(!fx.out_dir.join(format!("{APP}-{VERSION}-windows-x64.zip")).exists(), "no zip for a per-file client");

    // Same folder, same build id.
    let again = cli(&fx, "", None, "check");
    assert_eq!(again.code, EXIT_OK);
    assert!(again.out.contains(&list.build_id));

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
    assert!(r.out.contains("0 of 2 files already on Biome; uploading 2 ("), "{}", r.out);

    let m = server.mock();
    // The file list first, then the same release again with the objects described.
    assert_eq!(m.create_statuses, [201, 200]);
    let win = windows_files(&fx);
    let mut keys: Vec<_> = m.puts.iter().map(|p| p.key.clone()).collect();
    keys.sort();
    let mut expected = vec![rules_key(), key("linux-x64"), object_key(&win[0].1), object_key(&win[1].1)];
    expected.sort();
    assert_eq!(keys, expected);
    assert!(m.puts.iter().all(|p| !p.had_authorization), "the token went to storage");

    // What arrived is byte for byte what is on disk; objects as gzips of the files.
    assert_eq!(m.objects[&rules_key()], fs::read(demo_rules()).unwrap());
    assert_eq!(m.objects[&key("linux-x64")], fs::read(fx.root.join("dist/linux.zip")).unwrap(), "a given zip is uploaded as is");
    for (path, sha) in &win {
        let raw = biome_publish::files::gunzip_bytes(&m.objects[&object_key(sha)]).unwrap();
        assert_eq!(raw, fs::read(fx.root.join("dist/windows").join(path)).unwrap(), "{path}");
    }
    assert!(!fx.out_dir.join("objects").exists(), "compressed objects are cleaned up after submit");

    // And the release carries the module's own rules_version, the notes and the file list.
    let rel = &m.releases[0];
    assert_eq!(rel.status, "submitted");
    assert_eq!(rel.channel, "beta");
    assert_eq!(rel.notes, "Bigger board.");
    assert_eq!(rel.rules.as_ref().unwrap()["rules_version"], "1.0.0");
    let entrypoints: Vec<_> = rel.clients.as_array().unwrap().iter().map(|c| c["entrypoint"].as_str().unwrap().to_string()).collect();
    assert_eq!(entrypoints, ["bin/tictactoe", "TicTacToe.exe"]);
    let paths: Vec<_> = rel.contents.iter().map(|c| c.1.as_str()).collect();
    assert_eq!(paths, ["TicTacToe.exe", "data/board.json"]);
    assert!(m.store.values().all(|s| s.uploaded), "every object verified at submit");
}

#[test]
fn a_new_version_uploads_only_the_changed_file() {
    let server = Server::start();
    let fx = fixture();
    // A bigger game: 40 asset files next to the program.
    for i in 0..40 {
        write(&fx.root.join(format!("dist/windows/assets/tile{i:02}.png")), format!("tile {i} {}", "x".repeat(i * 10)).as_bytes());
    }
    let v1 = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(v1.code, EXIT_OK, "{}\n{}", v1.out, v1.err);
    assert!(v1.out.contains("0 of 42 files already on Biome; uploading 42 ("), "{}", v1.out);
    let before = server.mock().puts.len();

    // Only the program changes.
    write(&fx.root.join("dist/windows/TicTacToe.exe"), b"MZ the next version");
    set_version(&fx, "1.3.0");
    let v2 = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(v2.code, EXIT_OK, "{}\n{}", v2.out, v2.err);
    assert!(v2.out.contains("41 of 42 files already on Biome; uploading 1 ("), "{}", v2.out);
    let m = server.mock();
    let new_puts: Vec<_> = m.puts[before..].iter().map(|p| p.key.clone()).collect();
    let exe = sha256_hex(b"MZ the next version");
    assert_eq!(new_puts.iter().filter(|k| k.contains("/objects/")).cloned().collect::<Vec<_>>(), [object_key(&exe)], "exactly one object");
    assert_eq!(m.releases[1].status, "submitted");
}

#[test]
fn objects_go_up_in_the_batches_the_server_signs() {
    let server = Server::start();
    server.mock().sign_cap = 3;
    let fx = fixture();
    for i in 0..8 {
        write(&fx.root.join(format!("dist/windows/level{i}.json")), format!("{{\"level\":{i}}}").as_bytes());
    }
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    let m = server.mock();
    // 10 objects, 3 per answer: a file list, then four described rounds.
    assert_eq!(m.create_statuses, [201, 200, 200, 200, 200]);
    assert_eq!(m.puts.iter().filter(|p| p.key.contains("/objects/")).count(), 10, "each once");
    // Each round names the batch it just uploaded, for the server to check.
    assert_eq!(m.hints.iter().map(Vec::len).collect::<Vec<_>>(), [0, 0, 3, 3, 3]);
    assert!(r.out.contains("uploaded 3 of 10; asking for the rest"), "{}", r.out);
    assert_eq!(m.releases[0].status, "submitted");
}

#[test]
fn a_file_two_platforms_share_is_one_object() {
    let server = Server::start();
    let fx = fixture();
    write(&fx.root.join("dist/linux/bin/tictactoe"), b"\x7fELF");
    write(&fx.root.join("dist/linux/data/board.json"), br#"{"size":3}"#);
    let manifest = fs::read_to_string(&fx.manifest).unwrap().replace("path = \"dist/linux.zip\"", "path = \"dist/linux\"");
    fs::write(&fx.manifest, manifest).unwrap();
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    assert!(r.out.contains("linux-x64: 0 of 2 files already on Biome"), "{}", r.out);
    assert!(r.out.contains("windows-x64: 0 of 2 files already on Biome"), "{}", r.out);
    assert!(r.out.contains("uploading 3 ("), "four files, three contents: {}", r.out);
    let board = object_key(&sha256_hex(br#"{"size":3}"#));
    assert_eq!(server.mock().puts.iter().filter(|p| p.key == board).count(), 1);
}

#[test]
fn a_folder_can_still_go_as_a_zip() {
    let server = Server::start();
    let fx = fixture();
    let manifest = fs::read_to_string(&fx.manifest).unwrap().replace("entrypoint = \"TicTacToe.exe\"", "entrypoint = \"TicTacToe.exe\"\ndelivery = \"zip\"");
    fs::write(&fx.manifest, manifest).unwrap();
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    let m = server.mock();
    assert_eq!(m.create_statuses, [201], "one call, as before per-file delivery");
    let built = fx.out_dir.join(format!("{APP}-{VERSION}-windows-x64.zip"));
    assert_eq!(m.objects[&key("windows-x64")], fs::read(&built).unwrap());
    assert_eq!(biome_publish::pack::zip_file_names(&built).unwrap(), ["TicTacToe.exe", "data/board.json"]);
    assert!(m.puts.iter().all(|p| !p.key.contains("/objects/")));
}

#[test]
fn a_failed_upload_resumes_on_the_next_run() {
    let server = Server::start();
    let fx = fixture();
    let win = windows_files(&fx);
    server.mock().fail_next_put = Some(win[1].1.clone());

    let first = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(first.code, EXIT_REFUSED, "{}\n{}", first.out, first.err);
    assert!(first.err.contains("AccessDenied") && first.err.contains("Request has expired"), "{}", first.err);
    assert!(!first.err.contains("X-Amz-Signature"), "presigned URL leaked into the error: {}", first.err);
    assert_eq!(server.mock().releases[0].status, "uploading");
    let stored_first: Vec<_> = server.mock().puts.iter().map(|p| p.key.clone()).collect();

    // What made it to storage the first time (uploads run in parallel, so
    // the other object may or may not have gone before the failure).
    let made_it: Vec<String> = win.iter().map(|w| object_key(&w.1)).filter(|k| server.mock().objects.contains_key(k)).collect();
    assert!(!made_it.contains(&object_key(&win[1].1)));

    let second = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(second.code, EXIT_OK, "{}\n{}", second.out, second.err);
    assert!(second.out.contains("resuming release 00000000-0000-4000-8000-000000000001"), "{}", second.out);
    // What made it is found in storage by the server, not sent again.
    assert!(second.out.contains(&format!("{} of 2 files already on Biome; uploading {} (", made_it.len(), 2 - made_it.len())), "{}", second.out);
    let m = server.mock();
    let resent: Vec<_> = m.puts[stored_first.len()..].iter().filter(|p| p.key.contains("/objects/")).map(|p| p.key.clone()).collect();
    assert!(resent.contains(&object_key(&win[1].1)), "{resent:?}");
    assert!(resent.iter().all(|k| !made_it.contains(k)), "an object in storage was sent again: {resent:?}");
    assert_eq!(m.create_statuses, [201, 200, 200, 200]);
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
    assert_eq!(m.create_statuses, [201, 200, 409]);
    assert_eq!(m.puts.len(), puts_before, "nothing uploaded after a refusal");
}

#[test]
fn a_rules_module_that_clashes_with_the_approved_one_is_refused_before_uploading() {
    let server = Server::start();
    let fx = fixture();
    assert_eq!(cli(&fx, &server.endpoint, Some(TOKEN), "upload").code, EXIT_OK);
    // Approved, with other bytes under the same rules_version (a rebuild).
    let theirs = "0".repeat(64);
    {
        let mut m = server.mock();
        m.releases[0].status = "approved".into();
        m.releases[0].rules.as_mut().unwrap()["sha256"] = json!(theirs);
    }
    let puts_before = server.mock().puts.len();

    set_version(&fx, "1.0.1");
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_REFUSED, "{}", r.err);
    assert!(r.err.contains("409 rules-conflict"), "{}", r.err);
    assert!(r.err.contains("rules 1.0.0 is already approved with a different module"), "{}", r.err);
    assert!(r.err.contains(&format!("approved module sha256: {theirs}")), "{}", r.err);
    let mine = sha256_hex(&fs::read(demo_rules()).unwrap());
    assert!(r.err.contains(&format!("this module sha256:     {mine}")), "{}", r.err);
    let m = server.mock();
    assert_eq!(m.create_statuses, [201, 200, 409]);
    assert_eq!(m.releases.len(), 1, "nothing was created");
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
    assert!(r.err.contains("missing upload: client linux-x64") && r.err.contains("missing upload: rules any"), "{}", r.err);
    assert!(r.err.contains("missing upload: object TicTacToe.exe (") && r.err.contains("missing upload: object data/board.json ("), "{}", r.err);
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
    assert!(row.contains("BioShield: checking"), "{row}");
    assert!(r.out.contains("VERSION") && r.out.contains("BIOSHIELD") && r.out.contains("REVIEW NOTE"), "{}", r.out);
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

// ------------------------------------------------------- software products

const SW_VERSION: &str = "1.2.0";

/// A Blender add-on repo: the extension folder (zipped by the CLI), a
/// helper executable uploaded as it is, release notes and the manifest.
fn software_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write(&root.join("addon/blender_manifest.toml"), b"schema_version = \"1.0.0\"\nid = \"node_wrangler\"\nversion = \"1.2.0\"\n");
    write(&root.join("addon/__init__.py"), b"def register():\n    pass\n");
    write(&root.join("addon/icons/frame.png"), &[137, 80, 78, 71]);
    write(&root.join("dist/helper.exe"), b"MZ not really a helper");
    write(&root.join("CHANGELOG.md"), b"Frame nodes.\n");
    let manifest = root.join("biome.toml");
    write(
        &manifest,
        format!(
            r#"
[product]
app_id = "{SW_APP}"

[release]
version = "{SW_VERSION}"
notes = "CHANGELOG.md"

[[download]]
path = "addon"
label = "Add-on (Blender 4.2+)"
file_name = "node_wrangler-{SW_VERSION}.zip"

[[download]]
path = "dist/helper.exe"
platform = "windows-x64"
"#
        )
        .as_bytes(),
    );
    let out_dir = root.join("out");
    Fixture { _dir: dir, root, manifest, out_dir }
}

fn sw_key(name: &str) -> String {
    format!("{SW_APP}/{SW_VERSION}/files/{name}")
}

#[test]
fn software_check_prints_the_downloads() {
    let fx = software_fixture();
    let r = cli(&fx, "http://127.0.0.1:9/never", None, "check");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    let zip = fx.out_dir.join(format!("node_wrangler-{SW_VERSION}.zip"));
    for needle in [
        format!("product   {SW_APP} (software or file listing: the server knows which)"),
        "download  any (Add-on (Blender 4.2+))".to_string(),
        "download  windows-x64".to_string(),
        "upload as helper.exe".to_string(),
        sha256_hex(&fs::read(&zip).unwrap()),
        sha256_hex(&fs::read(fx.root.join("dist/helper.exe")).unwrap()),
        "check ok".to_string(),
    ] {
        assert!(r.out.contains(&needle), "missing {needle}:\n{}", r.out);
    }
    assert!(!r.out.contains("rules"), "no rules line for software:\n{}", r.out);
    // The add-on folder's contents sit at the zip's root, as Blender 4.2 wants.
    assert_eq!(biome_publish::pack::zip_file_names(&zip).unwrap(), ["__init__.py", "blender_manifest.toml", "icons/frame.png"]);
}

#[test]
fn software_upload_puts_every_download_and_submits() {
    let server = Server::start();
    let fx = software_fixture();
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    assert!(r.out.contains("is submitted"), "{}", r.out);

    let m = server.mock();
    let mut keys: Vec<_> = m.puts.iter().map(|p| p.key.clone()).collect();
    keys.sort();
    assert_eq!(keys, [sw_key("helper.exe"), sw_key(&format!("node_wrangler-{SW_VERSION}.zip"))]);
    assert!(m.puts.iter().all(|p| !p.had_authorization), "the token went to storage");
    assert_eq!(m.objects[&sw_key("helper.exe")], fs::read(fx.root.join("dist/helper.exe")).unwrap(), "a file is uploaded as is");
    let zip = fx.out_dir.join(format!("node_wrangler-{SW_VERSION}.zip"));
    assert_eq!(m.objects[&sw_key(&format!("node_wrangler-{SW_VERSION}.zip"))], fs::read(&zip).unwrap());

    let rel = m.releases.iter().find(|r| r.app == SW_APP).unwrap();
    assert_eq!(rel.status, "submitted");
    assert_eq!(rel.notes, "Frame nodes.");
    assert!(rel.rules.is_none() && rel.clients.is_null());
    let downloads = rel.downloads.as_array().unwrap();
    assert_eq!(downloads[0]["label"], "Add-on (Blender 4.2+)");
    assert_eq!(downloads[0]["platform"], "any");
    assert!(downloads[1].get("label").is_none(), "no label, no key: {}", downloads[1]);
}

/// SOFTWARE_PRODUCTS_SPEC.md §16: a software product with a desktop program,
/// sent file by file like a game's client, next to a download.
#[test]
fn software_with_a_desktop_client_uploads_it_file_by_file() {
    let server = Server::start();
    let fx = software_fixture();
    write(&fx.root.join("app/windows/Wrangler.exe"), b"MZ the program");
    write(&fx.root.join("app/windows/presets/default.json"), b"{}");
    let text = fs::read_to_string(&fx.manifest).unwrap();
    write(
        &fx.manifest,
        format!("{text}
[client.windows-x64]
path = \"app/windows\"
entrypoint = \"Wrangler.exe\"
").as_bytes(),
    );

    let check = cli(&fx, "http://127.0.0.1:9/never", None, "check");
    assert_eq!(check.code, EXIT_OK, "{}
{}", check.out, check.err);
    assert!(check.out.contains("client    windows-x64"), "{}", check.out);

    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}
{}", r.out, r.err);
    let m = server.mock();
    let rel = m.releases.iter().find(|r| r.app == SW_APP).unwrap();
    assert_eq!(rel.status, "submitted");
    assert!(rel.rules.is_none());
    let clients = rel.clients.as_array().unwrap();
    assert_eq!((clients.len(), clients[0]["platform"].as_str(), clients[0]["entrypoint"].as_str()), (1, Some("windows-x64"), Some("Wrangler.exe")));
    let paths: Vec<_> = rel.contents.iter().map(|c| c.1.as_str()).collect();
    assert_eq!(paths, ["Wrangler.exe", "presets/default.json"]);
    assert_eq!(rel.downloads.as_array().unwrap().len(), 2);
    let exe = sha256_hex(b"MZ the program");
    let raw = biome_publish::files::gunzip_bytes(&m.objects[&format!("{SW_APP}/objects/{exe}")]).unwrap();
    assert_eq!(raw, b"MZ the program");
}

#[test]
fn a_failed_software_upload_resumes_and_missing_files_are_named() {
    let server = Server::start();
    let fx = software_fixture();
    server.mock().fail_next_put = Some("helper.exe".into());
    let first = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(first.code, EXIT_REFUSED, "{}\n{}", first.out, first.err);
    let second = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(second.code, EXIT_OK, "{}\n{}", second.out, second.err);
    assert!(second.out.contains("resuming release"), "{}", second.out);
    assert_eq!(server.mock().create_statuses, [201, 200]);

    let dropped = Server::start();
    dropped.mock().drop_puts = true;
    let r = cli(&fx, &dropped.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_REFUSED, "{}", r.err);
    assert!(r.err.contains(&format!("missing upload: download any node_wrangler-{SW_VERSION}.zip")), "{}", r.err);
    assert!(r.err.contains("missing upload: download windows-x64 helper.exe"), "{}", r.err);
}

#[test]
fn software_status_lists_its_releases() {
    let server = Server::start();
    let fx = software_fixture();
    assert_eq!(cli(&fx, &server.endpoint, Some(TOKEN), "upload").code, EXIT_OK);
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "status");
    assert_eq!(r.code, EXIT_OK, "{}", r.err);
    assert!(r.out.contains(&format!("{SW_APP}: 1 most recent releases")), "{}", r.out);
    assert!(r.out.lines().any(|l| l.starts_with(SW_VERSION) && l.contains("submitted")), "{}", r.out);
}

// ----------------------------------------------------------- file listings

/// A shop item's repo: a print file and its settings, as a file listing.
fn file_fixture(extra: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write(&root.join("files/Benchy.STL"), b"solid benchy\nendsolid benchy\n");
    write(&root.join("files/settings.pdf"), b"%PDF-1.4 not really");
    write(&root.join("extras/readme.txt"), b"extra parts");
    let entries = biome_publish::pack::list_folder(&root.join("extras")).unwrap();
    biome_publish::pack::zip_folder(&entries, "", &root.join("files/extras.zip")).unwrap();
    let manifest = root.join("biome.toml");
    write(
        &manifest,
        format!(
            "[product]\napp_id = \"{FILE_APP}\"\n\n[release]\nversion = \"1.0.0\"\n\n[[download]]\npath = \"files/Benchy.STL\"\nlabel = \"Model\"\n\n[[download]]\npath = \"files/settings.pdf\"\n{extra}"
        )
        .as_bytes(),
    );
    let out_dir = root.join("out");
    Fixture { _dir: dir, root, manifest, out_dir }
}

#[test]
fn a_file_listing_uploads_with_a_product_manifest_and_goes_live_after_its_scan() {
    let server = Server::start();
    let fx = file_fixture("");
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    assert!(
        r.out.contains(&format!("{FILE_APP} 1.0.0 is uploaded. BioShield: checking. a file listing goes live once BioShield has checked its files")),
        "{}",
        r.out
    );
    let m = server.mock();
    let rel = m.releases.iter().find(|r| r.app == FILE_APP).unwrap();
    assert_eq!(rel.status, "submitted");
    assert_eq!(m.objects[&format!("{FILE_APP}/1.0.0/files/Benchy.STL")], fs::read(fx.root.join("files/Benchy.STL")).unwrap());
}

#[test]
fn a_zip_of_plain_files_in_a_file_listing_goes_live_after_its_scan() {
    let server = Server::start();
    let fx = file_fixture("\n[[download]]\npath = \"files/extras.zip\"\n");
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    assert!(r.out.contains(&format!("{FILE_APP} 1.0.0 is uploaded. BioShield: checking")), "{}", r.out);
    assert!(server.mock().objects.contains_key(&format!("{FILE_APP}/1.0.0/files/extras.zip")));
}

#[test]
fn a_zip_with_a_program_waits_for_review_and_says_why() {
    let server = Server::start();
    let fx = file_fixture("\n[[download]]\npath = \"files/tools.zip\"\n");
    write(&fx.root.join("tools/setup.exe"), b"MZ not really");
    write(&fx.root.join("tools/notes.txt"), b"notes");
    let entries = biome_publish::pack::list_folder(&fx.root.join("tools")).unwrap();
    biome_publish::pack::zip_folder(&entries, "", &fx.root.join("files/tools.zip")).unwrap();
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_OK, "{}\n{}", r.out, r.err);
    assert!(r.out.contains(&format!("{FILE_APP} 1.0.0 is waiting for a site admin's review:")), "{}", r.out);
    assert!(r.out.contains("  - tools.zip: contains setup.exe"), "{}", r.out);
    assert!(r.out.contains("BioShield: checking its files too."), "{}", r.out);
    assert_eq!(server.mock().releases.iter().find(|r| r.app == FILE_APP).unwrap().status, "submitted");
}

#[test]
fn a_program_in_a_file_listing_is_refused_with_the_reason() {
    let server = Server::start();
    let fx = file_fixture("\n[[download]]\npath = \"files/slicer.exe\"\n");
    write(&fx.root.join("files/slicer.exe"), b"MZ not really");
    let r = cli(&fx, &server.endpoint, Some(TOKEN), "upload");
    assert_eq!(r.code, EXIT_REFUSED, "{}\n{}", r.out, r.err);
    assert!(r.err.contains("400 not-plain-file") && r.err.contains("software product"), "{}", r.err);
    assert!(r.err.contains("not a plain file: slicer.exe"), "{}", r.err);
    assert!(server.mock().puts.is_empty(), "nothing uploaded");
}
