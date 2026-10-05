//! Game tickets: what they contain and how a match host verifies one
//! without calling Biome. Feature `ticket-verify`.
//!
//! A ticket is a compact JWS (a JWT), signed **ES256** (ECDSA P-256 +
//! SHA-256) by the `game-ticket` Supabase edge function. The private key
//! lives only in that function's secrets; the desktop app asks the function
//! for a ticket with the user's own session and relays the string to the
//! game. Public keys are published as a JWKS at
//!
//! ```text
//! GET https://<project>.supabase.co/functions/v1/game-ticket/jwks
//! ```
//!
//! (no auth, cacheable; `kid` in the ticket header selects the key, so
//! rotation is adding a key, then later removing the old one).
//!
//! ## Claims
//!
//! | claim | value |
//! |---|---|
//! | `iss` | `https://biome.cafe` ([`ISSUER`]) |
//! | `sub` | user id (uuid) |
//! | `aud` | the app id the ticket was issued to |
//! | `username` | the user's Biome username at issue time |
//! | `jti` | random uuid, unique per ticket (replay detection) |
//! | `iat` / `exp` | unix seconds; `exp - iat` is 300 ([`TTL_SECS`]) |
//!
//! Header: `{"alg":"ES256","typ":"biome-game-ticket+jwt","kid":"<key id>"}`.
//!
//! ## Verifying in a Worker (TypeScript sketch)
//!
//! ```text
//! const jwks = await fetch(JWKS_URL).then(r => r.json());         // cache it
//! const { payload } = await jose.jwtVerify(ticket, jose.createLocalJWKSet(jwks), {
//!   issuer: "https://biome.cafe", audience: "mecha-aether", clockTolerance: 30 });
//! // payload.sub, payload.username, payload.jti → your match state
//! ```
//!
//! ## Verifying in Rust
//!
//! [`verify`] below does the same with `jsonwebtoken`. Match hosts should
//! also remember seen `jti`s for `exp - now` seconds to refuse replays.

use serde::{Deserialize, Serialize};

use crate::error::BosError;

/// `iss` on every ticket.
pub const ISSUER: &str = "https://biome.cafe";
/// Ticket lifetime the backend uses.
pub const TTL_SECS: u64 = 300;
/// JOSE `typ` header the backend sets.
pub const TYP: &str = "biome-game-ticket+jwt";
/// Clock skew tolerated by [`verify`], seconds.
pub const LEEWAY_SECS: u64 = 30;

/// The verified contents of a ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketClaims {
    pub iss: String,
    /// User id (uuid as string).
    pub sub: String,
    /// App id the ticket was issued to.
    pub aud: String,
    pub username: String,
    /// Unique per ticket.
    pub jti: String,
    pub iat: u64,
    pub exp: u64,
}

/// One public key as published in the JWKS.
#[derive(Clone, Debug, Deserialize)]
pub struct Jwk {
    pub kty: String,
    pub crv: Option<String>,
    pub kid: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
    #[serde(default)]
    pub alg: Option<String>,
}

/// The published key set.
#[derive(Clone, Debug, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

impl Jwks {
    pub fn parse(json: &str) -> Result<Self, BosError> {
        serde_json::from_str(json).map_err(|e| BosError::TicketInvalid(format!("jwks: {e}")))
    }

    fn find(&self, kid: Option<&str>) -> Option<&Jwk> {
        let ec = |k: &&Jwk| k.kty == "EC" && k.crv.as_deref() == Some("P-256");
        match kid {
            Some(kid) => self.keys.iter().filter(ec).find(|k| k.kid.as_deref() == Some(kid)),
            // No kid in the header: only unambiguous if there is exactly one EC key.
            None => {
                let mut it = self.keys.iter().filter(ec);
                let first = it.next();
                if it.next().is_some() { None } else { first }
            }
        }
    }
}

/// Verify `ticket` against the published `jwks` for `expected_app_id`.
/// Checks signature, `iss`, `aud`, `exp` (with [`LEEWAY_SECS`]) and that the
/// key is a P-256 key the JWKS knows. Returns the claims on success.
pub fn verify(ticket: &str, jwks: &Jwks, expected_app_id: &str) -> Result<TicketClaims, BosError> {
    use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};

    let header = decode_header(ticket).map_err(|e| BosError::TicketInvalid(format!("header: {e}")))?;
    if header.alg != Algorithm::ES256 {
        return Err(BosError::TicketInvalid(format!("alg {:?} is not ES256", header.alg)));
    }
    let key = jwks
        .find(header.kid.as_deref())
        .ok_or_else(|| BosError::TicketInvalid(format!("no P-256 key for kid {:?}", header.kid)))?;
    let (x, y) = match (&key.x, &key.y) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(BosError::TicketInvalid("jwk missing x/y".into())),
    };
    let decoding = DecodingKey::from_ec_components(x, y)
        .map_err(|e| BosError::TicketInvalid(format!("jwk: {e}")))?;

    let mut validation = Validation::new(Algorithm::ES256);
    validation.leeway = LEEWAY_SECS;
    validation.validate_exp = true;
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[expected_app_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "jti"]);

    let data = decode::<TicketClaims>(ticket, &decoding, &validation)
        .map_err(|e| BosError::TicketInvalid(e.to_string()))?;
    Ok(data.claims)
}

/// Read the claims WITHOUT checking the signature. For display and logging
/// only ("signed in as …"); never for authorization.
pub fn decode_unverified(ticket: &str) -> Result<TicketClaims, BosError> {
    use base64::Engine;
    let mut parts = ticket.split('.');
    let (_h, payload) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(_s), None) => (h, p),
        _ => return Err(BosError::TicketInvalid("not a compact JWS".into())),
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| BosError::TicketInvalid(format!("payload b64: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| BosError::TicketInvalid(format!("payload: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use std::time::{SystemTime, UNIX_EPOCH};

    const KEY_PEM: &str = include_str!("../tests/fixtures/test_ticket_key.pem");
    const JWKS_JSON: &str = include_str!("../tests/fixtures/test_ticket_pub.jwk.json");

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    fn mint(aud: &str, iat: u64, exp: u64, kid: Option<&str>) -> String {
        let claims = TicketClaims {
            iss: ISSUER.into(),
            sub: "11111111-1111-1111-1111-111111111111".into(),
            aud: aud.into(),
            username: "tester".into(),
            jti: uuid::Uuid::new_v4().to_string(),
            iat,
            exp,
        };
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(str::to_string);
        header.typ = Some(TYP.into());
        encode(&header, &claims, &EncodingKey::from_ec_pem(KEY_PEM.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn valid_ticket_verifies() {
        let jwks = Jwks::parse(JWKS_JSON).unwrap();
        let t = mint("mecha-aether", now(), now() + TTL_SECS, Some("test-2026-10"));
        let claims = verify(&t, &jwks, "mecha-aether").unwrap();
        assert_eq!(claims.username, "tester");
        assert_eq!(claims.aud, "mecha-aether");
        assert_eq!(decode_unverified(&t).unwrap(), claims);
    }

    #[test]
    fn expired_ticket_is_rejected() {
        let jwks = Jwks::parse(JWKS_JSON).unwrap();
        let t = mint("mecha-aether", now() - 1000, now() - 600, Some("test-2026-10"));
        assert!(matches!(verify(&t, &jwks, "mecha-aether"), Err(BosError::TicketInvalid(_))));
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let jwks = Jwks::parse(JWKS_JSON).unwrap();
        let t = mint("some-other-game", now(), now() + TTL_SECS, Some("test-2026-10"));
        assert!(verify(&t, &jwks, "mecha-aether").is_err());
    }

    #[test]
    fn unknown_kid_is_rejected_but_single_key_without_kid_is_fine() {
        let jwks = Jwks::parse(JWKS_JSON).unwrap();
        let t = mint("mecha-aether", now(), now() + TTL_SECS, Some("rotated-away"));
        assert!(verify(&t, &jwks, "mecha-aether").is_err());
        let t = mint("mecha-aether", now(), now() + TTL_SECS, None);
        assert!(verify(&t, &jwks, "mecha-aether").is_ok());
    }

    #[test]
    fn tampered_payload_fails_signature() {
        let jwks = Jwks::parse(JWKS_JSON).unwrap();
        let t = mint("mecha-aether", now(), now() + TTL_SECS, Some("test-2026-10"));
        let mut parts: Vec<&str> = t.split('.').collect();
        // Forge a payload with a different sub, keep the real signature.
        use base64::Engine;
        let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            format!(r#"{{"iss":"{ISSUER}","sub":"22222222-2222-2222-2222-222222222222","aud":"mecha-aether","username":"tester","jti":"x","iat":{},"exp":{}}}"#, now(), now() + 300),
        );
        parts[1] = &forged;
        let forged_ticket = parts.join(".");
        assert!(verify(&forged_ticket, &jwks, "mecha-aether").is_err());
    }
}
