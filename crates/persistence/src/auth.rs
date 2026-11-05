//! Identity storage: users and bearer-token sessions.
//!
//! Structs that carry a password hash or a token digest derive
//! `Zeroize`/`ZeroizeOnDrop` and have redacting `Debug` impls so they cannot
//! leak through logs.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use twoeat_domain::Role;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::StoreResult;

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct NewUser {
    #[zeroize(skip)]
    pub id: Uuid,
    /// Already normalised (trimmed, lower-cased) by the caller.
    #[zeroize(skip)]
    pub email: String,
    /// Argon2id PHC string.
    pub password_hash: String,
    #[zeroize(skip)]
    pub role: Role,
    #[zeroize(skip)]
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct UserRecord {
    #[zeroize(skip)]
    pub id: Uuid,
    #[zeroize(skip)]
    pub email: String,
    pub password_hash: String,
    #[zeroize(skip)]
    pub role: Role,
    #[zeroize(skip)]
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct NewSession {
    #[zeroize(skip)]
    pub id: Uuid,
    #[zeroize(skip)]
    pub user_id: Uuid,
    /// SHA-256 of the opaque token. The token itself is never stored.
    pub token_hash: [u8; 32],
    #[zeroize(skip)]
    pub expires_at: DateTime<Utc>,
    #[zeroize(skip)]
    pub created_at: DateTime<Utc>,
}

/// A looked-up session joined with its user's role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub role: Role,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl SessionRecord {
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

macro_rules! redacted_debug {
    ($t:ty, $name:literal) => {
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct($name).field("id", &self.id).finish_non_exhaustive()
            }
        }
    };
}
redacted_debug!(NewUser, "NewUser");
redacted_debug!(UserRecord, "UserRecord");
redacted_debug!(NewSession, "NewSession");

#[async_trait]
pub trait AuthStore: Send + Sync {
    /// `StoreError::Conflict` when the email is already registered (case-insensitive).
    async fn create_user(&self, user: NewUser) -> StoreResult<()>;
    async fn find_user_by_email(&self, email: &str) -> StoreResult<Option<UserRecord>>;
    async fn get_user(&self, id: Uuid) -> StoreResult<UserRecord>;

    async fn create_session(&self, session: NewSession) -> StoreResult<()>;
    async fn find_session(&self, token_hash: &[u8; 32]) -> StoreResult<Option<SessionRecord>>;
    /// Returns whether an active session was revoked.
    async fn revoke_session(&self, session_id: Uuid, now: DateTime<Utc>) -> StoreResult<bool>;
    /// Revoke `old` and create `new` in ONE transaction. `NotFound` if `old`
    /// is no longer active (so two concurrent refreshes cannot both succeed).
    async fn rotate_session(&self, old: Uuid, new: NewSession, now: DateTime<Utc>) -> StoreResult<()>;
}
