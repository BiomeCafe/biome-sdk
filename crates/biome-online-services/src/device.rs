//! The device a license is bound to (docs/GAME_SDK_SPEC.md §10).
//!
//! A license carries `dev`, a hash of the Biome user id and this machine's
//! id. The Biome desktop app computes it when it asks for a license; the
//! app's SDK computes it again when it checks one (from Biome or from its
//! cache) and refuses a license made for another machine or another user.
//! Copying the cached license file to another computer doesn't carry it
//! over. The server never sees the machine id, only the hash, and the hash
//! differs per user, so it doesn't link one person's accounts on a shared
//! computer.
//!
//! The machine id is what the operating system keeps for that purpose:
//! - Windows: `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (set at
//!   install; readable by every user);
//! - Linux: `/etc/machine-id` (or `/var/lib/dbus/machine-id`);
//! - macOS: the `IOPlatformUUID` that `ioreg` reports (untested).
//!
//! None of these are secrets and an administrator can change them; this
//! binds a license to a device the way a Steam or app-store login does, it
//! doesn't attest hardware.

use sha2::{Digest, Sha256};

/// Prefix of the hashed text; a new scheme gets a new prefix.
pub const DEVICE_SCHEME: &str = "biome-license/device/v1";

/// `sha256("biome-license/device/v1\n<user id>\n<machine id>")` as 64
/// lowercase hex characters, the `dev` claim of a license.
pub fn device_hash_for(user_id: &str, machine_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(DEVICE_SCHEME.as_bytes());
    h.update(b"\n");
    h.update(user_id.as_bytes());
    h.update(b"\n");
    h.update(machine_id.trim().to_ascii_lowercase().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// This device's hash for `user_id`, or `None` when the machine id can't be
/// read.
pub fn device_hash(user_id: &str) -> Option<String> {
    machine_id().map(|m| device_hash_for(user_id, &m))
}

/// This machine's id (see the module docs), trimmed; `None` when there is
/// none to read.
pub fn machine_id() -> Option<String> {
    let id = platform_machine_id()?;
    let id = id.trim().to_string();
    (!id.is_empty()).then_some(id)
}

#[cfg(windows)]
fn platform_machine_id() -> Option<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    // The 64-bit view: a 32-bit program would otherwise read the WOW6432Node
    // copy, which has no MachineGuid.
    let key = winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\Microsoft\Cryptography", KEY_READ | KEY_WOW64_64KEY)
        .ok()?;
    key.get_value::<String, _>("MachineGuid").ok()
}

#[cfg(target_os = "linux")]
fn platform_machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"].iter().find_map(|p| std::fs::read_to_string(p).ok())
}

#[cfg(target_os = "macos")]
fn platform_machine_id() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find(|l| l.contains("\"IOPlatformUUID\"")).and_then(|l| l.split('"').nth(3)).map(str::to_string)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_machine_id() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_binds_user_and_machine() {
        let a = device_hash_for("11111111-1111-1111-1111-111111111111", "4C4C4544-0042-3510-8051-B4C04F4E4A32");
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        // The same machine id however the OS spells it.
        assert_eq!(a, device_hash_for("11111111-1111-1111-1111-111111111111", " 4c4c4544-0042-3510-8051-b4c04f4e4a32\n"));
        // Another user, another machine: another hash.
        assert_ne!(a, device_hash_for("22222222-2222-2222-2222-222222222222", "4C4C4544-0042-3510-8051-B4C04F4E4A32"));
        assert_ne!(a, device_hash_for("11111111-1111-1111-1111-111111111111", "4C4C4544-0042-3510-8051-B4C04F4E4A33"));
        // Pinned: the desktop app and every released SDK must agree on it.
        assert_eq!(
            device_hash_for("u", "m"),
            format!("{:x}", Sha256::digest(b"biome-license/device/v1\nu\nm"))
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn this_machine_has_an_id() {
        let id = machine_id().expect("a machine id");
        assert!(id.len() >= 8, "{id}");
        assert_eq!(device_hash("u"), Some(device_hash_for("u", &id)));
    }
}
