//! Refund governance: who decides a refund, and the swappable fraud policy.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::order::RefundReason;

/// Fraud-control thresholds. These are **platform policy, not law**: tune them
/// freely, they live in one place on purpose.
pub mod policy {
    /// Trailing window for both counters.
    pub const WINDOW_DAYS: i64 = 30;
    /// An eater with fewer than this many refunds in the window is auto-approved.
    pub const EATER_AUTO_APPROVE_BELOW: u32 = 3;
    /// A cook with MORE than this many refunds against them in the window is flagged for admin review.
    pub const COOK_FLAG_ABOVE: u32 = 5;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundDecision {
    Auto,
    AdminApproved,
    AdminDenied,
}

impl RefundDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::AdminApproved => "admin_approved",
            Self::AdminDenied => "admin_denied",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "admin_approved" => Self::AdminApproved,
            "admin_denied" => Self::AdminDenied,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundStatus {
    PendingReview,
    Approved,
    Denied,
}

impl RefundStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PendingReview => "pending_review",
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending_review" => Self::PendingReview,
            "approved" => Self::Approved,
            "denied" => Self::Denied,
            _ => return None,
        })
    }
}

/// How an approved refund is settled with the payment provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundSettlement {
    /// Captured funds are returned.
    Refund,
    /// Order was never accepted, so the authorization hold is released instead.
    /// Nothing was charged, so these do not count towards fraud windows.
    Void,
}

impl RefundSettlement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Refund => "refund",
            Self::Void => "void",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "refund" => Self::Refund,
            "void" => Self::Void,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundInitiator {
    Eater,
    Cook,
    Admin,
}

/// One refund request per order, ever (a unique constraint backs this up), so
/// an order can never be double-counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundRecord {
    pub id: Uuid,
    pub order_id: Uuid,
    pub initiated_by: Uuid,
    pub reason: RefundReason,
    /// Always the order's full total.
    pub amount_cents: i64,
    pub settlement: RefundSettlement,
    /// `None` while pending admin review.
    pub decision: Option<RefundDecision>,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl RefundRecord {
    pub fn status(&self) -> RefundStatus {
        match self.decision {
            None => RefundStatus::PendingReview,
            Some(RefundDecision::Auto | RefundDecision::AdminApproved) => RefundStatus::Approved,
            Some(RefundDecision::AdminDenied) => RefundStatus::Denied,
        }
    }
}

/// Counts the store gathers (inside the transaction) for the policy's window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefundHistory {
    /// Approved refunds on this eater's other orders in the window.
    pub eater_refunds_in_window: u32,
    /// Approved refunds against this cook in the window, including the current one.
    pub cook_refunds_in_window: u32,
}

/// Swappable refund fraud policy. The store supplies the counts for
/// [`window`](Self::window) so the decision is made atomically with the insert.
pub trait RefundPolicy: Send + Sync {
    fn window(&self) -> Duration;
    fn should_auto_approve(&self, history: &RefundHistory, eater_id: Uuid, cook_id: Uuid, now: DateTime<Utc>) -> bool;
    fn should_flag_cook(&self, history: &RefundHistory, cook_id: Uuid, now: DateTime<Utc>) -> bool;
}

/// Default policy: see [`policy`] for the numbers.
#[derive(Debug, Clone, Copy)]
pub struct WindowedRefundPolicy {
    pub window: Duration,
    pub eater_auto_approve_below: u32,
    pub cook_flag_above: u32,
}

impl Default for WindowedRefundPolicy {
    fn default() -> Self {
        Self {
            window: Duration::days(policy::WINDOW_DAYS),
            eater_auto_approve_below: policy::EATER_AUTO_APPROVE_BELOW,
            cook_flag_above: policy::COOK_FLAG_ABOVE,
        }
    }
}

impl RefundPolicy for WindowedRefundPolicy {
    fn window(&self) -> Duration {
        self.window
    }
    fn should_auto_approve(&self, h: &RefundHistory, _eater: Uuid, _cook: Uuid, _now: DateTime<Utc>) -> bool {
        h.eater_refunds_in_window < self.eater_auto_approve_below
    }
    fn should_flag_cook(&self, h: &RefundHistory, _cook: Uuid, _now: DateTime<Utc>) -> bool {
        h.cook_refunds_in_window > self.cook_flag_above
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(e: u32, c: u32) -> RefundHistory {
        RefundHistory { eater_refunds_in_window: e, cook_refunds_in_window: c }
    }

    #[test]
    fn eater_threshold_boundary() {
        let p = WindowedRefundPolicy::default();
        let (e, c, now) = (Uuid::new_v4(), Uuid::new_v4(), Utc::now());
        assert!(p.should_auto_approve(&h(0, 0), e, c, now));
        assert!(p.should_auto_approve(&h(2, 0), e, c, now), "third refund is still automatic");
        assert!(!p.should_auto_approve(&h(3, 0), e, c, now), "fourth refund goes to review");
        assert!(!p.should_auto_approve(&h(10, 0), e, c, now));
    }

    #[test]
    fn cook_flag_boundary() {
        let p = WindowedRefundPolicy::default();
        let (c, now) = (Uuid::new_v4(), Utc::now());
        assert!(!p.should_flag_cook(&h(0, 5), c, now), "exactly 5 is not 'more than 5'");
        assert!(p.should_flag_cook(&h(0, 6), c, now));
    }

    #[test]
    fn status_derives_from_decision() {
        let mut r = RefundRecord {
            id: Uuid::new_v4(),
            order_id: Uuid::new_v4(),
            initiated_by: Uuid::new_v4(),
            reason: RefundReason::Other,
            amount_cents: 100,
            settlement: RefundSettlement::Refund,
            decision: None,
            decided_by: None,
            decided_at: None,
            created_at: Utc::now(),
        };
        assert_eq!(r.status(), RefundStatus::PendingReview);
        r.decision = Some(RefundDecision::Auto);
        assert_eq!(r.status(), RefundStatus::Approved);
        r.decision = Some(RefundDecision::AdminApproved);
        assert_eq!(r.status(), RefundStatus::Approved);
        r.decision = Some(RefundDecision::AdminDenied);
        assert_eq!(r.status(), RefundStatus::Denied);
    }
}
