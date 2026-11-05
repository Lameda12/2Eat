//! The real stack end to end: HTTP router -> PgStore (PostgreSQL), real bearer
//! auth, mock payments. With `--features postgis`, discovery runs on PostGIS.
//! Skipped unless `DATABASE_URL` is set.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use twoeat_api::{
    AppConfig, AppState,
    auth::{
        limits::{RateLimitConfig, RateLimits},
        password::hash_password,
        token::digest_of,
    },
    state::{MemoryDocuments, SystemClock},
};
use twoeat_domain::{PricingPolicy, Role};
use twoeat_payments::{MockPaymentGateway, PaymentState};
use twoeat_persistence::{AuthStore, NewUser, PgStore};
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

async fn setup() -> Option<(Router, PgStore, Arc<MockPaymentGateway>)> {
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
    let pay = Arc::new(MockPaymentGateway::new());
    let limits = RateLimits::new(RateLimitConfig {
        login_per_ip_per_min: 1000,
        login_per_email_per_min: 1000,
        register_per_ip_per_min: 1000,
        register_per_email_per_min: 1000,
        trust_forwarded_for: false,
    });
    #[allow(unused_mut)]
    let mut state = AppState::new(
        Arc::new(pg.clone()),
        pay.clone(),
        Arc::new(MemoryDocuments::default()),
        Arc::new(SystemClock),
        AppConfig::new(PricingPolicy::default()),
    )
    .with_limits(limits);
    #[cfg(feature = "postgis")]
    {
        let repo = twoeat_persistence::PostgisCookLocationRepo::new(pg.pool().clone()).await.expect("PostGIS");
        state = state.with_locations(Arc::new(repo));
    }
    Some((twoeat_api::router(state), pg, pay))
}

async fn call(app: &Router, method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn signup(app: &Router, role: &str) -> (Uuid, String, String) {
    let e = format!("{role}-{}@example.com", Uuid::new_v4().simple());
    let (s, _) = call(app, Method::POST, "/auth/register", None, Some(json!({"email": e, "password": PASSWORD, "role": role}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, v) = call(app, Method::POST, "/auth/login", None, Some(json!({"email": e, "password": PASSWORD}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (v["user"]["id"].as_str().unwrap().parse().unwrap(), v["token"].as_str().unwrap().to_owned(), e)
}

#[tokio::test]
async fn full_stack_on_postgres() {
    let Some((app, pg, pay)) = setup().await else { return };

    // --- identity: only digests are stored
    let (eater, etok, eemail) = signup(&app, "eater").await;
    let row = sqlx::query("SELECT password_hash, role FROM users WHERE id = $1").bind(eater).fetch_one(pg.pool()).await.unwrap();
    assert!(row.get::<String, _>("password_hash").starts_with("$argon2id$"));
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM sessions WHERE user_id = $1").bind(eater).fetch_one(pg.pool()).await.unwrap();
    assert_eq!(stored, digest_of(&etok).to_vec(), "sessions.token_hash is the SHA-256 of the token");
    assert_ne!(stored, etok.as_bytes());
    let any_plain: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE convert_from(token_hash, 'LATIN1') = $1")
        .bind(&etok)
        .fetch_one(pg.pool())
        .await
        .unwrap();
    assert_eq!(any_plain, 0);
    // citext: case-insensitive login, duplicate registration silently ignored.
    let (s, _) = call(&app, Method::POST, "/auth/login", None, Some(json!({"email": eemail.to_uppercase(), "password": PASSWORD}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        Method::POST,
        "/auth/register",
        None,
        Some(json!({"email": eemail.to_uppercase(), "password": "another password", "role": "cook"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let users: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1::citext").bind(&eemail).fetch_one(pg.pool()).await.unwrap();
    assert_eq!(users, 1);

    // refresh is atomic: exactly one live session per rotation.
    let (s, v) = call(&app, Method::POST, "/auth/refresh", Some(&etok), None).await;
    assert_eq!(s, StatusCode::OK);
    let etok2 = v["token"].as_str().unwrap().to_owned();
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1 AND revoked_at IS NULL")
        .bind(eater)
        .fetch_one(pg.pool())
        .await
        .unwrap();
    assert_eq!(live, 2, "the login session from the case-insensitive login + the rotated one");
    assert_eq!(call(&app, Method::POST, "/auth/refresh", Some(&etok), None).await.0, StatusCode::UNAUTHORIZED);

    // --- admin + cook
    let (cook, ctok, _) = signup(&app, "cook").await;
    let admin_id = Uuid::new_v4();
    let admin_email = format!("admin-{}@example.com", Uuid::new_v4().simple());
    pg.create_user(NewUser {
        id: admin_id,
        email: admin_email.clone(),
        password_hash: hash_password(PASSWORD).await.unwrap(),
        role: Role::Admin,
        created_at: Utc::now(),
    })
    .await
    .unwrap();
    let (_, v) = call(&app, Method::POST, "/auth/login", None, Some(json!({"email": admin_email, "password": PASSWORD}))).await;
    let atok = v["token"].as_str().unwrap().to_owned();

    // cook profile (multipart)
    let b = "XB";
    let mut body = Vec::new();
    for (k, val) in [("display_name", "Auntie Mei"), ("lat", "1.3521"), ("lng", "103.8198"), ("accept_hbfb_terms", "true")] {
        body.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{val}\r\n").into_bytes());
    }
    body.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"cert_document\"; filename=\"c.pdf\"\r\nContent-Type: application/pdf\r\n\r\n%PDF-1.4 x\r\n--{b}--\r\n").into_bytes());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/cooks/register")
        .header("authorization", format!("Bearer {ctok}"))
        .header("content-type", format!("multipart/form-data; boundary={b}"))
        .body(Body::from(body))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);

    // cannot sell until approved; the review queue shows the submission
    let (s, v) = call(&app, Method::POST, "/dishes", Some(&ctok), Some(json!({"name": "Laksa", "price_cents": 800}))).await;
    assert_eq!((s, v["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("certification_not_verified")));
    let (_, q) = call(&app, Method::GET, "/admin/reviews?status=pending", Some(&atok), None).await;
    let review = q["items"][0]["review_id"].as_str().unwrap().to_owned();
    let (s, v) = call(
        &app,
        Method::POST,
        &format!("/admin/reviews/{review}/approve"),
        Some(&atok),
        Some(json!({"expires_at": (Utc::now() + Duration::days(300)).to_rfc3339()})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = call(&app, Method::POST, "/dishes", Some(&ctok), Some(json!({"name": "Laksa", "price_cents": 800}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let dish = v["id"].as_str().unwrap().to_owned();

    // --- discovery privacy (PostGIS repo when built with --features postgis)
    let (s, v) = call(&app, Method::GET, "/cooks/nearby?lat=1.3531&lng=103.8198&radius_m=1000", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["distance_m"], 200, "~111 m rounds UP to the next 100 m");
    let raw = v.to_string();
    assert!(!raw.contains("lat") && !raw.contains("lng") && !raw.contains("location"), "never exact coordinates: {raw}");
    assert_eq!(v[0]["id"], cook.to_string());

    // --- order -> accept -> refund (delivery: items + delivery fee + platform fee) in one transaction chain
    let (s, o) = call(
        &app,
        Method::POST,
        "/orders",
        Some(&etok2),
        Some(json!({"cook_id": cook, "items": [{"dish_id": dish, "quantity": 3}],
        "fulfillment": {"type": "delivery", "address": "10 Example Ave #02-03", "postal_code": "123456"}})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    let oid = o["id"].as_str().unwrap().to_owned();
    let total = o["total_cents"].as_i64().unwrap();
    assert_eq!(
        total,
        o["subtotal_cents"].as_i64().unwrap() + o["platform_fee_cents"].as_i64().unwrap() + o["delivery_fee_cents"].as_i64().unwrap()
    );
    assert_eq!(call(&app, Method::POST, &format!("/orders/{oid}/accept"), Some(&ctok), None).await.0, StatusCode::OK);
    let (s, r) = call(&app, Method::POST, &format!("/orders/{oid}/refund"), Some(&etok2), Some(json!({"reason": "unfulfilled"}))).await;
    assert_eq!((s, r["status"].as_str()), (StatusCode::OK, Some("approved")), "{r}");
    let pay_ref: String =
        sqlx::query_scalar("SELECT payment_ref FROM orders WHERE id = $1::uuid").bind(&oid).fetch_one(pg.pool()).await.unwrap();
    assert_eq!(pay.state_of(&pay_ref), Some(PaymentState::Refunded));
    assert_eq!(pay.refunded_cents(&pay_ref), Some(total));
    let (amount, status): (i64, String) =
        sqlx::query_as("SELECT refund_amount_cents, status FROM orders WHERE id = $1::uuid").bind(&oid).fetch_one(pg.pool()).await.unwrap();
    assert_eq!((amount, status.as_str()), (total, "refunded"));

    // --- audit trail written by the stack, readable by admins only
    let (s, log) = call(
        &app,
        Method::GET,
        &format!("/admin/audit?target_type=refund&target_id={}", r["refund_id"].as_str().unwrap()),
        Some(&atok),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let actions: Vec<_> = log.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap().to_owned()).collect();
    assert_eq!(actions, ["refund.requested", "refund.settled"]);
    assert_eq!(call(&app, Method::GET, "/admin/audit", Some(&etok2), None).await.0, StatusCode::FORBIDDEN);
    let (_, all) = call(&app, Method::GET, "/admin/audit?limit=200", Some(&atok), None).await;
    assert!(all.as_array().unwrap().iter().any(|a| a["action"] == "certificate.approved" && a["actor_id"] == admin_id.to_string()));
}
