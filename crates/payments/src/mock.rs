use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use uuid::Uuid;

use crate::{Authorization, AuthorizeRequest, PaymentError, PaymentGateway, RefundReceipt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentState {
    Authorized,
    Captured,
    Voided,
    Refunded,
}

#[derive(Debug)]
struct Payment {
    amount_cents: i64,
    state: PaymentState,
    refunded_cents: Option<i64>,
}

/// In-memory gateway for dev and tests. Enforces the same state rules a real
/// provider would: no capture after void, refund only after capture, refund
/// at most the captured amount, refunds idempotent per key.
#[derive(Debug, Default)]
pub struct MockPaymentGateway {
    payments: Mutex<HashMap<String, Payment>>,
    refunds: Mutex<HashMap<String, RefundReceipt>>,
    decline_authorizations: AtomicBool,
    fail_captures: AtomicBool,
}

impl MockPaymentGateway {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test hook: make every following `authorize` decline.
    pub fn set_decline_authorizations(&self, v: bool) {
        self.decline_authorizations.store(v, Ordering::SeqCst);
    }

    /// Test hook: make every following `capture` fail.
    pub fn set_fail_captures(&self, v: bool) {
        self.fail_captures.store(v, Ordering::SeqCst);
    }

    /// Test hook: how much the provider was asked to return for this payment.
    pub fn refunded_cents(&self, payment_id: &str) -> Option<i64> {
        self.payments.lock().unwrap().get(payment_id).and_then(|p| p.refunded_cents)
    }

    pub fn state_of(&self, payment_id: &str) -> Option<PaymentState> {
        self.payments.lock().unwrap().get(payment_id).map(|p| p.state)
    }
}

#[async_trait]
impl PaymentGateway for MockPaymentGateway {
    async fn authorize(&self, req: AuthorizeRequest) -> Result<Authorization, PaymentError> {
        if self.decline_authorizations.load(Ordering::SeqCst) {
            return Err(PaymentError::Declined("card declined (mock)".into()));
        }
        if req.amount_cents <= 0 || req.platform_fee_cents < 0 || req.platform_fee_cents > req.amount_cents {
            return Err(PaymentError::InvalidState("bad amounts".into()));
        }
        let id = format!("mock_pi_{}", Uuid::new_v4().simple());
        self.payments
            .lock()
            .unwrap()
            .insert(id.clone(), Payment { amount_cents: req.amount_cents, state: PaymentState::Authorized, refunded_cents: None });
        Ok(Authorization { id, amount_cents: req.amount_cents })
    }

    async fn capture(&self, payment_id: &str) -> Result<(), PaymentError> {
        if self.fail_captures.load(Ordering::SeqCst) {
            return Err(PaymentError::Provider("capture failed (mock)".into()));
        }
        let mut g = self.payments.lock().unwrap();
        let p = g.get_mut(payment_id).ok_or_else(|| PaymentError::UnknownPayment(payment_id.into()))?;
        match p.state {
            PaymentState::Authorized => {
                p.state = PaymentState::Captured;
                Ok(())
            }
            PaymentState::Captured => Ok(()),
            s => Err(PaymentError::InvalidState(format!("cannot capture a {s:?} payment"))),
        }
    }

    async fn void(&self, payment_id: &str) -> Result<(), PaymentError> {
        let mut g = self.payments.lock().unwrap();
        let p = g.get_mut(payment_id).ok_or_else(|| PaymentError::UnknownPayment(payment_id.into()))?;
        match p.state {
            PaymentState::Authorized => {
                p.state = PaymentState::Voided;
                Ok(())
            }
            PaymentState::Voided => Ok(()),
            s => Err(PaymentError::InvalidState(format!("cannot void a {s:?} payment"))),
        }
    }

    async fn refund(&self, payment_id: &str, amount_cents: i64, idempotency_key: &str) -> Result<RefundReceipt, PaymentError> {
        if let Some(r) = self.refunds.lock().unwrap().get(idempotency_key) {
            return Ok(r.clone());
        }
        let mut g = self.payments.lock().unwrap();
        let p = g.get_mut(payment_id).ok_or_else(|| PaymentError::UnknownPayment(payment_id.into()))?;
        if p.state != PaymentState::Captured {
            return Err(PaymentError::InvalidState(format!("cannot refund a {:?} payment", p.state)));
        }
        if amount_cents != p.amount_cents {
            return Err(PaymentError::InvalidState(format!(
                "only full refunds are supported (captured {}, requested {amount_cents})",
                p.amount_cents
            )));
        }
        p.state = PaymentState::Refunded;
        p.refunded_cents = Some(amount_cents);
        let receipt = RefundReceipt { id: format!("mock_re_{}", Uuid::new_v4().simple()), amount_cents };
        self.refunds.lock().unwrap().insert(idempotency_key.to_owned(), receipt.clone());
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(amount: i64) -> AuthorizeRequest {
        AuthorizeRequest { order_id: Uuid::new_v4(), amount_cents: amount, platform_fee_cents: 50, cook_account: None }
    }

    #[tokio::test]
    async fn authorize_capture_refund_full_amount_including_fee() {
        let g = MockPaymentGateway::new();
        let a = g.authorize(req(1050)).await.unwrap();
        assert!(g.refund(&a.id, 1050, "k").await.is_err(), "no refund before capture");
        g.capture(&a.id).await.unwrap();
        g.capture(&a.id).await.unwrap();
        assert!(g.refund(&a.id, 1000, "k2").await.is_err(), "partial refunds rejected");
        let r1 = g.refund(&a.id, 1050, "k").await.unwrap();
        let r2 = g.refund(&a.id, 1050, "k").await.unwrap();
        assert_eq!(r1, r2, "idempotent per key");
        assert_eq!(g.state_of(&a.id), Some(PaymentState::Refunded));
    }

    #[tokio::test]
    async fn void_releases_hold_and_blocks_capture() {
        let g = MockPaymentGateway::new();
        let a = g.authorize(req(500)).await.unwrap();
        g.void(&a.id).await.unwrap();
        g.void(&a.id).await.unwrap();
        assert!(g.capture(&a.id).await.is_err());
        assert!(g.refund(&a.id, 500, "k").await.is_err());
    }

    #[tokio::test]
    async fn decline_and_unknown() {
        let g = MockPaymentGateway::new();
        g.set_decline_authorizations(true);
        assert!(matches!(g.authorize(req(500)).await, Err(PaymentError::Declined(_))));
        assert!(matches!(g.capture("nope").await, Err(PaymentError::UnknownPayment(_))));
    }
}
