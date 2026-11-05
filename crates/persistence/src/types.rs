use chrono::{DateTime, Utc};
use twoeat_domain::{RefundInitiator, RefundReason, RefundRecord};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Total rows matching the filter, ignoring limit/offset.
    pub total: i64,
}

/// One append-only audit row. Written in the same transaction as the mutation it describes.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    /// `None` = system action.
    pub actor_id: Option<Uuid>,
    /// e.g. `certificate.approved`, `refund.requested`, `cook.flagged_for_review`.
    pub action: String,
    pub target_type: String,
    pub target_id: Uuid,
    pub metadata: serde_json::Value,
    pub at: DateTime<Utc>,
}

impl AuditEntry {
    pub fn new(
        actor_id: Option<Uuid>,
        action: &str,
        target_type: &str,
        target_id: Uuid,
        metadata: serde_json::Value,
        at: DateTime<Utc>,
    ) -> Self {
        Self { actor_id, action: action.to_owned(), target_type: target_type.to_owned(), target_id, metadata, at }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    pub id: Uuid,
    pub entry: AuditEntry,
}

#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub target_type: Option<String>,
    pub target_id: Option<Uuid>,
    pub action: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewRefund {
    pub order_id: Uuid,
    pub initiated_by: Uuid,
    pub initiator: RefundInitiator,
    pub reason: RefundReason,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefundOutcome {
    pub record: RefundRecord,
    /// `false` when a refund request already existed for the order (idempotent retry).
    pub created: bool,
    /// The order was still `Placed`, so it was cancelled in the same transaction;
    /// the caller must now void the payment authorization.
    pub order_cancelled: bool,
}
