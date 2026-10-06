//! The axum layer over [`Host`]: the same routes as the production Worker.
//!
//! Auth here is a development stand-in: `Authorization: Bearer
//! dev:<user id>:<username>` is believed as written. The production host
//! verifies a signed game ticket instead.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};

use crate::{parse_dev_ticket, Host, Out, Reply, User, TICK};

fn reply((status, body): Reply) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response()
}

/// `Bearer dev:<user id>:<username>` → the user. The username may contain
/// colons; the user id may not.
pub fn dev_user(headers: &HeaderMap) -> Option<User> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?.trim();
    parse_dev_ticket(token)
}

fn unauthorized() -> Response {
    reply((401, json!({ "error": "unauthorized", "message": "dev server expects `Authorization: Bearer dev:<user id>:<username>`" })))
}

fn ws_url(headers: &HeaderMap, app: &str, match_id: &str) -> String {
    let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("127.0.0.1");
    format!("ws://{host}/v1/{app}/matches/{match_id}/ws")
}

async fn health() -> Response {
    reply((200, json!({ "ok": true, "dev": true })))
}

async fn rules(State(host): State<Arc<Host>>) -> Response {
    reply((200, host.describe()))
}

async fn create(State(host): State<Arc<Host>>, Path(app): Path<String>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let Some(user) = dev_user(&headers) else { return unauthorized() };
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let (status, mut out) = host.create(&app, &user, &body);
    if status == 200 {
        let id = out["match_id"].as_str().unwrap_or_default().to_string();
        out["ws_url"] = json!(ws_url(&headers, &app, &id));
    }
    reply((status, out))
}

async fn join(State(host): State<Arc<Host>>, Path(app): Path<String>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let Some(user) = dev_user(&headers) else { return unauthorized() };
    if !host.has_app(&app) {
        return reply((404, json!({ "error": "unknown-app" })));
    }
    let code = body.as_ref().and_then(|Json(v)| v.get("join_code")).and_then(Value::as_str).unwrap_or("").to_string();
    let (status, mut out) = host.join_by_code(&app, &user, &code);
    if status == 200 {
        let id = out["match_id"].as_str().unwrap_or_default().to_string();
        out["ws_url"] = json!(ws_url(&headers, &app, &id));
    }
    reply((status, out))
}

async fn rejoin(State(host): State<Arc<Host>>, Path((app, id)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let Some(user) = dev_user(&headers) else { return unauthorized() };
    let (status, mut out) = host.rejoin(&app, &id, &user);
    if status == 200 {
        out["ws_url"] = json!(ws_url(&headers, &app, &id));
    }
    reply((status, out))
}

/// `POST /v1/:app/matches/:id/join` — the open seat of a listed match.
async fn join_open(State(host): State<Arc<Host>>, Path((app, id)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let Some(user) = dev_user(&headers) else { return unauthorized() };
    if !host.has_app(&app) {
        return reply((404, json!({ "error": "unknown-app" })));
    }
    let (status, mut out) = host.join_open(&app, &id, &user);
    if status == 200 {
        out["ws_url"] = json!(ws_url(&headers, &app, &id));
    }
    reply((status, out))
}

/// `GET /v1/:app/open?rules_version=` — public.
async fn open_list(State(host): State<Arc<Host>>, Path(app): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let version = q.get("rules_version").map(String::as_str).unwrap_or("");
    reply(host.open_matches(&app, version))
}

/// `GET /v1/:app/queue?mode=&rules_version=` — the queue socket. The first
/// frame is `hello` with a (dev) ticket, the config and the clock.
async fn queue_socket(
    State(host): State<Arc<Host>>,
    Path(app): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let mode = q.get("mode").cloned().unwrap_or_default();
    let version = q.get("rules_version").cloned().unwrap_or_default();
    if let Err(r) = host.queue_check(&app, &mode, &version) {
        return reply(r);
    }
    let Ok(upgrade) = upgrade else { return reply((426, json!({ "error": "upgrade-required" }))) };
    upgrade.on_upgrade(move |ws| run_queue_socket(host, app, mode, version, ws))
}

async fn run_queue_socket(host: Arc<Host>, app: String, mode: String, version: String, ws: WebSocket) {
    let (mut sink, mut stream) = ws.split();
    let (conn_id, rx) = match host.queue_connect(&app, &mode, &version) {
        Ok(v) => v,
        Err((_, body)) => {
            let _ = sink.send(Message::Text(json!({ "t": "error", "code": body["error"] }).to_string().into())).await;
            let _ = sink.send(Message::Close(Some(CloseFrame { code: 4410, reason: "no such queue".into() }))).await;
            return;
        }
    };
    let writer = tokio::spawn(write_out(sink, rx));
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => host.queue_frame(conn_id, text.as_str()),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Binary(_) | Message::Close(_) => break,
        }
    }
    host.queue_close(conn_id);
    writer.abort();
}

/// Deliver what the host queues for one socket until it says close.
async fn write_out(mut sink: futures_util::stream::SplitSink<WebSocket, Message>, mut rx: tokio::sync::mpsc::UnboundedReceiver<Out>) {
    while let Some(out) = rx.recv().await {
        match out {
            Out::Frame(text) => {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Out::Close(code, reason) => {
                let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                break;
            }
        }
    }
}

async fn summary(State(host): State<Arc<Host>>, Path((app, id)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let Some(user) = dev_user(&headers) else { return unauthorized() };
    reply(host.summary(&app, &id, &user))
}

async fn socket(State(host): State<Arc<Host>>, Path((app, id)): Path<(String, String)>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |ws| run_socket(host, app, id, ws))
}

async fn run_socket(host: Arc<Host>, app: String, match_id: String, ws: WebSocket) {
    let (mut sink, mut stream) = ws.split();
    let Some((conn_id, rx)) = host.connect(&app, &match_id) else {
        let _ = sink.send(Message::Text(json!({ "t": "error", "code": "no-such-match" }).to_string().into())).await;
        let _ = sink.send(Message::Close(Some(CloseFrame { code: 4410, reason: "no such match".into() }))).await;
        return;
    };

    // Outbound: whatever the host queues for this socket.
    let writer = tokio::spawn(write_out(sink, rx));

    // Inbound: text frames go to the host; anything else ends the socket.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => host.on_frame(&app, &match_id, conn_id, text.as_str()),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Binary(_) | Message::Close(_) => break,
        }
    }
    host.on_close(&app, &match_id, conn_id);
    writer.abort();
}

/// The routes of the production host, backed by a [`Host`].
pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/rules", get(rules))
        .route("/v1/{app}/matches", post(create))
        .route("/v1/{app}/matches/join", post(join))
        .route("/v1/{app}/matches/{id}", get(summary))
        .route("/v1/{app}/matches/{id}/rejoin", post(rejoin))
        .route("/v1/{app}/matches/{id}/join", post(join_open))
        .route("/v1/{app}/matches/{id}/ws", get(socket))
        .route("/v1/{app}/open", get(open_list))
        .route("/v1/{app}/queue", get(queue_socket))
        .with_state(host)
}

/// Bind `addr` (port 0 picks a free one) and serve until the task is
/// dropped. Returns the bound address and the server task.
pub async fn serve(host: Arc<Host>, addr: SocketAddr) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let ticker = host.clone();
    let app = router(host);
    let task = tokio::spawn(async move {
        let clocks = tokio::spawn(async move {
            let mut interval = tokio::time::interval(TICK);
            loop {
                interval.tick().await;
                ticker.tick();
            }
        });
        let _ = axum::serve(listener, app).await;
        clocks.abort();
    });
    Ok((local, task))
}
