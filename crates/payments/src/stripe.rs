use async_trait::async_trait;

use crate::{Authorization, AuthorizeRequest, PaymentError, PaymentGateway, RefundReceipt};

/// Stripe Connect gateway: **stub**.
///
/// Intended wiring (not implemented):
/// * `authorize`: PaymentIntent with `capture_method=manual`, `currency=sgd`,
///   `transfer_data[destination]=<cook acct>` and `application_fee_amount=<platform fee>`.
/// * `capture`: `POST /v1/payment_intents/{id}/capture`.
/// * `void`: `POST /v1/payment_intents/{id}/cancel`.
/// * `refund`: `POST /v1/refunds` with `refund_application_fee=true` and
///   `reverse_transfer=true` so the platform fee is returned too, using the
///   `Idempotency-Key` header.
#[derive(Debug, Default, Clone)]
pub struct StripeConnectGateway {
    #[allow(dead_code)]
    secret_key: String,
}

impl StripeConnectGateway {
    pub fn new(secret_key: impl Into<String>) -> Self {
        Self { secret_key: secret_key.into() }
    }
}

const MSG: &str = "StripeConnectGateway is a stub; use MockPaymentGateway";

#[async_trait]
impl PaymentGateway for StripeConnectGateway {
    async fn authorize(&self, _req: AuthorizeRequest) -> Result<Authorization, PaymentError> {
        Err(PaymentError::NotImplemented(MSG))
    }
    async fn capture(&self, _payment_id: &str) -> Result<(), PaymentError> {
        Err(PaymentError::NotImplemented(MSG))
    }
    async fn void(&self, _payment_id: &str) -> Result<(), PaymentError> {
        Err(PaymentError::NotImplemented(MSG))
    }
    async fn refund(&self, _payment_id: &str, _amount_cents: i64, _key: &str) -> Result<RefundReceipt, PaymentError> {
        Err(PaymentError::NotImplemented(MSG))
    }
}
