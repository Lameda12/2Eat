//! Password hashing: Argon2id with the `argon2` crate's **default** parameters,
//! serialised as PHC strings via `password-hash`. No custom parameters.

use std::sync::OnceLock;

use argon2::Argon2;
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::rngs::OsRng;
use zeroize::Zeroize;

use crate::error::ApiError;

pub const MIN_PASSWORD_CHARS: usize = 10;
pub const MAX_PASSWORD_BYTES: usize = 1024;

fn hash_blocking(password: &str) -> Result<String, password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default().hash_password(password.as_bytes(), &salt)?.to_string())
}

fn verify_blocking(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// A valid hash of a random password, verified against when the account does
/// not exist so "unknown email" costs the same as "wrong password".
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let mut junk = uuid::Uuid::new_v4().to_string();
        let h = hash_blocking(&junk).expect("argon2 with default params cannot fail on a short input");
        junk.zeroize();
        h
    })
}

/// Hash off the async executor; the plaintext copy is wiped afterwards.
pub async fn hash_password(password: &str) -> Result<String, ApiError> {
    let mut pw = password.to_owned();
    let res = tokio::task::spawn_blocking(move || {
        let r = hash_blocking(&pw);
        pw.zeroize();
        r
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    res.map_err(|e| ApiError::Internal(format!("password hashing failed: {e}")))
}

/// Constant-work verification. `phc = None` verifies against a dummy hash and
/// always returns `false`.
pub async fn verify_password(password: &str, phc: Option<&str>) -> bool {
    let mut pw = password.to_owned();
    let (phc, real) = match phc {
        Some(h) => (h.to_owned(), true),
        None => (dummy_hash().to_owned(), false),
    };
    tokio::task::spawn_blocking(move || {
        let ok = verify_blocking(&pw, &phc);
        pw.zeroize();
        ok && real
    })
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_is_argon2id_phc_and_verifies() {
        let h = hash_password("correct horse battery").await.unwrap();
        assert!(h.starts_with("$argon2id$v=19$"), "{h}");
        assert!(verify_password("correct horse battery", Some(&h)).await);
        assert!(!verify_password("wrong password!!", Some(&h)).await);
        assert!(!verify_password("correct horse battery", None).await, "missing account never verifies");
        assert!(!verify_password("x", Some("not a phc string")).await);
    }

    #[tokio::test]
    async fn salts_differ() {
        let a = hash_password("same password 1").await.unwrap();
        let b = hash_password("same password 1").await.unwrap();
        assert_ne!(a, b);
    }
}
