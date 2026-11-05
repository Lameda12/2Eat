#![cfg(feature = "dev-auth")]
// Phase 1 end-to-end suite. It authenticates with the dev-only `x-user-id` header, so it only
// builds with `--features dev-auth`; auth_flows.rs covers the real bearer-token path.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use twoeat_api::{
    AppConfig, AppState,
    state::{ManualClock, MemoryDocuments},
};
use twoeat_domain::PricingPolicy;
use twoeat_payments::{MockPaymentGateway, PaymentGateway};
use twoeat_persistence::{MemoryStore, Store};
use uuid::Uuid;

const ADMIN_ID: &str = "00000000-0000-4000-8000-0000000000ad";
const PDF: &[u8] = b"%PDF-1.4 fake certificate";

struct App {
    router: Router,
    store: Arc<MemoryStore>,
    pay: Arc<MockPaymentGateway>,
    clock: Arc<ManualClock>,
}

fn app() -> App {
    let store = Arc::new(MemoryStore::new());
    let pay = Arc::new(MockPaymentGateway::new());
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let state = AppState::new(
        store.clone(),
        pay.clone(),
        Arc::new(MemoryDocuments::default()),
        clock.clone(),
        AppConfig::new(PricingPolicy::default()),
    );
    App { router: twoeat_api::router(state), store, pay, clock }
}

struct Call<'a> {
    method: Method,
    uri: &'a str,
    user: Option<Uuid>,
    admin: bool,
    body: Option<Value>,
}

async fn send(app: &App, c: Call<'_>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(c.method).uri(c.uri);
    if let Some(u) = c.user {
        req = req.header("x-user-id", u.to_string());
    }
    if c.admin {
        // The x-admin-token mechanism is gone: admins are ordinary users with the admin role.
        req = req.header("x-user-id", ADMIN_ID).header("x-user-role", "admin");
    }
    let body = match c.body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn post(app: &App, uri: &str, user: Option<Uuid>, body: Value) -> (StatusCode, Value) {
    send(app, Call { method: Method::POST, uri, user, admin: false, body: Some(body) }).await
}
async fn post_empty(app: &App, uri: &str, user: Uuid) -> (StatusCode, Value) {
    send(app, Call { method: Method::POST, uri, user: Some(user), admin: false, body: None }).await
}
async fn get(app: &App, uri: &str, user: Option<Uuid>) -> (StatusCode, Value) {
    send(app, Call { method: Method::GET, uri, user, admin: false, body: None }).await
}
async fn admin_post(app: &App, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    send(app, Call { method: Method::POST, uri, user: None, admin: true, body }).await
}

fn multipart(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let b = "XTESTBOUNDARYX";
    let mut out = Vec::new();
    for (k, v) in fields {
        out.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").into_bytes());
    }
    if let Some((ct, bytes)) = file {
        out.extend(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"cert_document\"; filename=\"c\"\r\nContent-Type: {ct}\r\n\r\n")
                .into_bytes(),
        );
        out.extend(bytes);
        out.extend(b"\r\n");
    }
    out.extend(format!("--{b}--\r\n").into_bytes());
    (format!("multipart/form-data; boundary={b}"), out)
}

async fn register_raw(app: &App, fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> (StatusCode, Value) {
    let (ct, body) = multipart(fields, file);
    let req = Request::builder().method(Method::POST).uri("/cooks/register").header("content-type", ct).body(Body::from(body)).unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn register_cook(app: &App, name: &str, lat: f64, lng: f64) -> Uuid {
    let (lat, lng) = (lat.to_string(), lng.to_string());
    let (s, v) = register_raw(
        app,
        &[("display_name", name), ("lat", &lat), ("lng", &lng), ("accept_hbfb_terms", "true")],
        Some(("application/pdf", PDF)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["can_sell"], false);
    v["cook_id"].as_str().unwrap().parse().unwrap()
}

async fn verify(app: &App, cook: Uuid) {
    let exp = (app.clock_now() + Duration::days(365)).to_rfc3339();
    let (s, v) = admin_post(app, &format!("/admin/cooks/{cook}/certification/approve"), Some(json!({"expires_at": exp}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["can_sell"], true);
}

impl App {
    fn clock_now(&self) -> chrono::DateTime<Utc> {
        use twoeat_api::state::Clock;
        self.clock.now()
    }
}

async fn verified_cook_with_dish(app: &App) -> (Uuid, Uuid) {
    let cook = register_cook(app, "Auntie Mei", 1.3521, 103.8198).await;
    verify(app, cook).await;
    let (s, v) = post(app, "/dishes", Some(cook), json!({"name": "Chicken rice", "price_cents": 650})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    (cook, v["id"].as_str().unwrap().parse().unwrap())
}

fn order_body(cook: Uuid, dish: Uuid, qty: u32, fulfillment: Value) -> Value {
    json!({"cook_id": cook, "items": [{"dish_id": dish, "quantity": qty}], "fulfillment": fulfillment})
}

async fn place_order(app: &App, cook: Uuid, dish: Uuid, qty: u32, fulfillment: Value) -> (Uuid, Uuid, Value) {
    let eater = Uuid::new_v4();
    let (s, v) = post(app, "/orders", Some(eater), order_body(cook, dish, qty, fulfillment)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    (v["id"].as_str().unwrap().parse().unwrap(), eater, v)
}

fn delivery() -> Value {
    json!({"type": "delivery", "address": "10 Example Ave #02-03", "postal_code": "123456"})
}

#[tokio::test]
async fn uncertified_cook_is_blocked_until_admin_verifies() {
    let app = app();
    let cook = register_cook(&app, "New Cook", 1.30, 103.80).await;

    let (s, v) = post(&app, "/dishes", Some(cook), json!({"name": "Laksa", "price_cents": 800})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"]["code"], "certification_not_verified");
    assert_eq!(v["error"]["type"], "regulatory_violation");

    // Not discoverable while unverified.
    let (_, v) = get(&app, "/cooks/nearby?lat=1.30&lng=103.80", None).await;
    assert_eq!(v, json!([]));

    verify(&app, cook).await;
    let (s, _) = post(&app, "/dishes", Some(cook), json!({"name": "Laksa", "price_cents": 800})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, v) = get(&app, "/cooks/nearby?lat=1.30&lng=103.80", None).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn registration_enforces_terms_document_and_location() {
    let app = app();
    let base = [("display_name", "C"), ("lat", "1.30"), ("lng", "103.80")];

    let mut no_terms = base.to_vec();
    no_terms.push(("accept_hbfb_terms", "false"));
    let (s, v) = register_raw(&app, &no_terms, Some(("application/pdf", PDF))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("hbfb_terms_not_accepted")));

    let mut ok = base.to_vec();
    ok.push(("accept_hbfb_terms", "true"));
    let (s, v) = register_raw(&app, &ok, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("cert_document"));

    let (s, _) = register_raw(&app, &ok, Some(("application/pdf", b"not a pdf"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "content is sniffed, not just the declared type");
    let (s, _) = register_raw(&app, &ok, Some(("text/html", b"<html>"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let overseas = [("display_name", "C"), ("lat", "37.77"), ("lng", "-122.42"), ("accept_hbfb_terms", "true")];
    let (s, _) = register_raw(&app, &overseas, Some(("application/pdf", PDF))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn onboarding_terms_state_real_requirements_not_a_licence() {
    let app = app();
    let (s, v) = get(&app, "/cooks/onboarding-terms", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["required_certification"], "Food Safety Course Level 1 (WSQ)");
    let text = v["conditions"].to_string().to_lowercase();
    assert!(text.contains("external signage") && text.contains("household"));
}

#[tokio::test]
async fn prohibited_dishes_are_rejected() {
    let app = app();
    let (cook, _) = verified_cook_with_dish(&app).await;

    let (s, v) = post(&app, "/dishes", Some(cook), json!({"name": "Party tray", "price_cents": 9000, "is_catering": true})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("catering_prohibited")));

    let (s, v) = post(&app, "/dishes", Some(cook), json!({"name": "Oysters", "price_cents": 2000, "is_raw_seafood": true})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("raw_seafood_prohibited")));

    // Phase 2: a keyword match alone no longer rejects; it is created inactive and flagged for admin review.
    let (s, v) = post(&app, "/dishes", Some(cook), json!({"name": "Salmon sashimi", "price_cents": 1800})).await;
    assert_eq!((s, v["review_status"].as_str(), v["is_active"].as_bool()), (StatusCode::CREATED, Some("pending_review"), Some(false)));

    let (_, dishes) = get(&app, &format!("/cooks/{cook}/dishes"), None).await;
    assert_eq!(dishes.as_array().unwrap().len(), 1, "nothing prohibited is listed");
}

#[tokio::test]
async fn prohibited_orders_are_rejected() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let eater = Uuid::new_v4();

    for (purpose, code) in [("catering", "catering_prohibited"), ("resale_to_food_retailer", "sale_to_food_retailer_prohibited")] {
        let mut body = order_body(cook, dish, 1, json!({"type": "pickup"}));
        body["purpose"] = json!(purpose);
        let (s, v) = post(&app, "/orders", Some(eater), body).await;
        assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some(code)));
    }
    let (s, v) = post(&app, "/orders", Some(eater), order_body(cook, dish, 50, json!({"type": "pickup"}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("catering_prohibited")));
}

#[tokio::test]
async fn pickup_flow_end_to_end() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, eater, v) = place_order(&app, cook, dish, 2, json!({"type": "pickup"})).await;
    assert_eq!((v["status"].as_str(), v["total_cents"].as_i64()), (Some("placed"), Some(1365)));

    let (s, v) = post_empty(&app, &format!("/orders/{order}/accept"), cook).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("accepted_by_cook")));
    let (_, v) = post_empty(&app, &format!("/orders/{order}/ready"), cook).await;
    assert_eq!(v["status"], "ready_for_pickup");
    let (_, v) = post_empty(&app, &format!("/orders/{order}/complete"), cook).await;
    assert_eq!(v["status"], "completed");

    let (s, v) = get(&app, &format!("/orders/{order}"), Some(eater)).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("completed")));
}

#[tokio::test]
async fn delivery_flow_goes_out_for_delivery() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, _, v) = place_order(&app, cook, dish, 1, delivery()).await;
    assert_eq!(v["delivery_fee_cents"], 500);
    post_empty(&app, &format!("/orders/{order}/accept"), cook).await;
    let (_, v) = post_empty(&app, &format!("/orders/{order}/ready"), cook).await;
    assert_eq!(v["status"], "out_for_delivery");
    let (s, v) = post_empty(&app, &format!("/orders/{order}/complete"), cook).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("completed")));
}

#[tokio::test]
async fn accept_fails_after_certificate_expires_and_order_can_still_be_released() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, eater, _) = place_order(&app, cook, dish, 1, json!({"type": "pickup"})).await;

    app.clock.set(app.clock_now() + Duration::days(400));
    let (s, v) = post_empty(&app, &format!("/orders/{order}/accept"), cook).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("certification_expired")));

    // Expired cooks vanish from discovery and can't take new orders or list dishes.
    let (_, v) = get(&app, "/cooks/nearby?lat=1.3521&lng=103.8198", None).await;
    assert_eq!(v, json!([]));
    let (s, v) = post(&app, "/orders", Some(eater), order_body(cook, dish, 1, json!({"type": "pickup"}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("certification_expired")));

    // The eater is not stuck: cancelling releases the hold.
    let pay = app.store.get_order(order).await.unwrap().payment_ref.unwrap();
    let (s, v) = post_empty(&app, &format!("/orders/{order}/cancel"), eater).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("cancelled")));
    assert_eq!(app.pay.state_of(&pay), Some(twoeat_payments::PaymentState::Voided));
}

#[tokio::test]
async fn revoked_cook_cannot_accept() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, _, _) = place_order(&app, cook, dish, 1, json!({"type": "pickup"})).await;
    let (s, _) = admin_post(&app, &format!("/admin/cooks/{cook}/revoke"), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = post_empty(&app, &format!("/orders/{order}/accept"), cook).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("certification_not_verified")));
}

#[tokio::test]
async fn full_refund_includes_platform_and_delivery_fee_and_is_not_repeatable() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, eater, placed) = place_order(&app, cook, dish, 2, delivery()).await;
    let total = placed["total_cents"].as_i64().unwrap();
    assert!(placed["platform_fee_cents"].as_i64().unwrap() > 0);
    post_empty(&app, &format!("/orders/{order}/accept"), cook).await;
    let pay = app.store.get_order(order).await.unwrap().payment_ref.unwrap();
    assert_eq!(app.pay.state_of(&pay), Some(twoeat_payments::PaymentState::Captured));

    let uri = format!("/orders/{order}/refund");
    let (s, v) = post(&app, &uri, Some(eater), json!({"reason": "unfulfilled"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Phase 2: the refund endpoint answers { status, refund_id }; the order carries the amounts.
    assert_eq!(v["status"], "approved");
    let refund_id = v["refund_id"].as_str().unwrap().to_owned();
    let (_, o) = get(&app, &format!("/orders/{order}"), Some(eater)).await;
    assert_eq!(o["status"], "refunded");
    assert_eq!(o["refund"]["amount_cents"].as_i64(), Some(total), "refund equals the full total");
    assert_eq!(app.pay.state_of(&pay), Some(twoeat_payments::PaymentState::Refunded));

    // Phase 2: one refund per order, ever. Asking again is idempotent: same refund, no second payout.
    let (s, v) = post(&app, &uri, Some(eater), json!({"reason": "unfulfilled"})).await;
    assert_eq!((s, v["refund_id"].as_str()), (StatusCode::OK, Some(refund_id.as_str())));
}

#[tokio::test]
async fn refund_of_unaccepted_order_releases_the_hold() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, eater, _) = place_order(&app, cook, dish, 1, json!({"type": "pickup"})).await;
    let pay = app.store.get_order(order).await.unwrap().payment_ref.unwrap();
    let (s, v) = post(&app, &format!("/orders/{order}/refund"), Some(eater), json!({"reason": "other"})).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("approved")));
    assert_eq!(app.store.get_order(order).await.unwrap().status, twoeat_domain::OrderStatus::Cancelled);
    assert_eq!(app.pay.state_of(&pay), Some(twoeat_payments::PaymentState::Voided));
}

#[tokio::test]
async fn completed_orders_refund_only_for_incorrect_or_missing() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, eater, _) = place_order(&app, cook, dish, 1, json!({"type": "pickup"})).await;
    for step in ["accept", "ready", "complete"] {
        post_empty(&app, &format!("/orders/{order}/{step}"), cook).await;
    }
    let uri = format!("/orders/{order}/refund");
    let (s, _) = post(&app, &uri, Some(eater), json!({"reason": "other"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = post(&app, &uri, Some(eater), json!({"reason": "incorrect_order"})).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("approved")));
    assert_eq!(app.store.get_order(order).await.unwrap().status, twoeat_domain::OrderStatus::Refunded);
}

#[tokio::test]
async fn payment_decline_creates_no_order() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    app.pay.set_decline_authorizations(true);
    let (s, v) = post(&app, "/orders", Some(Uuid::new_v4()), order_body(cook, dish, 1, json!({"type": "pickup"}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::PAYMENT_REQUIRED, Some("payment_declined")));
}

#[tokio::test]
async fn capture_failure_unwinds_acceptance_and_releases_hold() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, _, _) = place_order(&app, cook, dish, 1, json!({"type": "pickup"})).await;
    let pay = app.store.get_order(order).await.unwrap().payment_ref.unwrap();
    app.pay.set_fail_captures(true);
    let (s, _) = post_empty(&app, &format!("/orders/{order}/accept"), cook).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(app.store.get_order(order).await.unwrap().status, twoeat_domain::OrderStatus::Cancelled);
    assert_eq!(app.pay.state_of(&pay), Some(twoeat_payments::PaymentState::Voided));
}

#[tokio::test]
async fn authorization_rules() {
    let app = app();
    let (cook, dish) = verified_cook_with_dish(&app).await;
    let (order, eater, _) = place_order(&app, cook, dish, 1, json!({"type": "pickup"})).await;
    let uri = format!("/orders/{order}/accept");

    let (s, _) = send(&app, Call { method: Method::POST, uri: &uri, user: None, admin: false, body: None }).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = post_empty(&app, &uri, eater).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "eater can't accept");
    let (s, _) = post_empty(&app, &uri, Uuid::new_v4()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "other cook can't accept");

    let (s, _) = get(&app, &format!("/orders/{order}"), Some(Uuid::new_v4())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = post(&app, &format!("/orders/{order}/refund"), Some(Uuid::new_v4()), json!({"reason": "other"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Phase 2: no x-admin-token any more; anonymous callers get 401 and non-admins 403.
    let (s, _) = post(&app, &format!("/admin/cooks/{cook}/revoke"), None, json!({})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "admin endpoints need an authenticated admin");
    let (s, _) = post(&app, &format!("/admin/cooks/{cook}/revoke"), Some(eater), json!({})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "non-admins are refused");

    // Eaters can cancel only before acceptance; afterwards it's a refund.
    post_empty(&app, &uri, cook).await;
    let (s, _) = post_empty(&app, &format!("/orders/{order}/cancel"), eater).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn nearby_hides_exact_location_and_orders_by_distance() {
    let app = app();
    let near = register_cook(&app, "Near", 1.3010, 103.8000).await;
    let far = register_cook(&app, "Far", 1.3100, 103.8000).await;
    verify(&app, near).await;
    verify(&app, far).await;
    let (s, v) = get(&app, "/cooks/nearby?lat=1.3&lng=103.8&radius_m=2000", None).await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<_> = v.as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, [near.to_string(), far.to_string()]);
    assert_eq!(v[0]["distance_m"], 200, "111 m rounds up to 200");
    let raw = v.to_string();
    assert!(!raw.contains("lat") && !raw.contains("lng"));

    let (s, _) = get(&app, "/cooks/nearby?lat=1.3&lng=103.8&radius_m=999999", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn openapi_spec_documents_the_api() {
    let app = app();
    let (s, v) = get(&app, "/openapi.json", None).await;
    assert_eq!(s, StatusCode::OK);
    for p in [
        "/cooks/register",
        "/cooks/nearby",
        "/dishes",
        "/orders",
        "/orders/{id}/accept",
        "/orders/{id}/refund",
        "/admin/cooks/{id}/certification/approve",
    ] {
        assert!(v["paths"].get(p).is_some(), "missing {p}");
    }
    assert_eq!(v["info"]["title"], "2Eat API");
    assert!(v["components"]["securitySchemes"]["bearer_auth"].is_object());
}

// Compile-time check that the gateway trait object can be shared.
#[allow(dead_code)]
fn _assert_object_safe(_: Arc<dyn PaymentGateway>, _: Arc<dyn Store>) {}
