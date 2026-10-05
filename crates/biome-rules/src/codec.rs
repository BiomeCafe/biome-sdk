//! Binary encoding for the game's own types (`Config`, `State`, `Move`,
//! `View`, `Event`): postcard. Compact, deterministic, the same bytes on
//! every target. Clients use these helpers to build move bytes and to read
//! views and events.

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Encode a value. Only fails for types serde itself can't serialize, which
/// a rules crate should treat as a bug.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    postcard::to_allocvec(value).map_err(|e| e.to_string())
}

/// Decode a value, requiring the whole buffer to be consumed.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let (value, rest) = postcard::take_from_bytes::<T>(bytes).map_err(|e| e.to_string())?;
    if !rest.is_empty() {
        return Err(format!("{} trailing byte(s)", rest.len()));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Sample {
        a: u8,
        b: Vec<u16>,
        c: Option<String>,
    }

    #[test]
    fn roundtrip() {
        let s = Sample { a: 7, b: vec![1, 300], c: Some("hi".into()) };
        assert_eq!(decode::<Sample>(&encode(&s).unwrap()).unwrap(), s);
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = encode(&7u8).unwrap();
        bytes.push(0);
        assert!(decode::<u8>(&bytes).is_err());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode::<Sample>(&[0xff; 3]).is_err());
        assert!(decode::<Sample>(&[]).is_err());
    }
}
