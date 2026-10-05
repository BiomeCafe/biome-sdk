//! IPC transport — length-delimited bincode over OS-native local IPC.
//!
//! - **Windows**: named pipes (`\\.\pipe\biome-os`)
//! - **Unix**: unix domain sockets (`$XDG_RUNTIME_DIR/biome-os.sock`
//!   or `/tmp/biome-os-$USER.sock`)
//!
//! Wire format per frame: `[u32 LE length][bincode body]`. Max frame size
//! is 1 MiB (defensive cap; control plane never approaches this).

use std::io;
#[cfg(unix)]
use std::path::PathBuf;

use bincode::config::Configuration;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::LengthDelimitedCodec;

use crate::error::BosError;
use crate::protocol::BosWire;

/// Max IPC frame size (1 MiB). Anything larger is a bug.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Bincode2 config used for every frame. Standard fixed-int encoding —
/// matches Rust types exactly, no varints (varints can be slower at low rate
/// and the savings don't matter for control-plane traffic).
const BINCODE_CFG: Configuration = bincode::config::standard();

// ─────────────────────────────────────────────────────────────────────────────
// Default IPC endpoint (platform-aware)
// ─────────────────────────────────────────────────────────────────────────────

/// Where the BOS server listens / the client connects by default.
///
/// Override with [`crate::types::Transport::Custom { path }`].
pub fn default_endpoint() -> String {
    #[cfg(windows)]
    {
        // Per-user namespacing avoids collisions on shared machines.
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        format!(r"\\.\pipe\biome-os-{}", user)
    }
    #[cfg(unix)]
    {
        let dir = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
        dir.join(format!("biome-os-{}.sock", user))
            .to_string_lossy()
            .into_owned()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Framed read / write helpers — work over any AsyncRead + AsyncWrite
// ─────────────────────────────────────────────────────────────────────────────

/// A framed BOS stream over any tokio stream (what [`frame`] returns).
pub type Framed<S> = tokio_util::codec::Framed<S, LengthDelimitedCodec>;

/// Wrap any tokio stream as a length-delimited frame stream of [`BosWire`].
pub fn frame<S>(stream: S) -> Framed<S>
where
    S: AsyncRead + AsyncWrite,
{
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .length_field_type::<u32>()
        .little_endian()
        .new_framed(stream)
}

/// Encode + send one wire message.
pub async fn send_wire<S>(
    framed: &mut Framed<S>,
    msg: &BosWire,
) -> Result<(), BosError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let bytes = bincode::serde::encode_to_vec(msg, BINCODE_CFG)
        .map_err(|e| BosError::Serde(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(BosError::Transport(format!(
            "frame too large: {} bytes (max {})",
            bytes.len(),
            MAX_FRAME_BYTES
        )));
    }
    framed
        .send(bytes.into())
        .await
        .map_err(|e: io::Error| BosError::Transport(e.to_string()))
}

/// Receive + decode one wire message. Returns `None` on clean EOF.
pub async fn recv_wire<S>(
    framed: &mut Framed<S>,
) -> Result<Option<BosWire>, BosError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(frame) = framed.next().await else {
        return Ok(None);
    };
    let bytes = frame.map_err(|e: io::Error| BosError::Transport(e.to_string()))?;
    let (msg, _consumed): (BosWire, usize) =
        bincode::serde::decode_from_slice(&bytes, BINCODE_CFG)
            .map_err(|e| BosError::Serde(e.to_string()))?;
    Ok(Some(msg))
}

// ─────────────────────────────────────────────────────────────────────────────
// Platform-specific connect / listen
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
pub mod platform {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tokio::net::{UnixListener, UnixStream};

    pub type ServerStream = UnixStream;
    pub type ClientStream = UnixStream;

    pub async fn connect(endpoint: &str) -> Result<ClientStream, BosError> {
        UnixStream::connect(endpoint)
            .await
            .map_err(|_| BosError::PlatformUnavailable)
    }

    /// Bind a unix listener. Removes any stale socket file first.
    pub fn bind_listener(endpoint: &str) -> Result<UnixListener, BosError> {
        let path = Path::new(endpoint);
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        let listener = UnixListener::bind(path)?;
        // 0600 — only the owning user can connect. Defense in depth even
        // though XDG_RUNTIME_DIR is already user-private.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        Ok(listener)
    }
}

#[cfg(windows)]
pub mod platform {
    use super::*;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    pub type ServerStream = NamedPipeServer;
    pub type ClientStream = NamedPipeClient;

    pub async fn connect(endpoint: &str) -> Result<ClientStream, BosError> {
        // ERROR_FILE_NOT_FOUND (2) = pipe doesn't exist yet = biome-app not running.
        // Any other error is a real transport error worth surfacing verbatim.
        match ClientOptions::new().open(endpoint) {
            Ok(c) => Ok(c),
            Err(e) if e.raw_os_error() == Some(2) => Err(BosError::PlatformUnavailable),
            Err(e) => Err(BosError::Transport(e.to_string())),
        }
    }

    /// Create the first server pipe instance. Caller is responsible for
    /// creating subsequent instances after each `connect()` accept.
    pub fn create_server_pipe(endpoint: &str, first_instance: bool) -> Result<NamedPipeServer, BosError> {
        ServerOptions::new()
            .first_pipe_instance(first_instance)
            .create(endpoint)
            .map_err(|e| BosError::Transport(e.to_string()))
    }
}
