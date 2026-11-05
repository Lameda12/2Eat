//! Persistence ports and adapters.
//!
//! * [`Store`]: cooks, dishes, orders, reviews, refunds, audit.
//! * [`AuthStore`]: users and sessions.
//! * [`CookLocationRepo`]: geospatial discovery.
//!
//! Each has a PostgreSQL adapter ([`PgStore`]) and an in-memory twin
//! ([`MemoryStore`]) used for fast tests and local runs without a database.

mod auth;
mod location;
mod memory;
mod pg;
mod types;

pub use auth::{AuthStore, NewSession, NewUser, SessionRecord, UserRecord};
#[cfg(feature = "postgis")]
pub use location::PostgisCookLocationRepo;
pub use location::{CookLocationRepo, HaversineCookLocationRepo};
pub use memory::MemoryStore;
pub use pg::PgStore;
pub use types::{AuditEntry, AuditFilter, AuditRecord, NewRefund, Page, RefundOutcome};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use twoeat_domain::{
    CertDecision, CertificationStatus, CertificationSubmission, Cook, Dish, DomainError, Order, RefundPolicy, RefundRecord, RefundStatus,
};
use twoeat_geo::NearbyQuery;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// State conflict (duplicate email, already-decided review, ...).
    #[error("{0}")]
    Conflict(&'static str),
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
    // ---- cooks & certificate reviews -------------------------------------
    /// Persist a freshly registered cook together with their certificate upload.
    async fn register_cook(&self, cook: &Cook, submission: &CertificationSubmission, terms_accepted_at: DateTime<Utc>) -> StoreResult<()>;
    async fn get_cook(&self, id: Uuid) -> StoreResult<Cook>;
    async fn latest_submission(&self, cook_id: Uuid) -> StoreResult<Option<CertificationSubmission>>;

    async fn get_review(&self, id: Uuid) -> StoreResult<CertificationSubmission>;
    /// Oldest first (review queue order).
    async fn list_reviews(
        &self,
        status: Option<CertificationStatus>,
        limit: i64,
        offset: i64,
    ) -> StoreResult<Page<CertificationSubmission>>;
    /// Decide a pending review. Locks cook + review, and writes the audit entry
    /// in the same transaction. `Conflict` if already decided.
    async fn decide_review(
        &self,
        review_id: Uuid,
        actor: Option<Uuid>,
        decision: CertDecision,
        notes: Option<String>,
        now: DateTime<Utc>,
    ) -> StoreResult<(CertificationSubmission, Cook)>;
    /// Pull a cook off the platform, audited.
    async fn revoke_cook(&self, actor: Option<Uuid>, cook_id: Uuid, now: DateTime<Utc>) -> StoreResult<Cook>;

    /// Phase 1 entry point: decide the cook's latest pending review (system actor).
    async fn review_certification(&self, cook_id: Uuid, decision: CertDecision, now: DateTime<Utc>) -> StoreResult<Cook>;
    /// Phase 1 entry point: `revoke_cook` with a system actor.
    async fn revoke_verification(&self, cook_id: Uuid) -> StoreResult<Cook>;

    /// Verified cooks with an unexpired certificate within the radius, nearest first.
    /// Kept for compatibility; [`CookLocationRepo`] is the extension point.
    async fn nearby_cooks(&self, query: &NearbyQuery, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>>;

    // ---- dishes -----------------------------------------------------------
    async fn insert_dish(&self, dish: &Dish) -> StoreResult<()>;
    async fn get_dish(&self, id: Uuid) -> StoreResult<Dish>;
    async fn list_dishes(&self, cook_id: Uuid) -> StoreResult<Vec<Dish>>;
    /// Dishes flagged by the raw-seafood keyword scan, awaiting an admin.
    async fn list_flagged_dishes(&self, limit: i64, offset: i64) -> StoreResult<Page<Dish>>;
    async fn decide_dish_review(&self, dish_id: Uuid, actor: Uuid, approve: bool, now: DateTime<Utc>) -> StoreResult<Dish>;

    // ---- orders -----------------------------------------------------------
    async fn insert_order(&self, order: &Order) -> StoreResult<()>;
    async fn get_order(&self, id: Uuid) -> StoreResult<Order>;

    /// Lock the order's cook (shared) then the order (exclusive), apply `f`,
    /// persist, and return the updated order. All-or-nothing.
    ///
    /// Independently of `f`, an order may only ENTER `AcceptedByCook` if the
    /// cook's latest decided certificate review is `Approved`; otherwise the
    /// call fails with `CertificationNotVerified` and nothing is persisted.
    async fn transition_order(&self, id: Uuid, f: OrderMutation) -> StoreResult<Order>;

    // ---- refunds ----------------------------------------------------------
    /// Record a refund request for an order, once. Idempotent: a second call
    /// returns the existing request with `created = false`.
    ///
    /// Decides auto-approve vs admin review via `policy` using counts taken in
    /// the same transaction (serialised per eater), writes the audit entry
    /// there too, and, for an unaccepted (`Placed`) order, cancels it in that
    /// transaction (nothing was captured).
    async fn request_refund(&self, req: NewRefund, policy: &dyn RefundPolicy) -> StoreResult<RefundOutcome>;
    async fn get_refund(&self, id: Uuid) -> StoreResult<RefundRecord>;
    async fn get_refund_for_order(&self, order_id: Uuid) -> StoreResult<Option<RefundRecord>>;
    async fn list_refunds(&self, status: Option<RefundStatus>, limit: i64, offset: i64) -> StoreResult<Page<RefundRecord>>;
    /// Admin approves or denies a pending refund (audited). `Conflict` if not pending.
    async fn decide_refund(
        &self,
        refund_id: Uuid,
        admin: Uuid,
        approve: bool,
        notes: Option<String>,
        now: DateTime<Utc>,
        policy: &dyn RefundPolicy,
    ) -> StoreResult<RefundRecord>;
    /// Mark an approved `Refund`-settlement refund as paid out: moves the order
    /// to `Refunded` for the FULL total. Idempotent if already settled. Call
    /// only after the payment provider confirmed the refund.
    async fn settle_refund(&self, refund_id: Uuid, now: DateTime<Utc>) -> StoreResult<(RefundRecord, Order)>;

    // ---- audit ------------------------------------------------------------
    async fn audit_entries(&self, filter: AuditFilter, limit: i64) -> StoreResult<Vec<AuditRecord>>;
}
