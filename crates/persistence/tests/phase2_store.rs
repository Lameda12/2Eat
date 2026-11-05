//! Phase 2 behaviour: auth store, certificate reviews, the approved-cook gate,
//! audit log, refund governance. Shared scenarios run against `MemoryStore` and
//! (when `DATABASE_URL` is set) `PgStore`; a few checks are Postgres-only.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Duration as Dur, SubsecRound, Utc};
use twoeat_domain::*;
use twoeat_persistence::*;
use uuid::Uuid;

// ------------------------------------------------------------------ fixtures

#[derive(Clone)]
struct B {
    store: Arc<dyn Store>,
    auth: Arc<dyn AuthStore>,
}

fn memory() -> B {
    let m = Arc::new(MemoryStore::new());
    B { store: m.clone(), auth: m }
}

async fn pg_fresh() -> Option<(B, PgStore)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let schema = format!("t_{}", Uuid::new_v4().simple());
    let admin = sqlx::postgres::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |conn, _| {
            let s = schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {s}, public")).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let pg = PgStore::new(pool);
    pg.migrate().await.unwrap();
    let a = Arc::new(pg.clone());
    Some((B { store: a.clone(), auth: a }, pg))
}

fn now() -> DateTime<Utc> {
    // Postgres keeps microseconds.
    Utc::now().trunc_subsecs(6)
}

fn sg(lat: f64, lng: f64) -> LatLng {
    LatLng::new_in_singapore(lat, lng).unwrap()
}

async fn user(b: &B, role: Role) -> Uuid {
    let id = Uuid::new_v4();
    b.auth
        .create_user(NewUser {
            id,
            email: format!("{id}@example.com"),
            password_hash: "$argon2id$placeholder".into(),
            role,
            created_at: now(),
        })
        .await
        .unwrap();
    id
}

async fn register(b: &B, name: &str) -> (Cook, Uuid) {
    let t = now();
    let cook = Cook::register(Uuid::new_v4(), name, sg(1.30, 103.80), &HbfbAcknowledgements::all()).unwrap();
    let sub = CertificationSubmission::new(cook.id, "doc".into(), "application/pdf".into(), t).unwrap();
    let review_id = sub.id;
    b.store.register_cook(&cook, &sub, t).await.unwrap();
    (cook, review_id)
}

async fn approved_cook(b: &B, admin: Uuid) -> Cook {
    let (_cook, review) = register(b, "Cook").await;
    let (_, cook) =
        b.store.decide_review(review, Some(admin), CertDecision::Approve { expires_at: now() + Dur::days(90) }, None, now()).await.unwrap();
    assert!(cook.can_sell(now()));
    cook
}

async fn dish(b: &B, cook: &Cook) -> Dish {
    let d = Dish::create(
        cook,
        NewDish { name: "Laksa".into(), description: String::new(), price_cents: 800, is_catering: false, is_raw_seafood: false },
        now(),
    )
    .unwrap();
    b.store.insert_dish(&d).await.unwrap();
    d
}

async fn placed(b: &B, cook: &Cook, dish: &Dish, eater: Uuid, qty: u32, f: Fulfillment) -> Order {
    let mut o = Order::place(
        cook,
        std::slice::from_ref(dish),
        NewOrder {
            eater_id: eater,
            items: vec![RequestedLine { dish_id: dish.id, quantity: qty }],
            fulfillment: f,
            purpose: OrderPurpose::PersonalConsumption,
        },
        &PricingPolicy::default(),
        now(),
    )
    .unwrap();
    o.attach_payment("pay".into());
    b.store.insert_order(&o).await.unwrap();
    o
}

async fn accepted(b: &B, cook: &Cook, dish: &Dish, eater: Uuid) -> Order {
    let o = placed(b, cook, dish, eater, 1, Fulfillment::Pickup).await;
    b.store.transition_order(o.id, Box::new(|c, o| o.accept(c, Utc::now()))).await.unwrap()
}

fn delivery() -> Fulfillment {
    Fulfillment::Delivery(DeliveryDetails { address: "1 Test Rd".into(), postal_code: "123456".into(), delivered_by: DeliveredBy::Cook })
}

fn refund_req(order: &Order, by: Uuid, who: RefundInitiator, at: DateTime<Utc>) -> NewRefund {
    NewRefund { order_id: order.id, initiated_by: by, initiator: who, reason: RefundReason::Unfulfilled, now: at }
}

async fn audit_actions(b: &B, target_type: &str, target: Uuid) -> Vec<String> {
    let f = AuditFilter { target_type: Some(target_type.into()), target_id: Some(target), action: None };
    b.store.audit_entries(f, 100).await.unwrap().into_iter().map(|a| a.entry.action).collect()
}

// -------------------------------------------------------------- auth store

async fn auth_contract(b: B) {
    let id = Uuid::new_v4();
    b.auth
        .create_user(NewUser {
            id,
            email: "Eater@Example.com".to_lowercase(),
            password_hash: "hash".into(),
            role: Role::Eater,
            created_at: now(),
        })
        .await
        .unwrap();
    // Email uniqueness is case-insensitive.
    let dup = b
        .auth
        .create_user(NewUser {
            id: Uuid::new_v4(),
            email: "EATER@example.COM".into(),
            password_hash: "h".into(),
            role: Role::Cook,
            created_at: now(),
        })
        .await;
    assert!(matches!(dup, Err(StoreError::Conflict(_))), "{dup:?}");
    let found = b.auth.find_user_by_email("EATER@EXAMPLE.COM").await.unwrap().expect("case-insensitive lookup");
    assert_eq!((found.id, found.role), (id, Role::Eater));
    assert!(b.auth.find_user_by_email("nobody@example.com").await.unwrap().is_none());

    // Session lifecycle.
    let t0 = now();
    let hash = [7u8; 32];
    let sid = Uuid::new_v4();
    b.auth
        .create_session(NewSession { id: sid, user_id: id, token_hash: hash, expires_at: t0 + Dur::days(30), created_at: t0 })
        .await
        .unwrap();
    let s = b.auth.find_session(&hash).await.unwrap().unwrap();
    assert!(s.is_active(t0) && s.role == Role::Eater && s.session_id == sid);
    assert!(!s.is_active(t0 + Dur::days(31)), "expired");
    assert!(b.auth.find_session(&[8u8; 32]).await.unwrap().is_none());

    // Rotation: old revoked, new active, and a stale rotate fails.
    let new = NewSession { id: Uuid::new_v4(), user_id: id, token_hash: [9u8; 32], expires_at: t0 + Dur::days(30), created_at: t0 };
    let new_id = new.id;
    b.auth.rotate_session(sid, new, t0).await.unwrap();
    assert!(!b.auth.find_session(&hash).await.unwrap().unwrap().is_active(t0), "old token revoked");
    assert!(b.auth.find_session(&[9u8; 32]).await.unwrap().unwrap().is_active(t0));
    let again = NewSession { id: Uuid::new_v4(), user_id: id, token_hash: [10u8; 32], expires_at: t0 + Dur::days(30), created_at: t0 };
    assert!(matches!(b.auth.rotate_session(sid, again, t0).await, Err(StoreError::NotFound(_))));
    assert!(b.auth.find_session(&[10u8; 32]).await.unwrap().is_none(), "failed rotation creates nothing");

    // Revocation is explicit and idempotent.
    assert!(b.auth.revoke_session(new_id, t0).await.unwrap());
    assert!(!b.auth.revoke_session(new_id, t0).await.unwrap());
    assert!(!b.auth.find_session(&[9u8; 32]).await.unwrap().unwrap().is_active(t0));
}

#[tokio::test]
async fn auth_store_memory() {
    auth_contract(memory()).await;
}
#[tokio::test]
async fn auth_store_pg() {
    if let Some((b, _)) = pg_fresh().await {
        auth_contract(b).await;
    }
}

// ------------------------------------------------- reviews + approval gate

async fn review_contract(b: B) {
    let admin = user(&b, Role::Admin).await;
    let (cook, review_id) = register(&b, "Reviewed").await;

    let pending = b.store.list_reviews(Some(CertificationStatus::Pending), 10, 0).await.unwrap();
    assert!(pending.items.iter().any(|r| r.id == review_id) && pending.total >= 1);
    assert_eq!(b.store.list_reviews(Some(CertificationStatus::Pending), 1, 0).await.unwrap().items.len(), 1, "pagination limit");

    // A cook with no decided review cannot have an order accepted, even if the Cook struct says verified.
    let t = now();
    let mut sneaky = Cook::register(Uuid::new_v4(), "Sneaky", sg(1.31, 103.81), &HbfbAcknowledgements::all()).unwrap();
    sneaky.is_verified = true;
    sneaky.food_safety_cert_expires_at = Some(t + Dur::days(30));
    assert!(sneaky.can_sell(t), "the domain object alone would allow it");
    let sub = CertificationSubmission::new(sneaky.id, "doc".into(), "application/pdf".into(), t).unwrap();
    b.store.register_cook(&sneaky, &sub, t).await.unwrap();
    let d = dish(&b, &sneaky).await;
    let o = placed(&b, &sneaky, &d, Uuid::new_v4(), 1, Fulfillment::Pickup).await;
    let res = b.store.transition_order(o.id, Box::new(|c, o| o.accept(c, Utc::now()))).await;
    assert!(
        matches!(res, Err(StoreError::Domain(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationNotVerified)))),
        "{res:?}"
    );
    assert_eq!(b.store.get_order(o.id).await.unwrap().status, OrderStatus::Placed, "nothing persisted");

    // Rejection stores a structured reason and the cook cannot sell.
    let (rev, cook_after) = b
        .store
        .decide_review(
            review_id,
            Some(admin),
            CertDecision::Reject { reason: RejectionReason::Other("stamp looks altered".into()) },
            Some("second look needed".into()),
            now(),
        )
        .await
        .unwrap();
    assert_eq!(rev.status, CertificationStatus::Rejected);
    assert_eq!(rev.reviewer_id, Some(admin));
    assert_eq!(rev.rejection_reason, Some(RejectionReason::Other("stamp looks altered".into())));
    assert!(!cook_after.can_sell(now()));
    assert_eq!(b.store.get_review(review_id).await.unwrap().notes.as_deref(), Some("second look needed"));
    assert!(!b.store.get_cook(cook.id).await.unwrap().is_verified);
    assert_eq!(audit_actions(&b, "certificate_review", review_id).await, ["certificate.rejected"]);

    // Decisions are final.
    let again =
        b.store.decide_review(review_id, Some(admin), CertDecision::Approve { expires_at: now() + Dur::days(9) }, None, now()).await;
    assert!(matches!(again, Err(StoreError::Conflict(_))), "{again:?}");
    // Rejection reason must be meaningful.
    let (_, r2) = register(&b, "Other").await;
    let bad =
        b.store.decide_review(r2, Some(admin), CertDecision::Reject { reason: RejectionReason::Other(" ".into()) }, None, now()).await;
    assert!(matches!(bad, Err(StoreError::Domain(DomainError::Validation(_)))));
    assert_eq!(b.store.get_review(r2).await.unwrap().status, CertificationStatus::Pending, "failed decision changes nothing");

    // Approve path + audit; a cook whose latest decided review is Approved can be accepted.
    let ok = approved_cook(&b, admin).await;
    let sub = b.store.latest_submission(ok.id).await.unwrap().unwrap();
    assert_eq!(sub.status, CertificationStatus::Approved);
    assert_eq!(audit_actions(&b, "certificate_review", sub.id).await, ["certificate.approved"]);
    let d = dish(&b, &ok).await;
    let o = accepted(&b, &ok, &d, Uuid::new_v4()).await;
    assert_eq!(o.status, OrderStatus::AcceptedByCook);

    // Revocation is audited.
    b.store.revoke_cook(Some(admin), ok.id, now()).await.unwrap();
    assert_eq!(audit_actions(&b, "cook", ok.id).await, ["cook.revoked"]);
}

#[tokio::test]
async fn reviews_memory() {
    review_contract(memory()).await;
}
#[tokio::test]
async fn reviews_pg() {
    if let Some((b, _)) = pg_fresh().await {
        review_contract(b).await;
    }
}

// Expiry between placement and acceptance (distinct from the revocation lock test):
// the cook is eligible when the order is placed, the certificate lapses, and acceptance is refused.
async fn expiry_mid_flight(b: B) {
    let admin = user(&b, Role::Admin).await;
    let (_cook, review) = register(&b, "ShortCert").await;
    let (_, cook) = b
        .store
        .decide_review(review, Some(admin), CertDecision::Approve { expires_at: Utc::now() + Dur::milliseconds(1500) }, None, now())
        .await
        .unwrap();
    let d = dish(&b, &cook).await;
    let o = placed(&b, &cook, &d, Uuid::new_v4(), 1, Fulfillment::Pickup).await; // eligible at placement
    assert!(cook.can_sell(Utc::now()));

    tokio::time::sleep(Duration::from_millis(1700)).await; // the certificate lapses

    let res = b.store.transition_order(o.id, Box::new(|c, o| o.accept(c, Utc::now()))).await;
    assert!(matches!(res, Err(StoreError::Domain(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationExpired)))), "{res:?}");
    assert_eq!(b.store.get_order(o.id).await.unwrap().status, OrderStatus::Placed);
    assert!(
        b.store
            .nearby_cooks(&twoeat_geo::NearbyQuery::new(cook.location, Some(1000.0), None).unwrap(), Utc::now())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expiry_mid_flight_memory() {
    expiry_mid_flight(memory()).await;
}
#[tokio::test]
async fn expiry_mid_flight_pg() {
    if let Some((b, _)) = pg_fresh().await {
        expiry_mid_flight(b).await;
    }
}

// ---------------------------------------------------------------- dishes

async fn dish_review_contract(b: B) {
    let admin = user(&b, Role::Admin).await;
    let cook = approved_cook(&b, admin).await;
    let flagged = Dish::create(
        &cook,
        NewDish { name: "Tuna Poke Bowl".into(), description: String::new(), price_cents: 1200, is_catering: false, is_raw_seafood: false },
        now(),
    )
    .unwrap();
    assert!(!flagged.is_active);
    b.store.insert_dish(&flagged).await.unwrap();
    assert_eq!(b.store.get_dish(flagged.id).await.unwrap(), flagged);
    let page = b.store.list_flagged_dishes(10, 0).await.unwrap();
    assert!(page.items.iter().any(|d| d.id == flagged.id));

    // A flagged dish cannot be ordered until approved.
    assert!(
        Order::place(
            &cook,
            std::slice::from_ref(&flagged),
            NewOrder {
                eater_id: Uuid::new_v4(),
                items: vec![RequestedLine { dish_id: flagged.id, quantity: 1 }],
                fulfillment: Fulfillment::Pickup,
                purpose: OrderPurpose::PersonalConsumption,
            },
            &PricingPolicy::default(),
            now()
        )
        .is_err()
    );

    let d = b.store.decide_dish_review(flagged.id, admin, true, now()).await.unwrap();
    assert!(d.is_active && d.review == DishReview::Approved);
    assert!(b.store.decide_dish_review(flagged.id, admin, true, now()).await.is_err(), "already decided");
    assert_eq!(audit_actions(&b, "dish", flagged.id).await, ["dish.review_approved"]);
    assert!(!b.store.list_flagged_dishes(10, 0).await.unwrap().items.iter().any(|d| d.id == flagged.id));
}

#[tokio::test]
async fn dish_review_memory() {
    dish_review_contract(memory()).await;
}
#[tokio::test]
async fn dish_review_pg() {
    if let Some((b, _)) = pg_fresh().await {
        dish_review_contract(b).await;
    }
}

// ---------------------------------------------------------------- refunds

async fn refund_contract(b: B) {
    let policy = WindowedRefundPolicy::default();
    let admin = user(&b, Role::Admin).await;
    let cook = approved_cook(&b, admin).await;
    let d = dish(&b, &cook).await;
    let eater = user(&b, Role::Eater).await;
    let t = now();

    // Refunds 1..=3 (0, 1, 2 prior in the window): automatic.
    let mut first_order = None;
    for prior in 0..3 {
        let o = accepted(&b, &cook, &d, eater).await;
        let out = b.store.request_refund(refund_req(&o, eater, RefundInitiator::Eater, t), &policy).await.unwrap();
        assert!(out.created && !out.order_cancelled);
        assert_eq!(out.record.status(), RefundStatus::Approved, "refund #{} (prior={prior}) is automatic", prior + 1);
        assert_eq!(out.record.decision, Some(RefundDecision::Auto));
        assert_eq!(out.record.amount_cents, o.total_cents);
        let (_, settled) = b.store.settle_refund(out.record.id, t).await.unwrap();
        assert_eq!(settled.status, OrderStatus::Refunded);
        first_order.get_or_insert(o);
    }
    let first_order = first_order.unwrap();

    // Re-requesting a refund on the same order is idempotent and never double-counts.
    let dup = b.store.request_refund(refund_req(&first_order, eater, RefundInitiator::Eater, t), &policy).await.unwrap();
    assert!(!dup.created);
    assert_eq!(
        b.store.list_refunds(Some(RefundStatus::Approved), 50, 0).await.unwrap().items.iter().filter(|r| r.initiated_by == eater).count(),
        3
    );

    // The 4th (3 prior in the trailing 30 days) goes to review: no payout yet.
    let o4 = accepted(&b, &cook, &d, eater).await;
    let out4 = b.store.request_refund(refund_req(&o4, eater, RefundInitiator::Eater, t), &policy).await.unwrap();
    assert_eq!(out4.record.status(), RefundStatus::PendingReview);
    assert_eq!(out4.record.decision, None);
    assert!(matches!(b.store.settle_refund(out4.record.id, t).await, Err(StoreError::Conflict(_))), "cannot pay out a pending refund");
    assert_eq!(b.store.get_order(o4.id).await.unwrap().status, OrderStatus::AcceptedByCook, "order untouched while pending");
    assert!(b.store.list_refunds(Some(RefundStatus::PendingReview), 50, 0).await.unwrap().items.iter().any(|r| r.id == out4.record.id));

    // Admin approves: pays out the FULL total.
    let dec = b.store.decide_refund(out4.record.id, admin, true, Some("looks legit".into()), t, &policy).await.unwrap();
    assert_eq!((dec.status(), dec.decision, dec.decided_by), (RefundStatus::Approved, Some(RefundDecision::AdminApproved), Some(admin)));
    assert!(
        matches!(b.store.decide_refund(out4.record.id, admin, false, None, t, &policy).await, Err(StoreError::Conflict(_))),
        "decisions are final"
    );
    let (_, settled) = b.store.settle_refund(out4.record.id, t).await.unwrap();
    assert_eq!(settled.refund.unwrap().amount_cents, settled.total_cents);

    // Admin denies the next one: nothing pays out and it cannot be re-requested.
    let o5 = accepted(&b, &cook, &d, eater).await;
    let out5 = b.store.request_refund(refund_req(&o5, eater, RefundInitiator::Eater, t), &policy).await.unwrap();
    assert_eq!(out5.record.status(), RefundStatus::PendingReview);
    let denied = b.store.decide_refund(out5.record.id, admin, false, None, t, &policy).await.unwrap();
    assert_eq!((denied.status(), denied.decision), (RefundStatus::Denied, Some(RefundDecision::AdminDenied)));
    assert!(matches!(b.store.settle_refund(out5.record.id, t).await, Err(StoreError::Conflict(_))));
    let retry = b.store.request_refund(refund_req(&o5, eater, RefundInitiator::Eater, t), &policy).await.unwrap();
    assert!(!retry.created && retry.record.status() == RefundStatus::Denied);

    // The window rolls: 31 days later the old refunds no longer count.
    let o6 = accepted(&b, &cook, &d, eater).await;
    let later = b.store.request_refund(refund_req(&o6, eater, RefundInitiator::Eater, t + Dur::days(31)), &policy).await.unwrap();
    assert_eq!(later.record.status(), RefundStatus::Approved);

    // Admin- and cook-initiated refunds bypass the eater counter.
    let o7 = accepted(&b, &cook, &d, eater).await;
    let by_admin = b.store.request_refund(refund_req(&o7, admin, RefundInitiator::Admin, t), &policy).await.unwrap();
    assert_eq!((by_admin.record.decision, by_admin.record.decided_by), (Some(RefundDecision::AdminApproved), Some(admin)));

    // Unaccepted order: cancelled in the same transaction, never counted.
    let eater2 = user(&b, Role::Eater).await;
    for _ in 0..5 {
        let p = placed(&b, &cook, &d, eater2, 1, Fulfillment::Pickup).await;
        let out = b.store.request_refund(refund_req(&p, eater2, RefundInitiator::Eater, t), &policy).await.unwrap();
        assert!(out.order_cancelled);
        assert_eq!((out.record.settlement, out.record.status()), (RefundSettlement::Void, RefundStatus::Approved));
        assert_eq!(b.store.get_order(p.id).await.unwrap().status, OrderStatus::Cancelled);
    }
    let o = accepted(&b, &cook, &d, eater2).await;
    let real = b.store.request_refund(refund_req(&o, eater2, RefundInitiator::Eater, t), &policy).await.unwrap();
    assert_eq!(real.record.status(), RefundStatus::Approved, "voided cancellations are not refunds");

    // Audit trail: request, decision and settlement are all recorded.
    assert_eq!(audit_actions(&b, "refund", out4.record.id).await, ["refund.requested", "refund.admin_approved", "refund.settled"]);
    assert_eq!(audit_actions(&b, "refund", out5.record.id).await, ["refund.requested", "refund.admin_denied"]);
}

#[tokio::test]
async fn refunds_memory() {
    refund_contract(memory()).await;
}
#[tokio::test]
async fn refunds_pg() {
    if let Some((b, _)) = pg_fresh().await {
        refund_contract(b).await;
    }
}

async fn full_total_on_review_path(b: B) {
    // Delivery order (items + delivery fee + platform fee), forced through admin review.
    let policy = WindowedRefundPolicy { eater_auto_approve_below: 0, ..Default::default() };
    let admin = user(&b, Role::Admin).await;
    let cook = approved_cook(&b, admin).await;
    let d = dish(&b, &cook).await;
    let eater = user(&b, Role::Eater).await;
    let o = placed(&b, &cook, &d, eater, 3, delivery()).await;
    let o = b.store.transition_order(o.id, Box::new(|c, o| o.accept(c, Utc::now()))).await.unwrap();
    assert!(o.platform_fee_cents > 0 && o.delivery_fee_cents > 0);
    assert_eq!(o.total_cents, o.subtotal_cents + o.platform_fee_cents + o.delivery_fee_cents);

    let out = b.store.request_refund(refund_req(&o, eater, RefundInitiator::Eater, now()), &policy).await.unwrap();
    assert_eq!(out.record.status(), RefundStatus::PendingReview);
    b.store.decide_refund(out.record.id, admin, true, None, now(), &policy).await.unwrap();
    let (rec, order) = b.store.settle_refund(out.record.id, now()).await.unwrap();
    assert_eq!(rec.amount_cents, o.total_cents);
    assert_eq!(order.refund.unwrap().amount_cents, o.total_cents, "pending_review -> AdminApproved refunds the full total, fees included");
    assert_eq!(order.status, OrderStatus::Refunded);
}

#[tokio::test]
async fn full_total_review_memory() {
    full_total_on_review_path(memory()).await;
}
#[tokio::test]
async fn full_total_review_pg() {
    if let Some((b, _)) = pg_fresh().await {
        full_total_on_review_path(b).await;
    }
}

async fn cook_flag(b: B) {
    let policy = WindowedRefundPolicy::default();
    let admin = user(&b, Role::Admin).await;
    let cook = approved_cook(&b, admin).await;
    let d = dish(&b, &cook).await;
    let flags = |b: &B| {
        let b = b.clone();
        let id = cook.id;
        async move {
            b.store
                .audit_entries(
                    AuditFilter { target_type: Some("cook".into()), target_id: Some(id), action: Some("cook.flagged_for_review".into()) },
                    50,
                )
                .await
                .unwrap()
                .len()
        }
    };
    // Six different eaters, one refund each, all against the same cook.
    for n in 1..=6 {
        let eater = user(&b, Role::Eater).await;
        let o = accepted(&b, &cook, &d, eater).await;
        let out = b.store.request_refund(refund_req(&o, eater, RefundInitiator::Eater, now()), &policy).await.unwrap();
        assert_eq!(out.record.status(), RefundStatus::Approved);
        assert_eq!(flags(&b).await, usize::from(n > 5), "flag appears only once MORE than 5 refunds exist (n = {n})");
    }
    // Flag only: the cook is not suspended.
    let c = b.store.get_cook(cook.id).await.unwrap();
    assert!(c.is_verified && c.can_sell(now()));
}

#[tokio::test]
async fn cook_flag_memory() {
    cook_flag(memory()).await;
}
#[tokio::test]
async fn cook_flag_pg() {
    if let Some((b, _)) = pg_fresh().await {
        cook_flag(b).await;
    }
}

// ----------------------------------------------------- Postgres-only checks

#[tokio::test]
async fn pg_trigger_blocks_accepting_orders_for_unapproved_cooks() {
    let Some((b, pg)) = pg_fresh().await else { return };
    let admin = user(&b, Role::Admin).await;
    let cook = approved_cook(&b, admin).await;
    let d = dish(&b, &cook).await;
    let o = placed(&b, &cook, &d, Uuid::new_v4(), 1, Fulfillment::Pickup).await;

    // Reject-after-approve scenario: latest decided review is 'rejected', yet the cook row still says verified
    // (e.g. a bad manual edit). The DB itself must refuse, bypassing all application code.
    sqlx::query("INSERT INTO certificate_reviews (id, cook_id, document_ref, content_type, status, submitted_at, reviewed_at, reviewer_id, rejection_reason) \
                 VALUES ($1, $2, 'd', 'application/pdf', 'rejected', now() + interval '1 minute', now() + interval '1 minute', $3, 'suspected_fraud')")
        .bind(Uuid::new_v4()).bind(cook.id).bind(admin).execute(pg.pool()).await.unwrap();
    let err = sqlx::query("UPDATE orders SET status = 'accepted_by_cook' WHERE id = $1").bind(o.id).execute(pg.pool()).await.unwrap_err();
    assert!(err.to_string().contains("no approved certificate review"), "{err}");

    // The application-level check agrees (same outcome through the Store).
    let res = b.store.transition_order(o.id, Box::new(|c, o| o.accept(c, Utc::now()))).await;
    assert!(
        matches!(res, Err(StoreError::Domain(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationNotVerified)))),
        "{res:?}"
    );
    assert_eq!(b.store.get_order(o.id).await.unwrap().status, OrderStatus::Placed);

    // Orders that were already accepted can still be refunded/completed later (the trigger only guards entering the state).
}

#[tokio::test]
async fn pg_audit_log_is_append_only() {
    let Some((b, pg)) = pg_fresh().await else { return };
    let admin = user(&b, Role::Admin).await;
    let _ = approved_cook(&b, admin).await;
    let any: Uuid = sqlx::query_scalar("SELECT id FROM audit_log LIMIT 1").fetch_one(pg.pool()).await.unwrap();
    let upd = sqlx::query("UPDATE audit_log SET action = 'tampered' WHERE id = $1").bind(any).execute(pg.pool()).await;
    assert!(upd.unwrap_err().to_string().contains("append-only"));
    let del = sqlx::query("DELETE FROM audit_log WHERE id = $1").bind(any).execute(pg.pool()).await;
    assert!(del.unwrap_err().to_string().contains("append-only"));
    let trunc = sqlx::query("TRUNCATE audit_log").execute(pg.pool()).await;
    assert!(trunc.unwrap_err().to_string().contains("append-only"));
}

/// The audit entry shares the mutation's transaction: when the mutation fails
/// AFTER the audit row was written, neither survives.
#[tokio::test]
async fn pg_audit_entry_rolls_back_with_its_mutation() {
    let Some((b, pg)) = pg_fresh().await else { return };
    let admin = user(&b, Role::Admin).await;
    let (cook, review) = register(&b, "Rollback").await;

    // Make the review UPDATE (the last write of decide_review) fail, like a lost connection or constraint would.
    sqlx::query("CREATE FUNCTION boom() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$")
        .execute(pg.pool())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER boom BEFORE UPDATE ON certificate_reviews FOR EACH ROW EXECUTE FUNCTION boom()")
        .execute(pg.pool())
        .await
        .unwrap();

    let res = b.store.decide_review(review, Some(admin), CertDecision::Approve { expires_at: now() + Dur::days(30) }, None, now()).await;
    assert!(res.is_err(), "the injected failure surfaces");

    let orphan_audit = audit_actions(&b, "certificate_review", review).await;
    assert!(orphan_audit.is_empty(), "no orphan audit entry: {orphan_audit:?}");
    let c = b.store.get_cook(cook.id).await.unwrap();
    assert!(!c.is_verified && c.food_safety_cert_expires_at.is_none(), "cook update rolled back too");
    assert_eq!(b.store.get_review(review).await.unwrap().status, CertificationStatus::Pending);

    // Control: once the fault is removed the same call commits BOTH the mutation and the audit entry.
    sqlx::query("DROP TRIGGER boom ON certificate_reviews").execute(pg.pool()).await.unwrap();
    b.store.decide_review(review, Some(admin), CertDecision::Approve { expires_at: now() + Dur::days(30) }, None, now()).await.unwrap();
    assert_eq!(audit_actions(&b, "certificate_review", review).await, ["certificate.approved"]);
    assert!(b.store.get_cook(cook.id).await.unwrap().is_verified);
}

#[tokio::test]
async fn pg_refund_audit_rolls_back_with_refund_row() {
    let Some((b, pg)) = pg_fresh().await else { return };
    let admin = user(&b, Role::Admin).await;
    let cook = approved_cook(&b, admin).await;
    let d = dish(&b, &cook).await;
    let eater = user(&b, Role::Eater).await;
    let o = accepted(&b, &cook, &d, eater).await;

    // Fail the INSERT into refunds (after the audit row was written in the same transaction).
    sqlx::query("CREATE FUNCTION boom() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected failure'; END $$")
        .execute(pg.pool())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER boom BEFORE INSERT ON refunds FOR EACH ROW EXECUTE FUNCTION boom()").execute(pg.pool()).await.unwrap();
    let res = b.store.request_refund(refund_req(&o, eater, RefundInitiator::Eater, now()), &WindowedRefundPolicy::default()).await;
    assert!(res.is_err());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'refund.requested'").fetch_one(pg.pool()).await.unwrap();
    assert_eq!(n, 0, "no orphan refund.requested entry");
    assert!(b.store.get_refund_for_order(o.id).await.unwrap().is_none());
}
