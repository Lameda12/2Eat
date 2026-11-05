//! Payment abstraction. Amounts are integer SGD cents.
//!
//! Money flow: authorize at order placement, capture when the cook accepts,
//! void if cancelled before capture, refund in full after capture.

mod mock;
mod stripe;

pub use mock::{MockPaymentGateway, PaymentState};
pub use stripe::StripeConnectGateway;

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeRequest {
    pub order_id: Uuid,
    pub amount_cents: i64,
    /// Platform's cut, included in `amount_cents`.
    pub platform_fee_cents: i64,
    /// Cook's Stripe Connect account (`acct_...`) that receives `amount - fee`. `None` in the mock.
    pub cook_account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    /// Opaque handle stored on the order (`payment_ref`).
    pub id: String,
    pub amount_cents: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundReceipt {
    pub id: String,
    pub amount_cents: i64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PaymentError {
    #[error("payment declined: {0}")]
    Declined(String),
    #[error("unknown payment {0}")]
    UnknownPayment(String),
    #[error("invalid payment state: {0}")]
    InvalidState(String),
    #[error("payment provider error: {0}")]
    Provider(String),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

#[async_trait]
pub trait PaymentGateway: Send + Sync {
    /// Place a hold for the full order amount.
    async fn authorize(&self, req: AuthorizeRequest) -> Result<Authorization, PaymentError>;

    /// Capture a previously authorized hold. Idempotent.
    async fn capture(&self, payment_id: &str) -> Result<(), PaymentError>;

    /// Release an uncaptured hold in full. Idempotent.
    async fn void(&self, payment_id: &str) -> Result<(), PaymentError>;

    /// Refund a captured payment. Implementations must return the platform fee
    /// to the customer too (Stripe: `refund_application_fee=true`,
    /// `reverse_transfer=true`). `idempotency_key` makes retries safe.
    async fn refund(&self, payment_id: &str, amount_cents: i64, idempotency_key: &str) -> Result<RefundReceipt, PaymentError>;
}
