//! Persistence ports and adapters.
//!
//! [`Store`] is the only thing the API depends on. [`PgStore`] is the real
//! PostgreSQL adapter; [`MemoryStore`] is a faithful in-memory twin used for
//! fast tests and local runs without a database.

mod memory;
mod pg;

pub use memory::MemoryStore;
pub use pg::PgStore;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use twoeat_domain::{CertDecision, CertificationSubmission, Cook, Dish, DomainError, Order};
use twoeat_geo::NearbyQuery;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error(transparent)]
    Domain(#[from] DomainError),
    #[error("database error: {0}")]
    Db(String),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Db(e.to_string())
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A cook as shown in discovery. Exact coordinates are deliberately absent.
#[derive(Debug, Clone, PartialEq)]
pub struct NearbyCook {
    pub id: Uuid,
    pub display_name: String,
    pub distance_m: f64,
}

/// Domain mutation applied to an order while the cook row (shared) and the
/// order row (exclusive) are locked. Receives the cook as of lock time, so
/// checks like [`Cook::can_sell`] can't race with a concurrent revocation.
pub type OrderMutation = Box<dyn FnOnce(&Cook, &mut Order) -> Result<(), DomainError> + Send>;

#[async_trait]
pub trait Store: Send + Sync {
    /// Persist a freshly registered cook together with their certificate upload.
    async fn register_cook(&self, cook: &Cook, submission: &CertificationSubmission, terms_accepted_at: DateTime<Utc>) -> StoreResult<()>;
    async fn get_cook(&self, id: Uuid) -> StoreResult<Cook>;
    async fn latest_submission(&self, cook_id: Uuid) -> StoreResult<Option<CertificationSubmission>>;

    /// Approve or reject the cook's pending certificate (locks the cook row).
    async fn review_certification(&self, cook_id: Uuid, decision: CertDecision, now: DateTime<Utc>) -> StoreResult<Cook>;
    /// Pull a cook off the platform (locks the cook row).
    async fn revoke_verification(&self, cook_id: Uuid) -> StoreResult<Cook>;

    /// Verified cooks with an unexpired certificate within the radius, nearest first.
    async fn nearby_cooks(&self, query: &NearbyQuery, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>>;

    async fn insert_dish(&self, dish: &Dish) -> StoreResult<()>;
    async fn get_dish(&self, id: Uuid) -> StoreResult<Dish>;
    async fn list_dishes(&self, cook_id: Uuid) -> StoreResult<Vec<Dish>>;

    async fn insert_order(&self, order: &Order) -> StoreResult<()>;
    async fn get_order(&self, id: Uuid) -> StoreResult<Order>;

    /// Lock the order's cook (shared) then the order (exclusive), apply `f`,
    /// persist, and return the updated order. All-or-nothing.
    async fn transition_order(&self, id: Uuid, f: OrderMutation) -> StoreResult<Order>;
}
