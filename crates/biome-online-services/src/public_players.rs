//! Public Biome cards and existing game/mode standings, without a login token.
//! These bounded HTTP reads work independently of the desktop IPC connection.
//! Browser integrations use the same JSON endpoints with fetch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use crossbeam_channel::Receiver;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use crate::match_client::MatchEndpoint;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub user_id: String,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub rank: u64,
    pub app_id: String,
    pub mode: String,
    pub user_id: String,
    pub username: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    pub rating: f64,
    pub deviation: f64,
    pub matches: u32,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeaderboardPage {
    pub app_id: String,
    pub mode: String,
    pub offset: u32,
    pub limit: u32,
    pub has_more: bool,
    pub entries: Vec<LeaderboardEntry>,
    #[serde(default)]
    pub player: Option<LeaderboardEntry>,
    /// UTC request time, not a database transaction snapshot guarantee.
    pub snapshot_at: String,
}

/// One background read. `poll` yields its result exactly once.
pub struct PublicRead<T> {
    rx: Receiver<Result<T, String>>,
    taken: AtomicBool,
}

impl<T> PublicRead<T> {
    pub fn poll(&self) -> Option<Result<T, String>> {
        if self.taken.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() { return None; }
        let result = match self.rx.try_recv() {
            Ok(result) => result,
            Err(crossbeam_channel::TryRecvError::Empty) => { self.taken.store(false, Ordering::Release); return None; }
            Err(crossbeam_channel::TryRecvError::Disconnected) => Err("internal: public read thread ended".into()),
        };
        Some(result)
    }
}

fn url(endpoint: &MatchEndpoint, path: &str) -> Result<reqwest::Url, String> {
    reqwest::Url::parse(&format!("{}/v1/{}/{}", endpoint.base_url.trim_end_matches('/'), endpoint.app_id, path))
        .map_err(|_| "invalid-endpoint".to_string())
}

/// At most 100 stable account IDs in one read. Missing/deleted cards are
/// absent; private users retain the same public name/avatar as Biome bylines.
pub fn profiles(endpoint: &MatchEndpoint, ids: &[String]) -> PublicRead<Vec<PublicProfile>> {
    let request = if ids.len() > 100 { Err("bad-request: at most 100 profile ids".into()) } else {
        url(endpoint, "profiles").map(|mut url| { url.query_pairs_mut().append_pair("ids", &ids.join(",")); url })
    };
    read(request)
}

/// Existing all-time Glicko-2 standings for this game and mode. A requested
/// player's row includes their actual rank even when outside this page.
pub fn leaderboard(endpoint: &MatchEndpoint, mode: &str, limit: u32, offset: u32, user_id: Option<&str>) -> PublicRead<LeaderboardPage> {
    let request = url(endpoint, "leaderboard").map(|mut url| {
        { let mut q = url.query_pairs_mut();
          q.append_pair("mode", mode).append_pair("limit", &limit.to_string()).append_pair("offset", &offset.to_string());
          if let Some(id) = user_id { q.append_pair("user_id", id); }
        }
        url
    });
    read(request)
}

fn read<T: DeserializeOwned + Send + 'static>(url: Result<reqwest::Url, String>) -> PublicRead<T> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let _ = std::thread::Builder::new().name("biome-public-read".into()).spawn(move || {
        let result = match url {
            Err(error) => Err(error),
            Ok(url) => match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Err(error) => Err(format!("internal: runtime: {error}")),
                Ok(rt) => rt.block_on(fetch_json(url)),
            },
        };
        let _ = tx.send(result);
    });
    PublicRead { rx, taken: AtomicBool::new(false) }
}

async fn fetch_json<T: DeserializeOwned>(url: reqwest::Url) -> Result<T, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|error| format!("internal: {error}"))?;
    let mut response = client.get(url).send().await.map_err(|_| "network: public read failed".to_string())?;
    let status = response.status();
    // Bound the response even when a proxy omits or lies about Content-Length.
    const MAX_BYTES: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "network: public read interrupted".to_string())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES { return Err("bad-response: public read too large".into()); }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let code = value.get("error").and_then(serde_json::Value::as_str).unwrap_or("http-error");
        return Err(format!("{code}: http {status}"));
    }
    serde_json::from_slice(&bytes).map_err(|_| "bad-response: invalid public read".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};

    fn response(body: serde_json::Value, status: &str) -> (MatchEndpoint, std::sync::mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let status = status.to_string();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16 * 1024);
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let _ = tx.send(String::from_utf8(request).unwrap());
            let text = body.to_string();
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
        });
        (MatchEndpoint::new(format!("http://{address}"), "omg", "test-rules"), rx, thread)
    }

    fn wait<T>(read: &PublicRead<T>) -> Result<T, String> {
        let started = std::time::Instant::now();
        loop {
            if let Some(result) = read.poll() { return result; }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn cards_are_minimal_and_optional_fields_are_compatible() {
        let card: PublicProfile = serde_json::from_value(json!({"user_id":"id", "username":"mandy"})).unwrap();
        assert_eq!(card.display_name, None);
        assert_eq!(card.avatar_url, None);
        assert!(serde_json::to_value(card).unwrap().get("email").is_none());
    }

    #[test]
    fn completed_reads_are_delivered_once() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let result = PublicRead { rx, taken: AtomicBool::new(false) };
        assert!(result.poll().is_none());
        tx.send(Ok::<_, String>(7)).unwrap();
        assert_eq!(result.poll(), Some(Ok(7)));
        assert_eq!(result.poll(), None);
    }

    #[test]
    fn concurrent_pollers_cannot_deliver_an_extra_closed_channel_error() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(Ok::<_, String>(7)).unwrap();
        drop(tx);
        let result = std::sync::Arc::new(PublicRead { rx, taken: AtomicBool::new(false) });
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let pollers: Vec<_> = (0..8).map(|_| {
            let result = result.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || { barrier.wait(); result.poll() })
        }).collect();
        let delivered: Vec<_> = pollers.into_iter().filter_map(|thread| thread.join().unwrap()).collect();
        assert_eq!(delivered, vec![Ok(7)]);
    }

    #[test]
    fn profile_reads_use_one_public_request_and_surface_old_host_errors() {
        let id = "00000000-0000-4000-8000-000000000007".to_string();
        let (endpoint, requests, thread) = response(json!([{"user_id":id,"username":"mandy","display_name":"Mandy","avatar_url":"https://cdn.test/a.png"}]), "200 OK");
        let read = profiles(&endpoint, &[id.clone()]);
        assert_eq!(wait(&read).unwrap()[0].user_id, id);
        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.starts_with("GET /v1/omg/profiles?ids="));
        assert!(!request.to_lowercase().contains("authorization:"));
        assert!(read.poll().is_none());
        thread.join().unwrap();

        let (endpoint, _, thread) = response(json!({"error":"not-found"}), "404 Not Found");
        assert!(wait(&profiles(&endpoint, &[id])).unwrap_err().starts_with("not-found: http 404"));
        thread.join().unwrap();
    }

    #[test]
    fn standings_keep_real_rank_counts_and_request_timestamp() {
        let (endpoint, requests, thread) = response(json!({
            "app_id":"omg","mode":"ranked","offset":25,"limit":25,"has_more":false,
            "entries":[],"player":{"rank":91,"app_id":"omg","mode":"ranked","user_id":"player-id","username":"mandy",
              "rating":1580.5,"deviation":75.0,"matches":13,"wins":7,"losses":5,"draws":1,"updated_at":"2026-10-17T01:00:00Z"},
            "snapshot_at":"2026-10-17T02:00:00Z"
        }), "200 OK");
        let page = wait(&leaderboard(&endpoint, "ranked", 25, 25, Some("player-id"))).unwrap();
        assert_eq!(page.player.unwrap().rank, 91);
        assert_eq!(page.snapshot_at, "2026-10-17T02:00:00Z");
        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.contains("mode=ranked&limit=25&offset=25&user_id=player-id"));
        thread.join().unwrap();
    }
}
