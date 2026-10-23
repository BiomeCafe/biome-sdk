//! The public keys a license may be signed with, compiled in.
//!
//! Biome signs with its `current` key (its `kid` in every license's header)
//! and keeps a `next` one waiting; both are listed here, so an app keeps
//! working when the admin desk rotates next into current. Nobody types
//! them: the published SDK (BiomeCafe/biome-sdk) gets production's from
//! `scripts/build-sdk.mjs` at every release, which fetches
//! `game-ticket/license-keys`. In the Biome repo the list stays empty.
//!
//! A build also trusts one key given at build time, for testing an app
//! against a local stack (`node scripts/license-keys.mjs http://127.0.0.1:54321`
//! prints the line):
//!
//! ```text
//! BIOME_LICENSE_DEV_PUBLIC_KEY=<kid>:<x>
//! ```
//!
//! read with `option_env!` when this crate is compiled. Set it only for
//! builds you test against your own local stack: a build that trusts a
//! local key accepts licenses signed by whoever holds that key's private
//! half.

/// One Ed25519 public key: its id and the 32-byte key as base64url (the
/// JWK's `x`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LicenseKey {
    pub kid: &'static str,
    pub x: &'static str,
}

/// Biome's production license keys: current, then next. Written by
/// scripts/build-sdk.mjs into the published SDK; empty here.
pub const PRODUCTION_KEYS: &[LicenseKey] = &[
    LicenseKey { kid: "biome-license-20261023-db001d55", x: "G4jPyiJDMnkUO6-r5uKVpSEKMFobf2Us-Bb9OFuCpfg" }, // current
    LicenseKey { kid: "biome-license-20261023-8e8e337c", x: "dWjsOeBk_XYqP9JIHQqIePMCohnXqDiKklIgc-n950s" }, // next
];

/// The local key this build was compiled with, if any
/// (`BIOME_LICENSE_DEV_PUBLIC_KEY=<kid>:<x>`).
pub fn dev_key() -> Option<(String, String)> {
    parse_dev_key(option_env!("BIOME_LICENSE_DEV_PUBLIC_KEY")?)
}

pub(crate) fn parse_dev_key(raw: &str) -> Option<(String, String)> {
    let (kid, x) = raw.trim().split_once(':')?;
    let ok_kid = !kid.is_empty() && kid.len() <= 64 && kid.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c));
    let ok_x = x.len() == 43 && x.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
    (ok_kid && ok_x).then(|| (kid.to_string(), x.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_key_is_kid_colon_x() {
        let x = "EcR7blVhbexkl7mryjhms3h2WQ5DmSnEv9ZEBVVGFPY";
        assert_eq!(parse_dev_key(&format!("dev-1:{x}")), Some(("dev-1".into(), x.into())));
        assert_eq!(parse_dev_key(&format!(" dev-1:{x}\n")), Some(("dev-1".into(), x.into())));
        assert_eq!(parse_dev_key(x), None);
        assert_eq!(parse_dev_key(&format!(":{x}")), None);
        assert_eq!(parse_dev_key("dev-1:short"), None);
        assert_eq!(parse_dev_key(&format!("dev 1:{x}")), None);
    }

    #[test]
    fn production_keys_are_well_formed() {
        for k in PRODUCTION_KEYS {
            assert!(parse_dev_key(&format!("{}:{}", k.kid, k.x)).is_some(), "{k:?}");
        }
    }
}
