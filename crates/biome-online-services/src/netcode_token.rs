//! Netcode-token codec.
//!
//! The orchestrator generates per-match netcode authentication tokens and
//! packs them into the opaque `netcode_token` field of [`crate::ConnectInfo`].
//! Some netcode library on the game side (lightyear, renet, naia, whatever)
//! needs those fields broken back out to call its `Client::connect()`-style
//! API.
//!
//! This module defines the wire format and provides a single decoder. BOS
//! itself has no opinion on which netcode library you use — the decoder
//! returns three primitive values you wire into whatever auth struct your
//! netcode crate exposes.
//!
//! ## Wire format
//!
//! Fixed 56 bytes, stable for `CURRENT_SDK_VERSION == 1`:
//!
//! ```text
//!   [0..16]    client_id (u128 little-endian) — packed in u128 for future-
//!              proofing; current netcodes use the low u64.
//!   [16..48]   private_key (32 bytes)         — opaque key material
//!   [48..56]   protocol_id (u64 little-endian)
//! ```

use std::net::SocketAddr;

use crate::error::BosError;

const TOKEN_LEN: usize = 56;

/// Decode the netcode-token bytes from a [`crate::ConnectInfo`] bundle.
///
/// Returns `(client_id, private_key, protocol_id)`. Plug each into your
/// netcode crate's auth API (e.g. lightyear's `Authentication::Manual`,
/// renet's `ConnectToken`, etc).
pub fn decode(token: &[u8]) -> Result<(u64, [u8; 32], u64), BosError> {
    if token.len() != TOKEN_LEN {
        return Err(BosError::Internal(format!(
            "expected netcode token of {TOKEN_LEN} bytes, got {}",
            token.len()
        )));
    }
    let mut client_id_bytes = [0u8; 16];
    client_id_bytes.copy_from_slice(&token[0..16]);
    let client_id = u128::from_le_bytes(client_id_bytes) as u64;

    let mut private_key = [0u8; 32];
    private_key.copy_from_slice(&token[16..48]);

    let mut protocol_bytes = [0u8; 8];
    protocol_bytes.copy_from_slice(&token[48..56]);
    let protocol_id = u64::from_le_bytes(protocol_bytes);

    Ok((client_id, private_key, protocol_id))
}

/// Convenience: full `(server_addr, client_id, private_key, protocol_id)`
/// tuple from the bundle.
pub fn decode_bundle(
    bundle: &crate::ConnectInfo,
) -> Result<(SocketAddr, u64, [u8; 32], u64), BosError> {
    let (client_id, key, protocol) = decode(&bundle.netcode_token)?;
    Ok((bundle.game_server, client_id, key, protocol))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(client_id: u128, key: [u8; 32], protocol_id: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(TOKEN_LEN);
        out.extend_from_slice(&client_id.to_le_bytes());
        out.extend_from_slice(&key);
        out.extend_from_slice(&protocol_id.to_le_bytes());
        out
    }

    #[test]
    fn decodes_fields_little_endian() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let token = encode(0x0102_0304_0506_0708, key, 0xDEAD_BEEF_CAFE_F00D);
        let (client_id, got_key, protocol_id) = decode(&token).unwrap();
        assert_eq!(client_id, 0x0102_0304_0506_0708);
        assert_eq!(got_key, key);
        assert_eq!(protocol_id, 0xDEAD_BEEF_CAFE_F00D);
    }

    #[test]
    fn client_id_uses_low_u64() {
        let token = encode((7u128 << 64) | 42, [0; 32], 1);
        assert_eq!(decode(&token).unwrap().0, 42);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(decode(&[0u8; 55]).is_err());
        assert!(decode(&[0u8; 57]).is_err());
        assert!(decode(&[]).is_err());
    }
}
