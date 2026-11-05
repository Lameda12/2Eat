use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cook::Cook;
use crate::dish::Dish;
use crate::error::{DomainError, RegulatoryViolation};

/// Who physically delivers an order. 2Eat does **not** dispatch third-party
/// couriers in Singapore, so the only variant is the cook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveredBy {
    #[default]
    Cook,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryDetails {
    pub address: String,
    /// Singapore 6-digit postal code.
    pub postal_code: String,
    /// Always the cook; explicit in the type so a courier model cannot appear by accident.
    #[serde(default)]
    pub delivered_by: DeliveredBy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fulfillment {
    Pickup,
    /// Delivery performed **by the cook** (`delivered_by: DeliveredBy::Cook`).
    /// 2Eat has no courier model: no third-party dispatch, routing or tracking.
    Delivery(DeliveryDetails),
}

impl Fulfillment {
    fn validate(&self) -> Result<(), DomainError> {
        if let Fulfillment::Delivery(d) = self {
            if d.address.trim().is_empty() || d.address.chars().count() > 300 {
                return Err(DomainError::validation("delivery address must be 1-300 characters"));
            }
            if d.postal_code.len() != 6 || !d.postal_code.bytes().all(|b| b.is_ascii_digit()) {
                return Err(DomainError::validation("postal_code must be a 6-digit Singapore postal code"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Placed,
    AcceptedByCook,
    ReadyForPickup,
    OutForDelivery,
    Completed,
    /// Released before the cook accepted: the payment authorization is voided in full.
    Cancelled,
    /// Captured funds returned in full (including the platform fee).
    Refunded,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Placed => "placed",
            Self::AcceptedByCook => "accepted_by_cook",
            Self::ReadyForPickup => "ready_for_pickup",
            Self::OutForDelivery => "out_for_delivery",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Refunded => "refunded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "placed" => Self::Placed,
            "accepted_by_cook" => Self::AcceptedByCook,
            "ready_for_pickup" => Self::ReadyForPickup,
            "out_for_delivery" => Self::OutForDelivery,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "refunded" => Self::Refunded,
            _ => return None,
        })
    }
}

/// What the eater declares the food is for. Anything but personal consumption
/// is prohibited for home-based food businesses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderPurpose {
    #[default]
    PersonalConsumption,
    Catering,
    ResaleToFoodRetailer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderLine {
    pub dish_id: Uuid,
    /// Snapshot at order time so later edits don't rewrite history.
    pub dish_name: String,
    pub unit_price_cents: i64,
    pub quantity: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedLine {
    pub dish_id: Uuid,
    pub quantity: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    pub eater_id: Uuid,
    pub items: Vec<RequestedLine>,
    pub fulfillment: Fulfillment,
    pub purpose: OrderPurpose,
}

/// Platform pricing and guard-rail knobs. `max_portions_per_order` is a
/// platform policy used as a catering heuristic, not a statutory figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingPolicy {
    pub platform_fee_bps: u32,
    pub delivery_fee_cents: i64,
    pub max_portions_per_order: u32,
}

impl Default for PricingPolicy {
    fn default() -> Self {
        Self { platform_fee_bps: 500, delivery_fee_cents: 500, max_portions_per_order: 20 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefundReason {
    Unfulfilled,
    IncorrectOrder,
    NotReceived,
    CookCancelled,
    Other,
}

impl RefundReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unfulfilled => "unfulfilled",
            Self::IncorrectOrder => "incorrect_order",
            Self::NotReceived => "not_received",
            Self::CookCancelled => "cook_cancelled",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "unfulfilled" => Self::Unfulfilled,
            "incorrect_order" => Self::IncorrectOrder,
            "not_received" => Self::NotReceived,
            "cook_cancelled" => Self::CookCancelled,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refund {
    /// Always equal to the order total (items + delivery + platform fee).
    pub amount_cents: i64,
    pub reason: RefundReason,
    pub refunded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: Uuid,
    pub eater_id: Uuid,
    pub cook_id: Uuid,
    pub fulfillment: Fulfillment,
    pub lines: Vec<OrderLine>,
    pub subtotal_cents: i64,
    pub platform_fee_cents: i64,
    pub delivery_fee_cents: i64,
    pub total_cents: i64,
    pub status: OrderStatus,
    /// Opaque payment-gateway handle (authorization id).
    pub payment_ref: Option<String>,
    pub refund: Option<Refund>,
    pub placed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Order {
    /// Build a new order in `Placed`. `catalogue` must contain the dishes
    /// being ordered (extra dishes are ignored).
    pub fn place(cook: &Cook, catalogue: &[Dish], new: NewOrder, policy: &PricingPolicy, now: DateTime<Utc>) -> Result<Self, DomainError> {
        cook.ensure_can_sell(now)?;

        match new.purpose {
            OrderPurpose::PersonalConsumption => {}
            OrderPurpose::Catering => return Err(RegulatoryViolation::Catering.into()),
            OrderPurpose::ResaleToFoodRetailer => return Err(RegulatoryViolation::SaleToFoodRetailer.into()),
        }
        new.fulfillment.validate()?;

        if new.items.is_empty() {
            return Err(DomainError::validation("order must contain at least one item"));
        }

        // Merge duplicate dish ids, keeping first-seen order.
        let mut wanted: Vec<RequestedLine> = Vec::with_capacity(new.items.len());
        for item in &new.items {
            if item.quantity == 0 {
                return Err(DomainError::validation("quantity must be at least 1"));
            }
            match wanted.iter_mut().find(|w| w.dish_id == item.dish_id) {
                Some(w) => w.quantity = w.quantity.checked_add(item.quantity).ok_or_else(overflow)?,
                None => wanted.push(*item),
            }
        }

        let portions: u64 = wanted.iter().map(|w| u64::from(w.quantity)).sum();
        if portions > u64::from(policy.max_portions_per_order) {
            // Bulk orders look like catering; reject rather than guess.
            return Err(RegulatoryViolation::Catering.into());
        }

        let mut lines = Vec::with_capacity(wanted.len());
        let mut subtotal: i64 = 0;
        for w in &wanted {
            let dish = catalogue
                .iter()
                .find(|d| d.id == w.dish_id)
                .ok_or_else(|| DomainError::validation(format!("unknown dish {}", w.dish_id)))?;
            if dish.cook_id != cook.id {
                return Err(DomainError::validation("all dishes must belong to the same cook"));
            }
            if !dish.is_active {
                return Err(DomainError::validation(format!("dish '{}' is not available", dish.name)));
            }
            if dish.is_catering {
                return Err(RegulatoryViolation::Catering.into());
            }
            if dish.is_raw_seafood {
                return Err(RegulatoryViolation::RawSeafood.into());
            }
            let line_total = dish.price_cents.checked_mul(i64::from(w.quantity)).ok_or_else(overflow)?;
            subtotal = subtotal.checked_add(line_total).ok_or_else(overflow)?;
            lines.push(OrderLine {
                dish_id: dish.id,
                dish_name: dish.name.clone(),
                unit_price_cents: dish.price_cents,
                quantity: w.quantity,
            });
        }

        // Round half up.
        let platform_fee =
            subtotal.checked_mul(i64::from(policy.platform_fee_bps)).ok_or_else(overflow)?.checked_add(5_000).ok_or_else(overflow)?
                / 10_000;
        let delivery_fee = match new.fulfillment {
            Fulfillment::Pickup => 0,
            Fulfillment::Delivery(_) => policy.delivery_fee_cents,
        };
        let total = subtotal.checked_add(platform_fee).and_then(|t| t.checked_add(delivery_fee)).ok_or_else(overflow)?;

        Ok(Self {
            id: Uuid::new_v4(),
            eater_id: new.eater_id,
            cook_id: cook.id,
            fulfillment: new.fulfillment,
            lines,
            subtotal_cents: subtotal,
            platform_fee_cents: platform_fee,
            delivery_fee_cents: delivery_fee,
            total_cents: total,
            status: OrderStatus::Placed,
            payment_ref: None,
            refund: None,
            placed_at: now,
            updated_at: now,
        })
    }

    pub fn attach_payment(&mut self, payment_ref: String) {
        self.payment_ref = Some(payment_ref);
    }

    /// Cook accepts. Only allowed while the cook's certificate is verified and current.
    pub fn accept(&mut self, cook: &Cook, now: DateTime<Utc>) -> Result<(), DomainError> {
        self.require(self.status == OrderStatus::Placed, "accept")?;
        if cook.id != self.cook_id {
            return Err(DomainError::validation("cook does not own this order"));
        }
        cook.ensure_can_sell(now)?;
        self.set(OrderStatus::AcceptedByCook, now);
        Ok(())
    }

    /// Food is ready: `ReadyForPickup` for pickup orders, `OutForDelivery` for delivery orders.
    pub fn mark_ready(&mut self, now: DateTime<Utc>) -> Result<(), DomainError> {
        self.require(self.status == OrderStatus::AcceptedByCook, "mark ready")?;
        let next = match self.fulfillment {
            Fulfillment::Pickup => OrderStatus::ReadyForPickup,
            Fulfillment::Delivery(_) => OrderStatus::OutForDelivery,
        };
        self.set(next, now);
        Ok(())
    }

    pub fn complete(&mut self, now: DateTime<Utc>) -> Result<(), DomainError> {
        self.require(matches!(self.status, OrderStatus::ReadyForPickup | OrderStatus::OutForDelivery), "complete")?;
        self.set(OrderStatus::Completed, now);
        Ok(())
    }

    /// Cancel before any money has been captured. The caller voids the
    /// authorization in full. Allowed from `Placed`; also from `AcceptedByCook`
    /// solely so a failed capture right after acceptance can be unwound.
    /// Once food is being prepared and paid for, use [`refund`](Self::refund).
    pub fn cancel(&mut self, now: DateTime<Utc>) -> Result<(), DomainError> {
        self.require(matches!(self.status, OrderStatus::Placed | OrderStatus::AcceptedByCook), "cancel")?;
        self.set(OrderStatus::Cancelled, now);
        Ok(())
    }

    /// Whether [`refund`](Self::refund) would succeed, without mutating.
    pub fn check_refundable(&self, reason: RefundReason) -> Result<(), DomainError> {
        match self.status {
            OrderStatus::AcceptedByCook | OrderStatus::ReadyForPickup | OrderStatus::OutForDelivery => Ok(()),
            // After completion only a wrong or missing order is refundable.
            OrderStatus::Completed => match reason {
                RefundReason::IncorrectOrder | RefundReason::NotReceived => Ok(()),
                _ => Err(DomainError::validation("a completed order can only be refunded as incorrect_order or not_received")),
            },
            _ => Err(DomainError::InvalidTransition { from: self.status, action: "refund" }),
        }
    }

    /// Full refund: always the entire total, platform fee included.
    pub fn refund(&mut self, reason: RefundReason, now: DateTime<Utc>) -> Result<Refund, DomainError> {
        self.check_refundable(reason)?;
        let refund = Refund { amount_cents: self.total_cents, reason, refunded_at: now };
        self.refund = Some(refund);
        self.set(OrderStatus::Refunded, now);
        Ok(refund)
    }

    fn require(&self, ok: bool, action: &'static str) -> Result<(), DomainError> {
        if ok { Ok(()) } else { Err(DomainError::InvalidTransition { from: self.status, action }) }
    }

    fn set(&mut self, status: OrderStatus, now: DateTime<Utc>) {
        self.status = status;
        self.updated_at = now;
    }
}

fn overflow() -> DomainError {
    DomainError::validation("order amount too large")
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::{cook::HbfbAcknowledgements, dish::NewDish, location::LatLng};

    struct Fx {
        now: DateTime<Utc>,
        cook: Cook,
        dishes: Vec<Dish>,
    }

    fn fx() -> Fx {
        let now = Utc::now();
        let mut cook =
            Cook::register(Uuid::new_v4(), "Cook", LatLng::new_in_singapore(1.35, 103.82).unwrap(), &HbfbAcknowledgements::all()).unwrap();
        cook.approve_certification(now + Duration::days(90), now).unwrap();
        let dish = Dish::create(
            &cook,
            NewDish { name: "Laksa".into(), description: String::new(), price_cents: 800, is_catering: false, is_raw_seafood: false },
            now,
        )
        .unwrap();
        Fx { now, cook, dishes: vec![dish] }
    }

    fn new_order(f: &Fx, qty: u32, fulfillment: Fulfillment) -> NewOrder {
        NewOrder {
            eater_id: Uuid::new_v4(),
            items: vec![RequestedLine { dish_id: f.dishes[0].id, quantity: qty }],
            fulfillment,
            purpose: OrderPurpose::PersonalConsumption,
        }
    }

    fn place(f: &Fx, qty: u32, fulfillment: Fulfillment) -> Order {
        Order::place(&f.cook, &f.dishes, new_order(f, qty, fulfillment), &PricingPolicy::default(), f.now).unwrap()
    }

    fn delivery() -> Fulfillment {
        Fulfillment::Delivery(DeliveryDetails {
            address: "1 Example Rd #01-01".into(),
            postal_code: "123456".into(),
            delivered_by: DeliveredBy::Cook,
        })
    }

    #[test]
    fn pricing_pickup_and_delivery() {
        let f = fx();
        let p = place(&f, 2, Fulfillment::Pickup);
        assert_eq!((p.subtotal_cents, p.platform_fee_cents, p.delivery_fee_cents, p.total_cents), (1600, 80, 0, 1680));
        let d = place(&f, 2, delivery());
        assert_eq!((d.delivery_fee_cents, d.total_cents), (500, 2180));
    }

    #[test]
    fn fee_rounds_half_up() {
        let f = fx();
        let mut f2 = f;
        f2.dishes[0].price_cents = 10; // 5% of 10 = 0.5 -> 1
        assert_eq!(place(&f2, 1, Fulfillment::Pickup).platform_fee_cents, 1);
    }

    #[test]
    fn pickup_happy_path() {
        let f = fx();
        let mut o = place(&f, 1, Fulfillment::Pickup);
        o.accept(&f.cook, f.now).unwrap();
        o.mark_ready(f.now).unwrap();
        assert_eq!(o.status, OrderStatus::ReadyForPickup);
        o.complete(f.now).unwrap();
        assert_eq!(o.status, OrderStatus::Completed);
    }

    #[test]
    fn delivery_happy_path_goes_out_for_delivery() {
        let f = fx();
        let mut o = place(&f, 1, delivery());
        o.accept(&f.cook, f.now).unwrap();
        o.mark_ready(f.now).unwrap();
        assert_eq!(o.status, OrderStatus::OutForDelivery);
        o.complete(f.now).unwrap();
    }

    #[test]
    fn accept_requires_can_sell() {
        let f = fx();
        let mut o = place(&f, 1, Fulfillment::Pickup);
        let mut revoked = f.cook.clone();
        revoked.revoke_verification();
        assert_eq!(o.accept(&revoked, f.now), Err(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationNotVerified)));
        assert_eq!(o.status, OrderStatus::Placed);
        assert_eq!(
            o.accept(&f.cook, f.now + Duration::days(91)),
            Err(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationExpired))
        );
    }

    #[test]
    fn accept_rejects_other_cook() {
        let f = fx();
        let mut o = place(&f, 1, Fulfillment::Pickup);
        let mut other = f.cook.clone();
        other.id = Uuid::new_v4();
        assert!(matches!(o.accept(&other, f.now), Err(DomainError::Validation(_))));
    }

    #[test]
    fn invalid_transitions_rejected() {
        let f = fx();
        let mut o = place(&f, 1, Fulfillment::Pickup);
        assert!(o.mark_ready(f.now).is_err());
        assert!(o.complete(f.now).is_err());
        o.accept(&f.cook, f.now).unwrap();
        assert!(o.accept(&f.cook, f.now).is_err());
        assert!(o.complete(f.now).is_err());
    }

    #[test]
    fn cancel_only_before_fulfilment_starts() {
        let f = fx();
        let mut o = place(&f, 1, Fulfillment::Pickup);
        o.cancel(f.now).unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert!(o.cancel(f.now).is_err());

        let mut o = place(&f, 1, Fulfillment::Pickup);
        o.accept(&f.cook, f.now).unwrap();
        o.mark_ready(f.now).unwrap();
        assert!(o.cancel(f.now).is_err());
    }

    #[test]
    fn refund_is_full_amount_including_platform_fee() {
        let f = fx();
        let mut o = place(&f, 3, delivery());
        o.accept(&f.cook, f.now).unwrap();
        let r = o.refund(RefundReason::Unfulfilled, f.now).unwrap();
        assert_eq!(r.amount_cents, o.total_cents);
        assert!(r.amount_cents > o.subtotal_cents, "fee and delivery are refunded too");
        assert_eq!(o.status, OrderStatus::Refunded);
        assert_eq!(o.refund, Some(r));
    }

    #[test]
    fn refund_rules_by_state() {
        let f = fx();
        let mut placed = place(&f, 1, Fulfillment::Pickup);
        assert!(placed.refund(RefundReason::Other, f.now).is_err(), "unpaid-out orders are cancelled, not refunded");

        let mut o = place(&f, 1, Fulfillment::Pickup);
        o.accept(&f.cook, f.now).unwrap();
        o.mark_ready(f.now).unwrap();
        o.complete(f.now).unwrap();
        assert!(o.refund(RefundReason::Other, f.now).is_err());
        assert!(o.refund(RefundReason::IncorrectOrder, f.now).is_ok());
        assert!(o.refund(RefundReason::IncorrectOrder, f.now).is_err(), "no double refunds");
    }

    #[test]
    fn rejects_catering_and_retail_purpose() {
        let f = fx();
        for (purpose, v) in [
            (OrderPurpose::Catering, RegulatoryViolation::Catering),
            (OrderPurpose::ResaleToFoodRetailer, RegulatoryViolation::SaleToFoodRetailer),
        ] {
            let n = NewOrder { purpose, ..new_order(&f, 1, Fulfillment::Pickup) };
            assert_eq!(Order::place(&f.cook, &f.dishes, n, &PricingPolicy::default(), f.now), Err(DomainError::RegulatoryViolation(v)));
        }
    }

    #[test]
    fn bulk_orders_treated_as_catering() {
        let f = fx();
        let n = new_order(&f, 21, Fulfillment::Pickup);
        assert_eq!(
            Order::place(&f.cook, &f.dishes, n, &PricingPolicy::default(), f.now),
            Err(DomainError::RegulatoryViolation(RegulatoryViolation::Catering))
        );
        assert!(Order::place(&f.cook, &f.dishes, new_order(&f, 20, Fulfillment::Pickup), &PricingPolicy::default(), f.now).is_ok());
    }

    #[test]
    fn bulk_limit_counts_duplicate_lines_together() {
        let f = fx();
        let id = f.dishes[0].id;
        let n = NewOrder {
            items: vec![RequestedLine { dish_id: id, quantity: 12 }, RequestedLine { dish_id: id, quantity: 12 }],
            ..new_order(&f, 1, Fulfillment::Pickup)
        };
        assert!(Order::place(&f.cook, &f.dishes, n, &PricingPolicy::default(), f.now).is_err());
    }

    #[test]
    fn rejects_prohibited_or_inactive_or_foreign_dishes() {
        let mut f = fx();
        f.dishes[0].is_raw_seafood = true; // e.g. legacy/tampered record
        let n = new_order(&f, 1, Fulfillment::Pickup);
        assert_eq!(
            Order::place(&f.cook, &f.dishes, n, &PricingPolicy::default(), f.now),
            Err(DomainError::RegulatoryViolation(RegulatoryViolation::RawSeafood))
        );

        let mut f = fx();
        f.dishes[0].is_active = false;
        assert!(Order::place(&f.cook, &f.dishes, new_order(&f, 1, Fulfillment::Pickup), &PricingPolicy::default(), f.now).is_err());

        let mut f = fx();
        f.dishes[0].cook_id = Uuid::new_v4();
        assert!(Order::place(&f.cook, &f.dishes, new_order(&f, 1, Fulfillment::Pickup), &PricingPolicy::default(), f.now).is_err());
    }

    #[test]
    fn uncertified_cook_cannot_receive_orders() {
        let mut f = fx();
        f.cook.revoke_verification();
        assert!(Order::place(&f.cook, &f.dishes, new_order(&f, 1, Fulfillment::Pickup), &PricingPolicy::default(), f.now).is_err());
    }

    #[test]
    fn validates_delivery_details_and_items() {
        let f = fx();
        let bad_postal =
            Fulfillment::Delivery(DeliveryDetails { address: "x".into(), postal_code: "12ab56".into(), delivered_by: DeliveredBy::Cook });
        assert!(Order::place(&f.cook, &f.dishes, new_order(&f, 1, bad_postal), &PricingPolicy::default(), f.now).is_err());
        assert!(Order::place(&f.cook, &f.dishes, new_order(&f, 0, Fulfillment::Pickup), &PricingPolicy::default(), f.now).is_err());
        let empty = NewOrder { items: vec![], ..new_order(&f, 1, Fulfillment::Pickup) };
        assert!(Order::place(&f.cook, &f.dishes, empty, &PricingPolicy::default(), f.now).is_err());
    }

    #[test]
    fn delivery_is_cook_performed_by_default() {
        let d: Fulfillment = serde_json::from_str(r#"{"type":"delivery","address":"a","postal_code":"123456"}"#).unwrap();
        match d {
            Fulfillment::Delivery(d) => assert_eq!(d.delivered_by, DeliveredBy::Cook),
            _ => panic!("expected delivery"),
        }
        // There is no way to ask for a courier.
        assert!(
            serde_json::from_str::<Fulfillment>(r#"{"type":"delivery","address":"a","postal_code":"123456","delivered_by":"grab"}"#)
                .is_err()
        );
    }

    #[test]
    fn expiry_between_placement_and_acceptance_blocks_acceptance() {
        let f = fx();
        // Eligible at placement time...
        let mut o = place(&f, 1, Fulfillment::Pickup);
        assert!(f.cook.can_sell(o.placed_at));
        // ...certificate lapses before the cook gets to it.
        let expiry = f.cook.food_safety_cert_expires_at.unwrap();
        assert_eq!(
            o.accept(&f.cook, expiry),
            Err(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationExpired)),
            "exact expiry instant already blocks"
        );
        assert_eq!(
            o.accept(&f.cook, expiry + Duration::hours(1)),
            Err(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationExpired))
        );
        assert_eq!(o.status, OrderStatus::Placed, "a refused acceptance changes nothing");
        // One tick earlier it would still have worked.
        assert!(o.accept(&f.cook, expiry - Duration::nanoseconds(1)).is_ok());
    }

    #[test]
    fn fulfillment_serde_shape() {
        assert_eq!(serde_json::to_string(&Fulfillment::Pickup).unwrap(), r#"{"type":"pickup"}"#);
        let d: Fulfillment = serde_json::from_str(r#"{"type":"delivery","address":"a","postal_code":"123456"}"#).unwrap();
        assert!(matches!(d, Fulfillment::Delivery(_)));
    }
}
