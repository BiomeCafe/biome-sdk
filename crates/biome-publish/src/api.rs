//! Client for the `game-publish` edge function (spec §3) and the presigned
//! storage uploads it hands out.
//!
//! The publish token goes only to the function, never to a presigned URL,
//! and is never printed. Presigned URLs are credentials too (for an hour),
//! so error messages show them without their query string.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::{Body, Client, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Refusal};

/// Supabase project the binary was built for (`SUPABASE_URL` at compile
/// time, as in `biome-core`), else the local stack.
pub const DEFAULT_SUPABASE_URL: &str = match option_env!("SUPABASE_URL") {
    Some(url) => url,
    None => "http://127.0.0.1:54321",
};

/// The function's path under a Supabase project URL.
pub const FUNCTION_PATH: &str = "/functions/v1/game-publish";

/// Environment variable holding the publish token.
pub const TOKEN_ENV: &str = "BIOME_PUBLISH_TOKEN";

/// Environment variable overriding the endpoint.
pub const ENDPOINT_ENV: &str = "BIOME_PUBLISH_ENDPOINT";

/// Budget for one API call (not for uploads, which take as long as they take).
const API_TIMEOUT: Duration = Duration::from_secs(120);

/// Attempts per upload when the network or storage (5xx) fails.
const UPLOAD_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------- wire types

/// `POST /releases` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRelease {
    pub app_id: String,
    pub version: String,
    pub channel: String,
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<RulesMeta>,
    pub clients: Vec<ClientMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesMeta {
    pub rules_version: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMeta {
    pub platform: String,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    pub entrypoint: String,
}

/// `POST /releases` answer (201 new, 200 resumed).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CreateResponse {
    pub release_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub uploads: Vec<Upload>,
}

/// One presigned upload. Send `method` to `url` with exactly `headers`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Upload {
    pub kind: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, serde_json::Value>,
    /// Unix seconds, as game-publish sends it.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

fn default_method() -> String {
    "PUT".to_string()
}

impl Upload {
    /// `("rules", "any")` or `("client", <platform>)`.
    pub fn target(&self) -> (String, String) {
        let platform = match self.kind.as_str() {
            "rules" => "any".to_string(),
            _ => self.platform.clone().unwrap_or_default(),
        };
        (self.kind.clone(), platform)
    }

    /// The headers as an HTTP header map. Numbers and booleans are sent as
    /// their JSON text (a server may write `"content-length": 123`).
    pub fn header_map(&self) -> Result<HeaderMap, Error> {
        let mut map = HeaderMap::new();
        for (k, v) in &self.headers {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                other => return Err(Error::bad_response(format!("upload header {k} has a non-scalar value {other}"))),
            };
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| Error::bad_response(format!("upload header name {k:?} is not valid")))?;
            let value = HeaderValue::from_str(&text).map_err(|_| Error::bad_response(format!("upload header {k} has an invalid value")))?;
            map.append(name, value);
        }
        Ok(map)
    }
}

/// `POST /releases/{id}/submit` answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubmitResponse {
    pub status: String,
}

/// One entry of `GET /releases?app_id=`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct ReleaseSummary {
    #[serde(default)]
    pub release_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub review_note: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<String>,
    #[serde(default)]
    pub reviewed_at: Option<String>,
}

// ------------------------------------------------------------ configuration

/// `--endpoint`, else `BIOME_PUBLISH_ENDPOINT`, else the build's Supabase
/// URL plus [`FUNCTION_PATH`]. Plain `http` is only allowed to loopback
/// hosts: the token must not cross a network in clear text.
pub fn resolve_endpoint(flag: Option<&str>, env: Option<&str>) -> Result<String, Error> {
    let nonempty = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let raw = nonempty(flag)
        .or_else(|| nonempty(env))
        .unwrap_or_else(|| format!("{}{FUNCTION_PATH}", DEFAULT_SUPABASE_URL.trim_end_matches('/')));
    let raw = raw.trim_end_matches('/').to_string();
    let url = Url::parse(&raw).map_err(|e| Error::invalid(format!("endpoint {raw:?} is not a URL: {e}")))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::invalid(format!("endpoint {raw:?} must not have a query or fragment")));
    }
    match url.scheme() {
        "https" => Ok(raw),
        "http" if is_loopback(&url) => Ok(raw),
        "http" => Err(Error::invalid(format!(
            "endpoint {raw:?}: refusing to send the publish token over plain http to a non-local host; use https"
        ))),
        other => Err(Error::invalid(format!("endpoint {raw:?}: unsupported scheme {other:?}"))),
    }
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else { return false };
    host.eq_ignore_ascii_case("localhost")
        || host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The token from the environment, trimmed (CI secrets often end in a
/// newline). Never echoed.
pub fn check_token(raw: Option<&str>) -> Result<String, Error> {
    let token = raw.map(str::trim).unwrap_or("");
    if token.is_empty() {
        return Err(Error::invalid(format!(
            "{TOKEN_ENV} is not set: create a publish token for this game (guild admin, game settings) and store it as a CI secret"
        )));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::invalid(format!("{TOKEN_ENV} contains characters that cannot be sent in an HTTP header")));
    }
    Ok(token.to_string())
}

// ------------------------------------------------------------------ client

pub struct Api {
    http: Client,
    endpoint: String,
    token: String,
    progress: bool,
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api").field("endpoint", &self.endpoint).field("token", &"<redacted>").finish()
    }
}

impl Api {
    /// `progress`: print upload percentages to stderr.
    pub fn new(endpoint: String, token: String, progress: bool) -> Result<Api, Error> {
        let http = Client::builder()
            .user_agent(concat!("biome-publish/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .timeout(None::<Duration>)
            .build()
            .map_err(|e| Error::io(format!("cannot start the HTTP client: {}", chain(&e))))?;
        Ok(Api { http, endpoint, token, progress })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Create the release, or resume it while it is still `uploading`.
    /// Returns the HTTP status (201 or 200) with the answer.
    pub fn create_release(&self, body: &CreateRelease) -> Result<(u16, CreateResponse), Error> {
        let url = format!("{}/releases", self.endpoint);
        let resp = self.call(self.http.post(&url).json(body), &url)?;
        let status = resp.status().as_u16();
        let text = read_body(resp, &url)?;
        if status != 200 && status != 201 {
            return Err(Error::Refused(refusal(status, &text)));
        }
        let created: CreateResponse = parse(&text, "POST /releases")?;
        if created.release_id.is_empty() || !created.release_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(Error::bad_response(format!("release_id {:?} is not an id", created.release_id)));
        }
        Ok((status, created))
    }

    pub fn submit(&self, release_id: &str) -> Result<SubmitResponse, Error> {
        let url = format!("{}/releases/{release_id}/submit", self.endpoint);
        let resp = self.call(self.http.post(&url).json(&serde_json::json!({})), &url)?;
        let status = resp.status().as_u16();
        let text = read_body(resp, &url)?;
        if status != 200 {
            return Err(Error::Refused(refusal(status, &text)));
        }
        parse(&text, "POST /releases/{id}/submit")
    }

    pub fn list(&self, app_id: &str) -> Result<Vec<ReleaseSummary>, Error> {
        let mut url = Url::parse(&format!("{}/releases", self.endpoint)).map_err(|e| Error::invalid(e.to_string()))?;
        url.query_pairs_mut().append_pair("app_id", app_id);
        let shown = url.to_string();
        let resp = self.call(self.http.get(url), &shown)?;
        let status = resp.status().as_u16();
        let text = read_body(resp, &shown)?;
        if status != 200 {
            return Err(Error::Refused(refusal(status, &text)));
        }
        parse(&text, "GET /releases")
    }

    fn call(&self, req: reqwest::blocking::RequestBuilder, url: &str) -> Result<Response, Error> {
        req.bearer_auth(&self.token)
            .timeout(API_TIMEOUT)
            .send()
            .map_err(|e| Error::io(format!("network error calling {url}: {}", chain(&e.without_url()))))
    }

    /// Stream `path` (exactly `size` bytes) to a presigned URL with exactly
    /// the headers the server returned. No `Authorization` header: the URL
    /// carries its own signature. Retries network failures and 5xx.
    pub fn upload(&self, upload: &Upload, path: &Path, size: u64, label: &str, log: &mut dyn Write) -> Result<(), Error> {
        let method = Method::from_bytes(upload.method.as_bytes())
            .map_err(|_| Error::bad_response(format!("upload method {:?} is not an HTTP method", upload.method)))?;
        let headers = upload.header_map()?;
        let url = Url::parse(&upload.url).map_err(|_| Error::bad_response(format!("upload URL for {label} is not a URL")))?;
        // Same rule as the endpoint: the build only travels over TLS (or to
        // a local test server).
        if !(url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url))) {
            return Err(Error::bad_response(format!("upload URL for {label} is not https")));
        }
        let shown = redact(&url);

        let mut attempt = 1;
        loop {
            let file = File::open(path).map_err(|e| Error::io(format!("cannot read {}: {e}", path.display())))?;
            let reader = Progress::new(file, size, label, self.progress);
            let result = self.http.request(method.clone(), url.clone()).headers(headers.clone()).body(Body::sized(reader, size)).send();
            let retry_reason = match result {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                Ok(resp) if resp.status().is_server_error() => format!("storage answered HTTP {}", resp.status().as_u16()),
                Ok(resp) => return Err(storage_refusal(resp, label, &shown)),
                Err(e) => format!("network error: {}", chain(&e.without_url())),
            };
            if attempt >= UPLOAD_ATTEMPTS {
                return Err(Error::io(format!("uploading {label} to {shown} failed after {attempt} attempts: {retry_reason}")));
            }
            let _ = writeln!(log, "  {label}: {retry_reason}; retrying ({}/{UPLOAD_ATTEMPTS})", attempt + 1);
            std::thread::sleep(Duration::from_secs(2 * u64::from(attempt)));
            attempt += 1;
        }
    }
}

fn read_body(resp: Response, url: &str) -> Result<String, Error> {
    resp.text().map_err(|e| Error::io(format!("network error reading the answer from {url}: {}", chain(&e.without_url()))))
}

fn parse<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|e| Error::bad_response(format!("{what}: {e}")))
}

/// The server's JSON error (`{"error","message",...}`) as a [`Refusal`].
pub fn refusal(status: u16, body: &str) -> Refusal {
    let json: Option<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(body).ok();
    let Some(obj) = json else {
        let snippet: String = body.trim().chars().take(300).collect();
        return Refusal {
            status,
            error: format!("http-{status}"),
            message: None,
            details: if snippet.is_empty() { Vec::new() } else { vec![snippet] },
        };
    };
    let text = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let mut details = Vec::new();
    if let Some(s) = text("status") {
        details.push(format!("existing release status: {s}"));
    }
    if let Some(missing) = obj.get("missing").and_then(|v| v.as_array()) {
        for m in missing {
            let kind = m.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let platform = m.get("platform").and_then(|v| v.as_str()).unwrap_or("any");
            details.push(format!("missing upload: {kind} {platform}"));
        }
    }
    Refusal { status, error: text("error").unwrap_or_else(|| format!("http-{status}")), message: text("message"), details }
}

/// Storage's answer to a presigned request (S3 XML error, usually).
fn storage_refusal(resp: Response, label: &str, shown_url: &str) -> Error {
    let status = resp.status().as_u16();
    let body = resp.text().unwrap_or_default();
    let tag = |t: &str| {
        let open = format!("<{t}>");
        let start = body.find(&open)? + open.len();
        let end = body[start..].find(&format!("</{t}>"))? + start;
        Some(body[start..end].to_string())
    };
    let error = tag("Code").unwrap_or_else(|| "upload-failed".to_string());
    let message = tag("Message").or_else(|| {
        let snippet: String = body.trim().chars().take(300).collect();
        (!snippet.is_empty()).then_some(snippet)
    });
    Error::Refused(Refusal { status, error, message, details: vec![format!("while uploading {label} to {shown_url}")] })
}

/// A URL without its query string (where presigned signatures live).
pub fn redact(url: &Url) -> String {
    let mut u = url.clone();
    u.set_query(None);
    u.set_fragment(None);
    u.to_string()
}

/// An error and its causes on one line ("error sending request: connection
/// refused").
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

/// Reader that prints every 10% of a large upload to stderr.
struct Progress<R> {
    inner: R,
    total: u64,
    done: u64,
    next_tenth: u64,
    label: String,
    enabled: bool,
}

impl<R> Progress<R> {
    fn new(inner: R, total: u64, label: &str, enabled: bool) -> Self {
        // Not worth the noise for small files.
        let enabled = enabled && total >= 8 * 1024 * 1024;
        Progress { inner, total, done: 0, next_tenth: 1, label: label.to_string(), enabled }
    }
}

impl<R: Read> Read for Progress<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.done += n as u64;
        if self.enabled {
            while self.next_tenth <= 10 && self.done * 10 >= self.total * self.next_tenth {
                eprintln!("  {}: {}%", self.label, self.next_tenth * 10);
                self.next_tenth += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_precedence_and_default() {
        assert_eq!(resolve_endpoint(Some("https://a.example/fn/"), Some("https://b.example/fn")).unwrap(), "https://a.example/fn");
        assert_eq!(resolve_endpoint(None, Some("https://b.example/fn")).unwrap(), "https://b.example/fn");
        assert_eq!(resolve_endpoint(Some("  "), None).unwrap(), format!("{}{FUNCTION_PATH}", DEFAULT_SUPABASE_URL.trim_end_matches('/')));
        assert!(resolve_endpoint(None, None).unwrap().ends_with("/functions/v1/game-publish"));
    }

    #[test]
    fn plain_http_only_to_loopback() {
        for ok in ["http://127.0.0.1:54321/functions/v1/game-publish", "http://localhost:8080/x", "http://[::1]:9/x", "https://xyz.supabase.co/functions/v1/game-publish"] {
            assert!(resolve_endpoint(Some(ok), None).is_ok(), "{ok}");
        }
        for bad in ["http://xyz.supabase.co/functions/v1/game-publish", "ftp://x/y", "not a url", "https://x.example/fn?a=b"] {
            assert!(matches!(resolve_endpoint(Some(bad), None), Err(Error::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn token_checks() {
        assert_eq!(check_token(Some(" bpt_abc\n")).unwrap(), "bpt_abc");
        assert!(matches!(check_token(None), Err(Error::Invalid(_))));
        assert!(matches!(check_token(Some("  ")), Err(Error::Invalid(_))));
        assert!(matches!(check_token(Some("bpt_a b")), Err(Error::Invalid(_))));
        let shown = format!("{:?}", Api::new("https://x.example".into(), "bpt_secret".into(), false).unwrap());
        assert!(!shown.contains("bpt_secret"), "{shown}");
    }

    #[test]
    fn refusal_parsing() {
        let r = refusal(409, r#"{"error":"version-exists","status":"submitted"}"#);
        assert_eq!(r.error, "version-exists");
        assert_eq!(r.details, ["existing release status: submitted"]);
        let r = refusal(409, r#"{"error":"upload-incomplete","missing":[{"kind":"client","platform":"windows-x64"},{"kind":"rules"}]}"#);
        assert_eq!(r.details, ["missing upload: client windows-x64", "missing upload: rules any"]);
        let r = refusal(400, r#"{"error":"bad-request","message":"clients: platforms must be unique"}"#);
        assert_eq!(r.message.as_deref(), Some("clients: platforms must be unique"));
        let r = refusal(502, "<html>Bad Gateway</html>");
        assert_eq!(r.error, "http-502");
        assert_eq!(r.details, ["<html>Bad Gateway</html>"]);
    }

    /// The create answer exactly as `supabase/functions/game-publish/handler.ts`
    /// builds it (numbers where it sends numbers).
    #[test]
    fn parses_the_real_create_response() {
        let body = r#"{"release_id":"00000000-0000-4000-8000-000000000001","status":"uploading","resumed":false,
            "uploads":[
              {"kind":"rules","platform":"any","method":"PUT",
               "url":"https://acct.r2.cloudflarestorage.com/biome-game-builds/mecha-aether/0.3.0/rules.wasm?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=ab",
               "headers":{"content-length":"1234","x-amz-checksum-sha256":"qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo="},
               "expires_at":1791158400},
              {"kind":"client","platform":"windows-x64","method":"PUT",
               "url":"https://acct.r2.cloudflarestorage.com/biome-game-builds/mecha-aether/0.3.0/windows-x64/mecha-aether-0.3.0-windows-x64.zip?X-Amz-Signature=cd",
               "headers":{"content-length":"99999","x-amz-checksum-sha256":"u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7s="},
               "expires_at":1791158400}]}"#;
        let created: CreateResponse = serde_json::from_str(body).expect("the handler's own shape parses");
        assert_eq!(created.uploads.len(), 2);
        assert_eq!(created.uploads[0].expires_at, Some(1_791_158_400));
        assert_eq!(created.uploads[1].target(), ("client".to_string(), "windows-x64".to_string()));
    }

    #[test]
    fn upload_headers_and_targets() {
        let u: Upload = serde_json::from_str(
            r#"{"kind":"client","platform":"linux-x64","method":"PUT","url":"https://r2.example/k?X-Amz-Signature=s",
                "headers":{"content-length":"12","x-amz-checksum-sha256":"abc=","x-num":5},"expires_at":1791158400}"#,
        )
        .unwrap();
        assert_eq!(u.target(), ("client".to_string(), "linux-x64".to_string()));
        let h = u.header_map().unwrap();
        assert_eq!(h.get("content-length").unwrap(), "12");
        assert_eq!(h.get("x-num").unwrap(), "5");
        let r: Upload = serde_json::from_str(r#"{"kind":"rules","url":"https://r2.example/r"}"#).unwrap();
        assert_eq!(r.target(), ("rules".to_string(), "any".to_string()));
        assert_eq!(r.method, "PUT");
        assert_eq!(redact(&Url::parse(&u.url).unwrap()), "https://r2.example/k");
    }
}
