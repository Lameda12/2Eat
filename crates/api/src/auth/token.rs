//! Opaque bearer tokens: 32 random bytes from the OS CSPRNG, base64url-encoded.
//! Only the SHA-256 digest is ever stored.

use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const TOKEN_BYTES: usize = 32;
/// Sessions last 30 days.
pub const SESSION_TTL_DAYS: i64 = 30;

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct BearerToken(String);

impl BearerToken {
    pub fn generate() -> Self {
        let mut raw = [0u8; TOKEN_BYTES];
        OsRng.fill_bytes(&mut raw);
        let encoded = URL_SAFE_NO_PAD.encode(raw);
        raw.zeroize();
        Self(encoded)
    }

    /// The only way to read the token: put it in the one response that hands it to the client.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn digest(&self) -> [u8; 32] {
        digest_of(&self.0)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

pub fn digest_of(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_random_bytes_base64url() {
        let a = BearerToken::generate();
        let b = BearerToken::generate();
        assert_ne!(a.expose(), b.expose());
        let raw = URL_SAFE_NO_PAD.decode(a.expose()).unwrap();
        assert_eq!(raw.len(), 32);
        assert!(a.expose().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn digest_is_sha256_of_the_encoded_token_and_not_the_token() {
        let t = BearerToken::generate();
        let d = t.digest();
        assert_eq!(d, digest_of(t.expose()));
        assert_ne!(&d[..], t.expose().as_bytes());
        // Known-answer test for SHA-256("abc").
        assert_eq!(digest_of("abc")[..4], [0xba, 0x78, 0x16, 0xbf]);
    }

    #[test]
    fn debug_never_prints_the_token() {
        let t = BearerToken::generate();
        assert!(!format!("{t:?}").contains(t.expose()));
    }
}
