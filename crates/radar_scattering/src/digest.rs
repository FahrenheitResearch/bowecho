use std::fmt;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// An exact SHA-256 digest serialized as 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    #[must_use]
    pub fn compute(bytes: &[u8]) -> Self {
        let computed = Sha256::digest(bytes);
        let mut digest = [0_u8; 32];
        digest.copy_from_slice(&computed);
        Self(digest)
    }

    pub fn from_hex(value: &str) -> Result<Self, DigestError> {
        if value.len() != 64 {
            return Err(DigestError::Length {
                actual: value.len(),
            });
        }
        if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(DigestError::NotCanonicalLowercase);
        }

        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high =
                decode_nibble(pair[0]).ok_or(DigestError::InvalidHex { index: index * 2 })?;
            let low = decode_nibble(pair[1]).ok_or(DigestError::InvalidHex {
                index: index * 2 + 1,
            })?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Sha256Digest")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

struct DigestVisitor;

impl Visitor<'_> for DigestVisitor {
    type Value = Sha256Digest;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical 64-character lowercase SHA-256 digest")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Sha256Digest::from_hex(value).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(DigestVisitor)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DigestError {
    #[error("SHA-256 text must be 64 characters, got {actual}")]
    Length { actual: usize },
    #[error("SHA-256 text must use canonical lowercase hexadecimal")]
    NotCanonicalLowercase,
    #[error("invalid hexadecimal character at SHA-256 text index {index}")]
    InvalidHex { index: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector_and_serde_are_canonical() {
        let digest = Sha256Digest::compute(b"abc");
        assert_eq!(
            digest.to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(serde_json::from_str::<Sha256Digest>(&json).unwrap(), digest);
    }

    #[test]
    fn digest_parser_rejects_noncanonical_or_malformed_text() {
        assert_eq!(
            Sha256Digest::from_hex(&"0".repeat(63)),
            Err(DigestError::Length { actual: 63 })
        );
        assert_eq!(
            Sha256Digest::from_hex(&"A".repeat(64)),
            Err(DigestError::NotCanonicalLowercase)
        );
        assert_eq!(
            Sha256Digest::from_hex(&format!("{}z", "0".repeat(63))),
            Err(DigestError::InvalidHex { index: 63 })
        );
    }
}
