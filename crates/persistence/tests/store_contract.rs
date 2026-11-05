//! The same behavioural contract is run against `MemoryStore` and, when
//! `DATABASE_URL` is set, against `PgStore` (in a throwaway schema).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SubsecRound, Utc};
use twoeat_domain::*;
use twoeat_geo::NearbyQuery;
use twoeat_persistence::{MemoryStore, PgStore, Store, StoreError};
use uuid::Uuid;

fn sg(lat: f64, lng: f64) -> LatLng {
    LatLng::new_in_singapore(lat, lng).unwrap()
}

async fn register(store: &dyn Store, name: &str, at: LatLng) -> Cook {
    let now = Utc::now();
    let cook = Cook::register(Uuid::new_v4(), name, at, &HbfbAcknowledgements::all()).unwrap();
    let sub = CertificationSubmission::new(cook.id, "doc-1".into(), "application/pdf".into(), now).unwrap();
    store.register_cook(&cook, &sub, now).await.unwrap();
    cook
}

async fn approve(store: &dyn Store, cook: &Cook, days: i64) -> Cook {
    let now = Utc::now();
    store.review_certification(cook.id, CertDecision::Approve { expires_at: now + chrono::Duration::days(days) }, now).await.unwrap()
}

async fn dish_for(store: &dyn Store, cook: &Cook) -> Dish {
    let d = Dish::create(
        cook,
        NewDish { name: "Nasi lemak".into(), description: "".into(), price_cents: 700, is_catering: false, is_raw_seafood: false },
        Utc::now(),
    )
    .unwrap();
    store.insert_dish(&d).await.unwrap();
    d
}

async fn place(store: &dyn Store, cook: &Cook, dish: &Dish, qty: u32, f: Fulfillment) -> Order {
    let mut o = Order::place(
        cook,
        std::slice::from_ref(dish),
        NewOrder {
            eater_id: Uuid::new_v4(),
            items: vec![RequestedLine { dish_id: dish.id, quantity: qty }],
            fulfillment: f,
            purpose: OrderPurpose::PersonalConsumption,
        },
        &PricingPolicy::default(),
        // Postgres stores microseconds; keep round-trip equality exact.
        Utc::now().trunc_subsecs(6),
    )
    .unwrap();
    o.attach_payment("pay_1".into());
    store.insert_order(&o).await.unwrap();
    o
}

async fn contract(store: Arc<dyn Store>) {
    let s = store.as_ref();

    // --- discovery only returns verified, unexpired cooks, nearest first ---
    let centre = sg(1.3000, 103.8000);
    let near = register(s, "Near", sg(1.3010, 103.8000)).await;
    let far = register(s, "Far", sg(1.3100, 103.8000)).await;
    let unverified = register(s, "Unverified", sg(1.3005, 103.8000)).await;
    let outside = register(s, "Outside", sg(1.4000, 103.8000)).await;
    for c in [&near, &far, &outside] {
        approve(s, c, 30).await;
    }
    let q = NearbyQuery::new(centre, Some(2_000.0), Some(10)).unwrap();
    let hits = s.nearby_cooks(&q, Utc::now()).await.unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id).collect();
    assert_eq!(ids, vec![near.id, far.id], "nearest first, unverified and out-of-radius excluded");
    assert!(!ids.contains(&unverified.id));
    assert!((hits[0].distance_m - 111.2).abs() < 2.0, "{}", hits[0].distance_m);
    // Expired certificate => not discoverable.
    let later: DateTime<Utc> = Utc::now() + chrono::Duration::days(31);
    assert!(s.nearby_cooks(&q, later).await.unwrap().is_empty());
    // Revoked => not discoverable.
    let revoked = s.revoke_verification(far.id).await.unwrap();
    assert!(!revoked.is_verified);
    let ids: Vec<_> = s.nearby_cooks(&q, Utc::now()).await.unwrap().iter().map(|h| h.id).collect();
    assert_eq!(ids, vec![near.id]);

    // --- cook / submission round trip ---
    let got = s.get_cook(near.id).await.unwrap();
    assert!(got.is_verified && got.food_safety_cert_expires_at.is_some());
    let sub = s.latest_submission(near.id).await.unwrap().unwrap();
    assert_eq!(sub.status, CertificationStatus::Approved);
    assert!(matches!(s.get_cook(Uuid::new_v4()).await, Err(StoreError::NotFound(_))));

    // --- dishes ---
    let cook = s.get_cook(near.id).await.unwrap();
    let dish = dish_for(s, &cook).await;
    assert_eq!(s.get_dish(dish.id).await.unwrap(), dish);
    assert_eq!(s.list_dishes(cook.id).await.unwrap(), vec![dish.clone()]);

    // --- order round trip, pickup and delivery ---
    let pickup = place(s, &cook, &dish, 2, Fulfillment::Pickup).await;
    assert_eq!(s.get_order(pickup.id).await.unwrap(), pickup);
    let delivery = place(
        s,
        &cook,
        &dish,
        1,
        Fulfillment::Delivery(DeliveryDetails {
            address: "1 Test Rd".into(),
            postal_code: "123456".into(),
            delivered_by: DeliveredBy::Cook,
        }),
    )
    .await;
    assert_eq!(s.get_order(delivery.id).await.unwrap(), delivery);

    // --- transitions persist; domain errors roll back ---
    let accepted = s.transition_order(pickup.id, Box::new(|c, o| o.accept(c, Utc::now()))).await.unwrap();
    assert_eq!(accepted.status, OrderStatus::AcceptedByCook);
    let bad = s.transition_order(pickup.id, Box::new(|c, o| o.accept(c, Utc::now()))).await;
    assert!(matches!(bad, Err(StoreError::Domain(DomainError::InvalidTransition { .. }))));
    assert_eq!(s.get_order(pickup.id).await.unwrap().status, OrderStatus::AcceptedByCook);

    let refunded =
        s.transition_order(pickup.id, Box::new(|_, o| o.refund(RefundReason::Unfulfilled, Utc::now()).map(|_| ()))).await.unwrap();
    assert_eq!(refunded.refund.unwrap().amount_cents, refunded.total_cents);
    let reloaded = s.get_order(pickup.id).await.unwrap();
    assert_eq!(reloaded.status, OrderStatus::Refunded);
    assert_eq!(reloaded.refund.unwrap().amount_cents, pickup.total_cents);

    // --- acceptance is refused once the cook has been revoked ---
    s.revoke_verification(cook.id).await.unwrap();
    let blocked = s.transition_order(delivery.id, Box::new(|c, o| o.accept(c, Utc::now()))).await;
    assert!(
        matches!(blocked, Err(StoreError::Domain(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationNotVerified)))),
        "{blocked:?}"
    );
    assert_eq!(s.get_order(delivery.id).await.unwrap().status, OrderStatus::Placed);
}

#[tokio::test]
async fn memory_store_contract() {
    contract(Arc::new(MemoryStore::new())).await;
}

async fn pg_in_fresh_schema() -> Option<PgStore> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let schema = format!("t_{}", Uuid::new_v4().simple());
    let admin = sqlx::postgres::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
    let s2 = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |conn, _| {
            let s = s2.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {s}, public")).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let store = PgStore::new(pool);
    store.migrate().await.unwrap();
    Some(store)
}

#[tokio::test]
async fn pg_store_contract() {
    let Some(store) = pg_in_fresh_schema().await else {
        eprintln!("DATABASE_URL not set; skipping Postgres contract test");
        return;
    };
    contract(Arc::new(store)).await;
}

/// A revocation in flight must block acceptance, and acceptance must then see
/// the revoked status (no accept-after-revoke race).
#[tokio::test]
async fn pg_accept_waits_for_in_flight_revocation_then_fails() {
    let Some(store) = pg_in_fresh_schema().await else {
        eprintln!("DATABASE_URL not set; skipping Postgres lock test");
        return;
    };
    let store = Arc::new(store);
    let cook = register(store.as_ref(), "Locky", sg(1.30, 103.80)).await;
    let cook = approve(store.as_ref(), &cook, 30).await;
    let dish = dish_for(store.as_ref(), &cook).await;
    let order = place(store.as_ref(), &cook, &dish, 1, Fulfillment::Pickup).await;

    // Simulate an admin revocation that has updated the row but not yet committed.
    let mut tx = store.pool().begin().await.unwrap();
    sqlx::query("UPDATE cooks SET is_verified = FALSE WHERE id = $1").bind(cook.id).execute(&mut *tx).await.unwrap();

    let s2 = store.clone();
    let accept = tokio::spawn(async move { s2.transition_order(order.id, Box::new(|c, o| o.accept(c, Utc::now()))).await });

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!accept.is_finished(), "accept must block on the cook row lock held by the revocation");

    tx.commit().await.unwrap();
    let res = accept.await.unwrap();
    assert!(
        matches!(res, Err(StoreError::Domain(DomainError::RegulatoryViolation(RegulatoryViolation::CertificationNotVerified)))),
        "{res:?}"
    );
    assert_eq!(store.get_order(order.id).await.unwrap().status, OrderStatus::Placed);
}

/// Concurrent acceptances of one order: exactly one wins.
#[tokio::test]
async fn pg_concurrent_accepts_single_winner() {
    let Some(store) = pg_in_fresh_schema().await else {
        return;
    };
    let store = Arc::new(store);
    let cook = register(store.as_ref(), "Racer", sg(1.31, 103.81)).await;
    let cook = approve(store.as_ref(), &cook, 30).await;
    let dish = dish_for(store.as_ref(), &cook).await;
    let order = place(store.as_ref(), &cook, &dish, 1, Fulfillment::Pickup).await;

    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let s = store.clone();
            let id = order.id;
            tokio::spawn(async move { s.transition_order(id, Box::new(|c, o| o.accept(c, Utc::now()))).await })
        })
        .collect();
    let mut wins = 0;
    for t in tasks {
        if t.await.unwrap().is_ok() {
            wins += 1;
        }
    }
    assert_eq!(wins, 1);
}
