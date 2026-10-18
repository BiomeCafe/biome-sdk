//! Is Biome here? Three states, cheaply, with no side effects:
//!
//! | state | how we know |
//! |---|---|
//! | [`PlatformState::Running`] | the IPC endpoint accepts a connection (we open and immediately close; the host treats that as a probe) |
//! | [`PlatformState::Installed`] | **Windows:** the `biome:` URL scheme is registered for the current user at `HKCU\Software\Classes\biome\shell\open\command`. The Velopack installer writes it on first launch and every update re-points it at the stable launcher stub (`%LocalAppData%\Biome\Biome.exe`). **macOS:** `/Applications/Biome.app` exists. **Linux:** a `biome.desktop` entry exists in the user or system applications dir. |
//! | [`PlatformState::NotInstalled`] | neither |
//!
//! Waking an installed-but-closed Biome is [`wake`]: it starts the launcher
//! with a `biome://connect?app=<app_id>` link and returns at once. Biome
//! comes up on its sign-in window (or straight to the main window when a
//! session is remembered) and starts serving the IPC endpoint early in its
//! launch, so [`wait_until_running`] usually resolves within a few seconds.
//! The game then connects normally; the consent prompt (first time only) is
//! shown by Biome.
//!
//! Focus: launching another app on Windows gives it the foreground. Biome
//! can't avoid that for its own first window; keep the game's "waiting for
//! Biome…" state visible so the player knows what happened.
//!
//! macOS and Linux paths are implemented best-effort and are **untested**
//! (no deep-link registration exists on those platforms yet; see
//! `crates/biome-app/src/deeplink.rs`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::BosError;
use crate::transport;

/// What [`detect`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlatformState {
    /// Biome is running and accepting connections.
    Running,
    /// Biome is installed but not running. `launcher` is what [`wake`] starts.
    Installed { launcher: PathBuf },
    /// No Biome install found for this user.
    NotInstalled,
}

/// The deep link a wake sends. Biome opens, and (once signed in) accepts
/// the attach that follows.
pub fn connect_url(app_id: &str) -> String {
    format!("biome://connect?app={}", url_escape(app_id))
}

/// Probe the default endpoint, then the install marker.
pub fn detect() -> PlatformState {
    detect_at(&transport::default_endpoint())
}

/// [`detect`] against a specific IPC endpoint (dev hosts, tests).
pub fn detect_at(endpoint: &str) -> PlatformState {
    if endpoint_accepts(endpoint) {
        return PlatformState::Running;
    }
    match installed_launcher() {
        Some(launcher) => PlatformState::Installed { launcher },
        None => PlatformState::NotInstalled,
    }
}

/// `true` when something is listening on the IPC endpoint right now.
/// Opens and closes a connection without sending anything; the host treats
/// a hello-less close as a probe and stays quiet.
pub fn endpoint_accepts(endpoint: &str) -> bool {
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        match OpenOptions::new().read(true).write(true).open(endpoint) {
            Ok(_pipe) => true,
            Err(e) => match e.raw_os_error() {
                // ERROR_FILE_NOT_FOUND: no server has created the pipe.
                Some(2) => false,
                // ERROR_PIPE_BUSY: every instance is mid-accept — a server exists.
                Some(231) => true,
                _ => false,
            },
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(endpoint).is_ok()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = endpoint;
        false
    }
}

/// The executable (or `open`/`xdg-open` target) a wake should start, if
/// Biome is installed for this user.
pub fn installed_launcher() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows::launcher_from_registry()
    }
    #[cfg(target_os = "macos")]
    {
        let app = PathBuf::from("/Applications/Biome.app");
        app.exists().then_some(app)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let candidates = [
            home.join(".local/share/applications/biome.desktop"),
            PathBuf::from("/usr/share/applications/biome.desktop"),
            PathBuf::from("/usr/local/share/applications/biome.desktop"),
        ];
        candidates.into_iter().find(|p| p.exists())
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

/// Start Biome with a `biome://connect?app=<app_id>` link and return
/// immediately. Errors: [`BosError::PlatformNotInstalled`],
/// [`BosError::WakeFailed`].
pub fn wake(app_id: &str) -> Result<(), BosError> {
    let launcher = installed_launcher().ok_or(BosError::PlatformNotInstalled)?;
    wake_with(&launcher, app_id)
}

fn wake_with(launcher: &PathBuf, app_id: &str) -> Result<(), BosError> {
    open_link(launcher, &connect_url(app_id))
}

/// Hand Biome a `biome://` link through its launcher (what the shell does
/// for a registered scheme, minus the shell) and return at once. A running
/// Biome gets the link from the second launch; a closed one starts with it.
pub(crate) fn open_link(launcher: &PathBuf, url: &str) -> Result<(), BosError> {
    #[cfg(windows)]
    {
        // Spawn the launcher stub directly with the link as its argument —
        // exactly what the shell does for a registered scheme, minus the
        // shell. No console window, no inherited handles we care about.
        std::process::Command::new(launcher)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| BosError::WakeFailed(format!("{}: {e}", launcher.display())))
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-a")
            .arg(launcher)
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| BosError::WakeFailed(format!("open -a {}: {e}", launcher.display())))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = launcher;
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| BosError::WakeFailed(format!("xdg-open: {e}")))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (launcher, url);
        Err(BosError::WakeFailed("unsupported platform".into()))
    }
}

/// Poll the default endpoint until it accepts or `timeout` passes.
pub fn wait_until_running(timeout: Duration) -> Result<(), BosError> {
    wait_until_running_at(&transport::default_endpoint(), timeout)
}

/// [`wait_until_running`] against a specific endpoint.
pub fn wait_until_running_at(endpoint: &str, timeout: Duration) -> Result<(), BosError> {
    let start = Instant::now();
    loop {
        if endpoint_accepts(endpoint) {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(BosError::WakeFailed(format!(
                "biome did not start serving within {}s",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// One call for the common boot path: running → `Ok`; installed → wake and
/// wait up to `wake_timeout`; not installed → `Err(PlatformNotInstalled)`.
/// After `Ok`, call [`crate::BiomeClient::connect`].
pub fn ensure_running(app_id: &str, wake_timeout: Duration) -> Result<PlatformState, BosError> {
    match detect() {
        PlatformState::Running => Ok(PlatformState::Running),
        PlatformState::Installed { launcher } => {
            wake_with(&launcher, app_id)?;
            wait_until_running(wake_timeout)?;
            Ok(PlatformState::Installed { launcher })
        }
        PlatformState::NotInstalled => Err(BosError::PlatformNotInstalled),
    }
}

/// Percent-encode anything outside the unreserved set so a hostile app id
/// can't smuggle extra query parameters into the link.
pub(crate) fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    /// `HKCU\Software\Classes\biome\shell\open\command` default value is
    /// `"C:\Users\me\AppData\Local\Biome\Biome.exe" "%1"`. Written by
    /// `biome-app/src/deeplink.rs::register_protocol` on every launch of an
    /// installed build.
    pub fn launcher_from_registry() -> Option<PathBuf> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\Classes\biome\shell\open\command")
            .ok()?;
        let command: String = key.get_value("").ok()?;
        let exe = super::first_command_token(&command)?;
        let path = PathBuf::from(exe);
        path.is_file().then_some(path)
    }
}

/// The program part of a shell command line: the first quoted token, or
/// the first whitespace-delimited token.
fn first_command_token(command: &str) -> Option<String> {
    let s = command.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    s.split_whitespace().next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_url_escapes_hostile_ids() {
        assert_eq!(connect_url("mecha-aether"), "biome://connect?app=mecha-aether");
        assert_eq!(connect_url("a&b=c d"), "biome://connect?app=a%26b%3Dc%20d");
    }

    #[test]
    fn first_token_handles_quotes_and_bare_paths() {
        assert_eq!(
            first_command_token(r#""C:\Users\me\AppData\Local\Biome\Biome.exe" "%1""#).as_deref(),
            Some(r"C:\Users\me\AppData\Local\Biome\Biome.exe")
        );
        assert_eq!(first_command_token(r"C:\x\biome.exe %1").as_deref(), Some(r"C:\x\biome.exe"));
        assert_eq!(first_command_token(""), None);
    }

    #[test]
    fn nothing_listens_on_a_made_up_endpoint() {
        #[cfg(windows)]
        let ep = format!(r"\\.\pipe\biome-os-test-nobody-{}", std::process::id());
        #[cfg(unix)]
        let ep = format!("/tmp/biome-os-test-nobody-{}.sock", std::process::id());
        assert!(!endpoint_accepts(&ep));
        assert!(wait_until_running_at(&ep, Duration::from_millis(50)).is_err());
    }
}
