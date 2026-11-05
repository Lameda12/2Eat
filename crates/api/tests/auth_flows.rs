//! End-to-end tests over HTTP using real bearer-token authentication
//! (no `dev-auth`). In-memory store + mock payments.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Method, Request, StatusCode},
};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use twoeat_api::{
    AppConfig, AppState,
    auth::{
        limits::{RateLimitConfig, RateLimits},
        password::hash_password,
    },
    state::{ManualClock, MemoryDocuments},
};
use twoeat_domain::{PricingPolicy, Role};
use twoeat_payments::{MockPaymentGateway, PaymentState};
use twoeat_persistence::{AuthStore, MemoryStore, NewUser, Store};
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";
const PDF: &[u8] = b"%PDF-1.4 fake certificate";

struct App {
    router: Router,
    store: Arc<MemoryStore>,
    pay: Arc<MockPaymentGateway>,
    clock: Arc<ManualClock>,
}

fn roomy_limits() -> RateLimits {
    RateLimits::new(RateLimitConfig {
        login_per_ip_per_min: 1000,
        login_per_email_per_min: 1000,
        register_per_ip_per_min: 1000,
        register_per_email_per_min: 1000,
        trust_forwarded_for: false,
    })
}

fn app_with(limits: RateLimits) -> App {
    let store = Arc::new(MemoryStore::new());
    let pay = Arc::new(MockPaymentGateway::new());
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let state = AppState::new(
        store.clone(),
        pay.clone(),
        Arc::new(MemoryDocuments::default()),
        clock.clone(),
        AppConfig::new(PricingPolicy::default()),
    )
    .with_limits(limits);
    App { router: twoeat_api::router(state), store, pay, clock }
}

fn app() -> App {
    app_with(roomy_limits())
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

async fn send(app: &App, method: Method, uri: &str, token: Option<&str>, body: Option<Value>, ip: Option<&str>) -> Resp {
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
    let mut req = req.body(body).unwrap();
    if let Some(ip) = ip {
        let addr: SocketAddr = format!("{ip}:40000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
    }
    into_resp(app.router.clone().oneshot(req).await.unwrap()).await
}

async fn into_resp(res: axum::response::Response) -> Resp {
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Resp { status, headers, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null) }
}

async fn post(app: &App, uri: &str, token: Option<&str>, body: Value) -> Resp {
    send(app, Method::POST, uri, token, Some(body), None).await
}
async fn post_empty(app: &App, uri: &str, token: &str) -> Resp {
    send(app, Method::POST, uri, Some(token), None, None).await
}
async fn get(app: &App, uri: &str, token: Option<&str>) -> Resp {
    send(app, Method::GET, uri, token, None, None).await
}

fn email(tag: &str) -> String {
    format!("{tag}-{}@example.com", Uuid::new_v4().simple())
}

/// Register + login. Returns (user id, token).
async fn signup(app: &App, role: &str) -> (Uuid, String) {
    let e = email(role);
    let r = post(app, "/auth/register", None, json!({"email": e, "password": PASSWORD, "role": role})).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    login(app, &e, PASSWORD).await
}

async fn login(app: &App, e: &str, pw: &str) -> (Uuid, String) {
    let r = post(app, "/auth/login", None, json!({"email": e, "password": pw})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    (r.body["user"]["id"].as_str().unwrap().parse().unwrap(), r.body["token"].as_str().unwrap().to_owned())
}

async fn admin_token(app: &App) -> (Uuid, String) {
    let e = email("admin");
    let id = Uuid::new_v4();
    let password_hash = hash_password(PASSWORD).await.unwrap();
    app.store.create_user(NewUser { id, email: e.clone(), password_hash, role: Role::Admin, created_at: Utc::now() }).await.unwrap();
    login(app, &e, PASSWORD).await
}

fn multipart(fields: &[(&str, &str)], file: Option<&[u8]>) -> (String, Vec<u8>) {
    let b = "XTESTBOUNDARYX";
    let mut out = Vec::new();
    for (k, v) in fields {
        out.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").into_bytes());
    }
    if let Some(bytes) = file {
        out.extend(format!("--{b}\r\nContent-Disposition: form-data; name=\"cert_document\"; filename=\"c.pdf\"\r\nContent-Type: application/pdf\r\n\r\n").into_bytes());
        out.extend(bytes);
        out.extend(b"\r\n");
    }
    out.extend(format!("--{b}--\r\n").into_bytes());
    (format!("multipart/form-data; boundary={b}"), out)
}

async fn register_cook_profile(app: &App, token: Option<&str>) -> Resp {
    let (ct, body) =
        multipart(&[("display_name", "Auntie Mei"), ("lat", "1.3521"), ("lng", "103.8198"), ("accept_hbfb_terms", "true")], Some(PDF));
    let mut req = Request::builder().method(Method::POST).uri("/cooks/register").header("content-type", ct);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    into_resp(app.router.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap()).await
}

/// A verified cook with one dish. Returns (cook id, cook token, dish id, admin token).
async fn verified_cook(app: &App) -> (Uuid, String, Uuid, String) {
    let (cook, ctok) = signup(app, "cook").await;
    let r = register_cook_profile(app, Some(&ctok)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["cook_id"].as_str().unwrap(), cook.to_string(), "cook id is the user id");
    let (_, atok) = admin_token(app).await;
    let review = app.store.latest_submission(cook).await.unwrap().unwrap().id;
    let exp = (Utc::now() + Duration::days(365)).to_rfc3339();
    let r = post(app, &format!("/admin/reviews/{review}/approve"), Some(&atok), json!({"expires_at": exp})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = post(app, "/dishes", Some(&ctok), json!({"name": "Chicken rice", "price_cents": 650})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    (cook, ctok, r.body["id"].as_str().unwrap().parse().unwrap(), atok)
}

fn pickup() -> Value {
    json!({"type": "pickup"})
}
fn delivery() -> Value {
    json!({"type": "delivery", "address": "10 Example Ave #02-03", "postal_code": "123456"})
}

async fn accepted_order(app: &App, cook: Uuid, ctok: &str, dish: Uuid, etok: &str, fulfillment: Value) -> Uuid {
    let r =
        post(app, "/orders", Some(etok), json!({"cook_id": cook, "items": [{"dish_id": dish, "quantity": 2}], "fulfillment": fulfillment}))
            .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let id: Uuid = r.body["id"].as_str().unwrap().parse().unwrap();
    let r = post_empty(app, &format!("/orders/{id}/accept"), ctok).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    id
}

// ============================================================ authentication

#[tokio::test]
async fn register_login_use_logout() {
    let app = app();
    let e = email("eater");
    let r = post(&app, "/auth/register", None, json!({"email": e, "password": PASSWORD, "role": "eater"})).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);

    let r = post(&app, "/auth/login", None, json!({"email": e.to_uppercase(), "password": PASSWORD})).await;
    assert_eq!(r.status, StatusCode::OK, "emails are case-insensitive");
    let token = r.body["token"].as_str().unwrap().to_owned();
    assert_eq!(r.body["user"]["role"], "eater");
    assert_eq!(token.len(), 43, "32 bytes, base64url without padding");
    let exp: chrono::DateTime<Utc> = r.body["expires_at"].as_str().unwrap().parse().unwrap();
    assert!((exp - Utc::now() - Duration::days(30)).num_seconds().abs() < 60, "30-day expiry");

    // Anonymous is rejected, the token is accepted (404 = got past auth, order doesn't exist).
    let missing = Uuid::new_v4();
    let anon = get(&app, &format!("/orders/{missing}"), None).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anon.headers.get("www-authenticate").unwrap(), "Bearer");
    assert_eq!(get(&app, &format!("/orders/{missing}"), Some(&token)).await.status, StatusCode::NOT_FOUND);

    // Logout revokes exactly this token.
    let (_, other) = login(&app, &e, PASSWORD).await;
    assert_eq!(post_empty(&app, "/auth/logout", &token).await.status, StatusCode::NO_CONTENT);
    assert_eq!(get(&app, &format!("/orders/{missing}"), Some(&token)).await.status, StatusCode::UNAUTHORIZED, "revoked token");
    assert_eq!(get(&app, &format!("/orders/{missing}"), Some(&other)).await.status, StatusCode::NOT_FOUND, "other sessions unaffected");
}

#[tokio::test]
async fn refresh_rotates_the_token() {
    let app = app();
    let (_, old) = signup(&app, "eater").await;
    let r = post_empty(&app, "/auth/refresh", &old).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let new = r.body["token"].as_str().unwrap().to_owned();
    assert_ne!(new, old);

    let probe = format!("/orders/{}", Uuid::new_v4());
    assert_eq!(get(&app, &probe, Some(&old)).await.status, StatusCode::UNAUTHORIZED, "old token is dead after rotation");
    assert_eq!(get(&app, &probe, Some(&new)).await.status, StatusCode::NOT_FOUND, "new token works");
    assert_eq!(post_empty(&app, "/auth/refresh", &old).await.status, StatusCode::UNAUTHORIZED, "old token cannot rotate again");
    assert_eq!(post_empty(&app, "/auth/refresh", &new).await.status, StatusCode::OK, "new token can");
}

#[tokio::test]
async fn expired_and_garbage_tokens_are_rejected() {
    let app = app();
    let (_, token) = signup(&app, "eater").await;
    let probe = format!("/orders/{}", Uuid::new_v4());

    app.clock.set(Utc::now() + Duration::days(29));
    assert_eq!(get(&app, &probe, Some(&token)).await.status, StatusCode::NOT_FOUND, "still valid on day 29");
    app.clock.set(Utc::now() + Duration::days(31));
    assert_eq!(get(&app, &probe, Some(&token)).await.status, StatusCode::UNAUTHORIZED, "expired after 30 days");
    assert_eq!(post_empty(&app, "/auth/refresh", &token).await.status, StatusCode::UNAUTHORIZED, "an expired token cannot be refreshed");

    for bad in ["", "garbage", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"] {
        assert_eq!(get(&app, &probe, Some(bad)).await.status, StatusCode::UNAUTHORIZED, "{bad:?}");
    }
    let req = Request::builder().uri(&probe).header("authorization", format!("Basic {token}")).body(Body::empty()).unwrap();
    assert_eq!(app.router.clone().oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED, "wrong scheme");
}

#[tokio::test]
async fn login_does_not_reveal_whether_the_email_exists() {
    let app = app();
    let (_, _) = signup(&app, "eater").await;
    let e = email("known");
    post(&app, "/auth/register", None, json!({"email": e, "password": PASSWORD, "role": "cook"})).await;

    let wrong_pw = post(&app, "/auth/login", None, json!({"email": e, "password": "not the password!"})).await;
    let unknown = post(&app, "/auth/login", None, json!({"email": email("ghost"), "password": "not the password!"})).await;
    assert_eq!(wrong_pw.status, StatusCode::UNAUTHORIZED);
    assert_eq!((wrong_pw.status, &wrong_pw.body), (unknown.status, &unknown.body), "identical status and body");
    assert_eq!(wrong_pw.body["error"]["code"], "invalid_credentials");
    let oversized = post(&app, "/auth/login", None, json!({"email": e, "password": "x".repeat(5000)})).await;
    assert_eq!((oversized.status, &oversized.body), (unknown.status, &unknown.body));

    // Nor does registering: same response whether or not the email is taken, and the original password still works.
    let again = post(&app, "/auth/register", None, json!({"email": e, "password": "a different password", "role": "eater"})).await;
    let fresh = post(&app, "/auth/register", None, json!({"email": email("new"), "password": PASSWORD, "role": "eater"})).await;
    assert_eq!((again.status, &again.body), (fresh.status, &fresh.body));
    assert_eq!(post(&app, "/auth/login", None, json!({"email": e, "password": PASSWORD})).await.status, StatusCode::OK);
    assert_eq!(
        post(&app, "/auth/login", None, json!({"email": e, "password": "a different password"})).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn registration_validation_and_roles() {
    let app = app();
    let bad = [
        json!({"email": "nope", "password": PASSWORD, "role": "eater"}),
        json!({"email": email("x"), "password": "short", "role": "eater"}),
    ];
    for b in bad {
        assert_eq!(post(&app, "/auth/register", None, b).await.status, StatusCode::BAD_REQUEST);
    }
    let admin = post(&app, "/auth/register", None, json!({"email": email("x"), "password": PASSWORD, "role": "admin"})).await;
    assert_eq!(admin.status, StatusCode::FORBIDDEN, "admins are never self-registered");
    assert!(app.store.find_user_by_email("whoever@example.com").await.unwrap().is_none());
}

#[tokio::test]
async fn passwords_are_stored_as_argon2id_and_tokens_only_as_digests() {
    let app = app();
    let e = email("eater");
    post(&app, "/auth/register", None, json!({"email": e, "password": PASSWORD, "role": "eater"})).await;
    let u = app.store.find_user_by_email(&e).await.unwrap().unwrap();
    assert!(u.password_hash.starts_with("$argon2id$v=19$"), "{}", u.password_hash);
    assert!(!u.password_hash.contains(PASSWORD));

    let (_, token) = login(&app, &e, PASSWORD).await;
    let digest = twoeat_api::auth::token::digest_of(&token);
    let s = app.store.find_session(&digest).await.unwrap().expect("found by SHA-256 digest");
    assert_eq!(s.user_id, u.id);
}

// ================================================================ rate limits

#[tokio::test]
async fn login_is_rate_limited_per_email_and_per_ip() {
    let app = app_with(RateLimits::new(RateLimitConfig {
        login_per_ip_per_min: 8,
        login_per_email_per_min: 3,
        register_per_ip_per_min: 2,
        register_per_email_per_min: 2,
        trust_forwarded_for: false,
    }));
    let target = email("victim");
    let attempt = |e: String, ip: &'static str| {
        let app = &app;
        async move { send(app, Method::POST, "/auth/login", None, Some(json!({"email": e, "password": "wrong password!!"})), Some(ip)).await }
    };

    // Per email: 3 allowed (even from different IPs, and even though the account does not exist), the 4th is throttled.
    for ip in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
        assert_eq!(attempt(target.clone(), ip).await.status, StatusCode::UNAUTHORIZED);
    }
    let throttled = attempt(target.clone(), "10.0.0.4").await;
    assert_eq!(throttled.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(throttled.headers.get("retry-after").is_some());
    assert_eq!(throttled.body["error"]["code"], "rate_limited");

    // Per IP: a single address spraying many different emails is cut off after 8.
    let mut codes = vec![];
    for _ in 0..10 {
        codes.push(attempt(email("spray"), "192.0.2.9").await.status);
    }
    assert_eq!(codes.iter().filter(|c| **c == StatusCode::UNAUTHORIZED).count(), 8);
    assert_eq!(codes.iter().filter(|c| **c == StatusCode::TOO_MANY_REQUESTS).count(), 2);
    // Other addresses are unaffected.
    assert_eq!(attempt(email("calm"), "198.51.100.7").await.status, StatusCode::UNAUTHORIZED);

    // Registration has its own limits (2 per IP per minute here).
    let reg = |ip: &'static str| {
        let app = &app;
        async move {
            send(
                app,
                Method::POST,
                "/auth/register",
                None,
                Some(json!({"email": email("r"), "password": PASSWORD, "role": "eater"})),
                Some(ip),
            )
            .await
            .status
        }
    };
    assert_eq!(reg("203.0.113.5").await, StatusCode::ACCEPTED);
    assert_eq!(reg("203.0.113.5").await, StatusCode::ACCEPTED);
    assert_eq!(reg("203.0.113.5").await, StatusCode::TOO_MANY_REQUESTS);
}

// ================================================================ dev-auth gate

#[cfg(not(feature = "dev-auth"))]
#[tokio::test]
async fn x_user_id_is_not_accepted_without_dev_auth() {
    let app = app();
    let (cook, ctok, _dish, _a) = verified_cook(&app).await;
    let id = cook.to_string();
    for (method, uri) in
        [(Method::POST, "/dishes"), (Method::POST, "/orders"), (Method::GET, "/orders/00000000-0000-4000-8000-000000000001")]
    {
        for role in [None, Some("admin")] {
            let mut req =
                Request::builder().method(method.clone()).uri(uri).header("x-user-id", &id).header("content-type", "application/json");
            if let Some(r) = role {
                req = req.header("x-user-role", r);
            }
            let res = app.router.clone().oneshot(req.body(Body::from("{}")).unwrap()).await.unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {uri} role={role:?}");
        }
    }
    let admin_req = Request::builder()
        .method(Method::GET)
        .uri("/admin/reviews")
        .header("x-user-id", &id)
        .header("x-user-role", "admin")
        .header("x-admin-token", "anything")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.router.clone().oneshot(admin_req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED,
        "neither x-user-role nor x-admin-token grant anything"
    );
    // Cook registration is also closed to anonymous callers.
    assert_eq!(register_cook_profile(&app, None).await.status, StatusCode::UNAUTHORIZED);
    // ...and the real token still works.
    assert_eq!(post(&app, "/dishes", Some(&ctok), json!({"name": "Laksa", "price_cents": 800})).await.status, StatusCode::CREATED);
}

#[cfg(feature = "dev-auth")]
#[tokio::test]
async fn dev_auth_is_a_debug_build_convenience_only() {
    // With the feature on, the same checks the server runs at startup refuse a release build.
    assert!(twoeat_api::auth::check_dev_auth_allowed(cfg!(feature = "dev-auth"), true).is_err());
    assert!(twoeat_api::auth::check_dev_auth_allowed(cfg!(feature = "dev-auth"), false).is_ok());
}

// ============================================================== role gating

#[tokio::test]
async fn roles_gate_endpoints() {
    let app = app();
    let (_, eater) = signup(&app, "eater").await;
    let (_, cook) = signup(&app, "cook").await;
    let (_, admin) = admin_token(&app).await;

    assert_eq!(
        post(&app, "/dishes", Some(&eater), json!({"name": "x", "price_cents": 100})).await.status,
        StatusCode::FORBIDDEN,
        "eaters can't list dishes"
    );
    assert_eq!(
        post(&app, "/dishes", Some(&cook), json!({"name": "x", "price_cents": 100})).await.status,
        StatusCode::NOT_FOUND,
        "cook account without a profile"
    );
    assert_eq!(register_cook_profile(&app, Some(&eater)).await.status, StatusCode::FORBIDDEN, "eater accounts can't become cooks");

    for t in [&eater, &cook] {
        assert_eq!(get(&app, "/admin/reviews", Some(t)).await.status, StatusCode::FORBIDDEN);
        assert_eq!(get(&app, "/admin/audit", Some(t)).await.status, StatusCode::FORBIDDEN);
        assert_eq!(get(&app, "/admin/refunds", Some(t)).await.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(get(&app, "/admin/reviews", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(get(&app, "/admin/reviews", Some(&admin)).await.status, StatusCode::OK);
    let legacy = Request::builder().uri("/admin/reviews").header("x-admin-token", "x").body(Body::empty()).unwrap();
    assert_eq!(app.router.clone().oneshot(legacy).await.unwrap().status(), StatusCode::UNAUTHORIZED, "x-admin-token is gone");

    // A second cook profile for the same account is a conflict.
    assert_eq!(register_cook_profile(&app, Some(&cook)).await.status, StatusCode::CREATED);
    assert_eq!(register_cook_profile(&app, Some(&cook)).await.status, StatusCode::CONFLICT);
}

// ====================================================== admin certificate review

#[tokio::test]
async fn admin_review_flow_with_audit_trail() {
    let app = app();
    let (admin_id, admin) = admin_token(&app).await;
    let (c1, t1) = signup(&app, "cook").await;
    let (c2, t2) = signup(&app, "cook").await;
    register_cook_profile(&app, Some(&t1)).await;
    register_cook_profile(&app, Some(&t2)).await;

    // A cook with a review row still pending cannot sell.
    assert_eq!(
        post(&app, "/dishes", Some(&t1), json!({"name": "Laksa", "price_cents": 800})).await.body["error"]["code"],
        "certification_not_verified"
    );

    let q = get(&app, "/admin/reviews?status=pending&limit=1", Some(&admin)).await;
    assert_eq!(q.status, StatusCode::OK);
    assert_eq!(
        (q.body["items"].as_array().unwrap().len(), q.body["total"].as_i64(), q.body["limit"].as_i64()),
        (1, Some(2), Some(1)),
        "paginated"
    );
    let page2 = get(&app, "/admin/reviews?status=pending&limit=1&offset=1", Some(&admin)).await;
    let ids: Vec<_> = [&q, &page2].iter().map(|p| p.body["items"][0]["cook_id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 2, "two different pages");
    assert_eq!(get(&app, "/admin/reviews?status=bogus", Some(&admin)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(get(&app, "/admin/reviews?limit=0", Some(&admin)).await.status, StatusCode::BAD_REQUEST);

    let r1 = app.store.latest_submission(c1).await.unwrap().unwrap().id;
    let r2 = app.store.latest_submission(c2).await.unwrap().unwrap().id;

    // Approve c1.
    let exp = (Utc::now() + Duration::days(200)).to_rfc3339();
    let a =
        post(&app, &format!("/admin/reviews/{r1}/approve"), Some(&admin), json!({"expires_at": exp, "notes": "checked with SkillsFuture"}))
            .await;
    assert_eq!(
        (a.status, a.body["status"].as_str(), a.body["reviewer_id"].as_str()),
        (StatusCode::OK, Some("approved"), Some(admin_id.to_string().as_str()))
    );
    assert_eq!(post(&app, "/dishes", Some(&t1), json!({"name": "Laksa", "price_cents": 800})).await.status, StatusCode::CREATED);
    // Decisions are final.
    assert_eq!(
        post(&app, &format!("/admin/reviews/{r1}/approve"), Some(&admin), json!({"expires_at": exp})).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        post(&app, &format!("/admin/reviews/{r1}/reject"), Some(&admin), json!({"reason": "suspected_fraud"})).await.status,
        StatusCode::CONFLICT
    );
    // Past expiry is refused.
    let past = (Utc::now() - Duration::days(1)).to_rfc3339();
    assert_eq!(
        post(&app, &format!("/admin/reviews/{r2}/approve"), Some(&admin), json!({"expires_at": past})).await.status,
        StatusCode::BAD_REQUEST
    );

    // Reject c2 with the escape-hatch reason.
    assert_eq!(
        post(&app, &format!("/admin/reviews/{r2}/reject"), Some(&admin), json!({"reason": {"other": " "}})).await.status,
        StatusCode::BAD_REQUEST,
        "blank Other"
    );
    let rej = post(
        &app,
        &format!("/admin/reviews/{r2}/reject"),
        Some(&admin),
        json!({"reason": {"other": "photo of a screen"}, "notes": "ask for original"}),
    )
    .await;
    assert_eq!((rej.status, rej.body["status"].as_str()), (StatusCode::OK, Some("rejected")), "{}", rej.body);
    assert_eq!(rej.body["rejection_reason"], json!({"other": "photo of a screen"}));
    assert_eq!(
        post(&app, "/dishes", Some(&t2), json!({"name": "Laksa", "price_cents": 800})).await.body["error"]["code"],
        "certification_not_verified",
        "rejected => cannot sell"
    );
    let (_, nearby) = (0, get(&app, "/cooks/nearby?lat=1.3521&lng=103.8198", None).await);
    let nearby_ids: Vec<_> = nearby.body.as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(nearby_ids, [c1.to_string()], "only the approved cook is discoverable");

    // Filtering the queue.
    assert_eq!(get(&app, "/admin/reviews?status=pending", Some(&admin)).await.body["total"], 0);
    assert_eq!(get(&app, "/admin/reviews?status=rejected", Some(&admin)).await.body["items"][0]["review_id"], r2.to_string());

    // Every decision is in the audit log with the acting admin.
    let log = get(&app, &format!("/admin/audit?target_type=certificate_review&target_id={r2}"), Some(&admin)).await;
    assert_eq!(log.body[0]["action"], "certificate.rejected");
    assert_eq!(log.body[0]["actor_id"], admin_id.to_string());
    assert_eq!(log.body[0]["metadata"]["reason"], "other");
    let log = get(&app, "/admin/audit?action=certificate.approved", Some(&admin)).await;
    assert_eq!(log.body.as_array().unwrap().len(), 1);
    // Revocation is audited and removes the cook from discovery.
    assert_eq!(post_empty(&app, &format!("/admin/cooks/{c1}/revoke"), &admin).await.status, StatusCode::OK);
    assert_eq!(
        get(&app, &format!("/admin/audit?target_id={c1}&action=cook.revoked"), Some(&admin)).await.body.as_array().unwrap().len(),
        1
    );
    assert_eq!(get(&app, "/cooks/nearby?lat=1.3521&lng=103.8198", None).await.body, json!([]));
}

// ===================================================== raw-seafood dish review

#[tokio::test]
async fn keyword_dishes_wait_for_admin_but_declared_ones_are_rejected() {
    let app = app();
    let (cook, ctok, _dish, admin) = verified_cook(&app).await;

    let r = post(&app, "/dishes", Some(&ctok), json!({"name": "Tuna Poke Bowl", "price_cents": 1400})).await;
    assert_eq!(
        (r.status, r.body["review_status"].as_str(), r.body["is_active"].as_bool()),
        (StatusCode::CREATED, Some("pending_review"), Some(false))
    );
    assert_eq!(r.body["review_flag"], "poke");
    let flagged: Uuid = r.body["id"].as_str().unwrap().parse().unwrap();

    // Declared raw seafood is still a hard stop, whatever the name.
    let hard = post(&app, "/dishes", Some(&ctok), json!({"name": "Chicken rice", "price_cents": 700, "is_raw_seafood": true})).await;
    assert_eq!((hard.status, hard.body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("raw_seafood_prohibited")));

    // Not listed, not orderable, until approved.
    assert_eq!(get(&app, &format!("/cooks/{cook}/dishes"), None).await.body.as_array().unwrap().len(), 1);
    let eater = signup(&app, "eater").await.1;
    let o = post(
        &app,
        "/orders",
        Some(&eater),
        json!({"cook_id": cook, "items": [{"dish_id": flagged, "quantity": 1}], "fulfillment": pickup()}),
    )
    .await;
    assert_eq!(o.status, StatusCode::BAD_REQUEST);

    let q = get(&app, "/admin/dishes/flagged", Some(&admin)).await;
    assert_eq!(q.body["total"], 1);
    assert_eq!(post_empty(&app, &format!("/admin/dishes/{flagged}/approve"), &ctok).await.status, StatusCode::FORBIDDEN);
    let ok = post_empty(&app, &format!("/admin/dishes/{flagged}/approve"), &admin).await;
    assert_eq!((ok.status, ok.body["is_active"].as_bool()), (StatusCode::OK, Some(true)));
    assert_eq!(get(&app, &format!("/cooks/{cook}/dishes"), None).await.body.as_array().unwrap().len(), 2);
    assert_eq!(get(&app, &format!("/admin/audit?target_id={flagged}"), Some(&admin)).await.body[0]["action"], "dish.review_approved");
}

// ================================================================== refunds

#[tokio::test]
async fn refund_threshold_boundary_then_admin_review_pays_the_full_total() {
    let app = app();
    let (cook, ctok, dish, admin) = verified_cook(&app).await;
    let (_eater_id, etok) = signup(&app, "eater").await;

    // Refunds 1, 2, 3 (0/1/2 already in the trailing 30 days): automatic, full total.
    for n in 1..=3 {
        let order = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
        let pay = app.store.get_order(order).await.unwrap().payment_ref.unwrap();
        let r = post(&app, &format!("/orders/{order}/refund"), Some(&etok), json!({"reason": "unfulfilled"})).await;
        assert_eq!((r.status, r.body["status"].as_str()), (StatusCode::OK, Some("approved")), "refund #{n}: {}", r.body);
        assert!(r.body["refund_id"].is_string());
        assert_eq!(app.pay.state_of(&pay), Some(PaymentState::Refunded));
    }

    // Refund #4 (3 already): routed to an admin. No money moves.
    let order = accepted_order(&app, cook, &ctok, dish, &etok, delivery()).await; // delivery => delivery fee + platform fee
    let before = app.store.get_order(order).await.unwrap();
    assert!(before.delivery_fee_cents > 0 && before.platform_fee_cents > 0);
    let pay = before.payment_ref.clone().unwrap();
    let r = post(&app, &format!("/orders/{order}/refund"), Some(&etok), json!({"reason": "incorrect_order"})).await;
    assert_eq!((r.status, r.body["status"].as_str()), (StatusCode::ACCEPTED, Some("pending_review")), "{}", r.body);
    let refund_id = r.body["refund_id"].as_str().unwrap().to_owned();
    assert_eq!(app.pay.state_of(&pay), Some(PaymentState::Captured), "no money moved while pending");
    let o = app.store.get_order(order).await.unwrap();
    assert_eq!((o.status, o.refund), (twoeat_domain::OrderStatus::AcceptedByCook, None));
    // Asking again doesn't create another refund or pay anything.
    let again = post(&app, &format!("/orders/{order}/refund"), Some(&etok), json!({"reason": "incorrect_order"})).await;
    assert_eq!((again.status, again.body["refund_id"].as_str()), (StatusCode::ACCEPTED, Some(refund_id.as_str())));
    assert_eq!(app.pay.state_of(&pay), Some(PaymentState::Captured));

    // Only admins can decide.
    assert_eq!(post(&app, &format!("/admin/refunds/{refund_id}/approve"), Some(&etok), json!({})).await.status, StatusCode::FORBIDDEN);
    assert_eq!(post(&app, &format!("/admin/refunds/{refund_id}/approve"), Some(&ctok), json!({})).await.status, StatusCode::FORBIDDEN);

    let pending = get(&app, "/admin/refunds", Some(&admin)).await;
    assert_eq!((pending.body["total"].as_i64(), pending.body["items"][0]["refund_id"].as_str()), (Some(1), Some(refund_id.as_str())));

    // pending_review -> AdminApproved: refunds the FULL total, platform and delivery fee included.
    let ok = post(&app, &format!("/admin/refunds/{refund_id}/approve"), Some(&admin), json!({"notes": "cook confirmed wrong dish"})).await;
    assert_eq!((ok.status, ok.body["status"].as_str()), (StatusCode::OK, Some("approved")), "{}", ok.body);
    assert_eq!(app.pay.state_of(&pay), Some(PaymentState::Refunded));
    assert_eq!(app.pay.refunded_cents(&pay), Some(before.total_cents), "payment provider was asked for the whole total");
    let after = app.store.get_order(order).await.unwrap();
    assert_eq!(after.status, twoeat_domain::OrderStatus::Refunded);
    assert_eq!(after.refund.unwrap().amount_cents, before.subtotal_cents + before.platform_fee_cents + before.delivery_fee_cents);
    let item = get(&app, "/admin/refunds?status=approved&limit=100", Some(&admin)).await;
    let rec = item.body["items"].as_array().unwrap().iter().find(|r| r["refund_id"] == refund_id.as_str()).unwrap().clone();
    assert_eq!((rec["decision"].as_str(), rec["amount_cents"].as_i64()), (Some("admin_approved"), Some(before.total_cents)));
    // A decision is final.
    assert_eq!(post(&app, &format!("/admin/refunds/{refund_id}/approve"), Some(&admin), json!({})).await.status, StatusCode::CONFLICT);
    assert_eq!(post(&app, &format!("/admin/refunds/{refund_id}/deny"), Some(&admin), json!({})).await.status, StatusCode::CONFLICT);

    // Audit trail for the whole story.
    let log = get(&app, &format!("/admin/audit?target_type=refund&target_id={refund_id}"), Some(&admin)).await;
    let actions: Vec<_> = log.body.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap().to_owned()).collect();
    assert_eq!(actions, ["refund.requested", "refund.admin_approved", "refund.settled"]);

    // Denial path: nothing pays out and the eater cannot ask again.
    let order5 = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
    let pay5 = app.store.get_order(order5).await.unwrap().payment_ref.unwrap();
    let r = post(&app, &format!("/orders/{order5}/refund"), Some(&etok), json!({"reason": "unfulfilled"})).await;
    assert_eq!(r.body["status"], "pending_review");
    let id5 = r.body["refund_id"].as_str().unwrap().to_owned();
    let d = post(&app, &format!("/admin/refunds/{id5}/deny"), Some(&admin), json!({"notes": "pattern of abuse"})).await;
    assert_eq!((d.status, d.body["status"].as_str(), d.body["decision"].as_str()), (StatusCode::OK, Some("denied"), Some("admin_denied")));
    assert_eq!(app.pay.state_of(&pay5), Some(PaymentState::Captured));
    assert_eq!(
        post(&app, &format!("/orders/{order5}/refund"), Some(&etok), json!({"reason": "unfulfilled"})).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn refund_window_rolls_after_30_days() {
    let app = app();
    let (cook, ctok, dish, _admin) = verified_cook(&app).await;
    let e = email("eater");
    post(&app, "/auth/register", None, json!({"email": e, "password": PASSWORD, "role": "eater"})).await;
    let (_, etok) = login(&app, &e, PASSWORD).await;
    for _ in 0..3 {
        let order = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
        assert_eq!(post(&app, &format!("/orders/{order}/refund"), Some(&etok), json!({"reason": "other"})).await.status, StatusCode::OK);
    }
    let order = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
    // 31 days later the 30-day sessions are gone too, so log in again. The cook's certificate (365 days) is still valid
    // and the three old refunds are outside the trailing window.
    app.clock.set(Utc::now() + Duration::days(31));
    let (_, etok) = login(&app, &e, PASSWORD).await;
    let r = post(&app, &format!("/orders/{order}/refund"), Some(&etok), json!({"reason": "other"})).await;
    assert_eq!((r.status, r.body["status"].as_str()), (StatusCode::OK, Some("approved")), "{}", r.body);
}

#[tokio::test]
async fn only_parties_to_the_order_or_admins_can_refund_and_cooks_bypass_the_eater_limit() {
    let app = app();
    let (cook, ctok, dish, admin) = verified_cook(&app).await;
    let (_, etok) = signup(&app, "eater").await;
    let (_, stranger) = signup(&app, "eater").await;
    let order = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
    let uri = format!("/orders/{order}/refund");
    assert_eq!(post(&app, &uri, Some(&stranger), json!({"reason": "other"})).await.status, StatusCode::FORBIDDEN);
    assert_eq!(post(&app, &uri, None, json!({"reason": "other"})).await.status, StatusCode::UNAUTHORIZED);
    // The cook can refund their own customer; it is recorded as initiated by the cook and never throttled by the eater's history.
    let r = post(&app, &uri, Some(&ctok), json!({"reason": "cook_cancelled"})).await;
    assert_eq!((r.status, r.body["status"].as_str()), (StatusCode::OK, Some("approved")));
    let rec = app.store.get_refund_for_order(order).await.unwrap().unwrap();
    assert_eq!((rec.initiated_by, rec.decision), (cook, Some(twoeat_domain::RefundDecision::Auto)));
    // Admin-initiated refunds are recorded as such.
    let order2 = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
    let r = post(&app, &format!("/orders/{order2}/refund"), Some(&admin), json!({"reason": "unfulfilled"})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(app.store.get_refund_for_order(order2).await.unwrap().unwrap().decision, Some(twoeat_domain::RefundDecision::AdminApproved));
}

#[tokio::test]
async fn cook_with_more_than_five_refunds_is_flagged_not_suspended() {
    let app = app();
    let (cook, ctok, dish, admin) = verified_cook(&app).await;
    let flags = |app: &App| {
        let uri = format!("/admin/audit?target_type=cook&target_id={cook}&action=cook.flagged_for_review");
        let admin = admin.clone();
        let router = app.router.clone();
        async move {
            let req = Request::builder().uri(uri).header("authorization", format!("Bearer {admin}")).body(Body::empty()).unwrap();
            into_resp(router.oneshot(req).await.unwrap()).await.body.as_array().unwrap().len()
        }
    };
    for n in 1..=6 {
        let (_, etok) = signup(&app, "eater").await; // a different eater each time: no eater throttling involved
        let order = accepted_order(&app, cook, &ctok, dish, &etok, pickup()).await;
        let r = post(&app, &format!("/orders/{order}/refund"), Some(&etok), json!({"reason": "unfulfilled"})).await;
        assert_eq!(r.body["status"], "approved");
        assert_eq!(flags(&app).await, usize::from(n > 5), "after refund #{n}");
    }
    // Flag only: the cook is still verified and discoverable, and can keep listing dishes.
    assert_eq!(get(&app, "/cooks/nearby?lat=1.3521&lng=103.8198", None).await.body.as_array().unwrap().len(), 1);
    assert_eq!(post(&app, "/dishes", Some(&ctok), json!({"name": "Still selling", "price_cents": 500})).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn unaccepted_orders_cancel_without_counting_as_refunds() {
    let app = app();
    let (cook, _ctok, dish, _admin) = verified_cook(&app).await;
    let (_, etok) = signup(&app, "eater").await;
    for _ in 0..5 {
        let r = post(
            &app,
            "/orders",
            Some(&etok),
            json!({"cook_id": cook, "items": [{"dish_id": dish, "quantity": 1}], "fulfillment": pickup()}),
        )
        .await;
        let id: Uuid = r.body["id"].as_str().unwrap().parse().unwrap();
        let pay = app.store.get_order(id).await.unwrap().payment_ref.unwrap();
        let rf = post(&app, &format!("/orders/{id}/refund"), Some(&etok), json!({"reason": "other"})).await;
        assert_eq!((rf.status, rf.body["status"].as_str()), (StatusCode::OK, Some("approved")));
        assert_eq!(app.store.get_order(id).await.unwrap().status, twoeat_domain::OrderStatus::Cancelled);
        assert_eq!(app.pay.state_of(&pay), Some(PaymentState::Voided));
    }
}

// =================================================================== secrets

/// Captures everything `tracing` emits, at every level.
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogSink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn neither_tokens_nor_passwords_reach_the_logs() {
    let sink = LogSink::default();
    let sub = tracing_subscriber::fmt().with_max_level(tracing::Level::TRACE).with_writer(sink.clone()).with_ansi(false).finish();
    let _guard = tracing::subscriber::set_default(sub); // tokio::test is single-threaded, so this covers the whole flow

    let app = app();
    let secret_pw = "my-very-secret-password-123";
    let e = email("logs");
    post(&app, "/auth/register", None, json!({"email": e, "password": secret_pw, "role": "eater"})).await;
    let r = post(&app, "/auth/login", None, json!({"email": e, "password": secret_pw})).await;
    let token = r.body["token"].as_str().unwrap().to_owned();
    let hash = app.store.find_user_by_email(&e).await.unwrap().unwrap().password_hash.clone();
    let r2 = post_empty(&app, "/auth/refresh", &token).await;
    let token2 = r2.body["token"].as_str().unwrap().to_owned();
    post(&app, "/auth/login", None, json!({"email": e, "password": "wrong-but-secret-guess"})).await;
    get(&app, "/orders/00000000-0000-4000-8000-000000000001", Some(&token2)).await;
    post_empty(&app, "/auth/logout", &token2).await;

    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.is_empty(), "the capture works (request spans were logged)");
    for secret in [secret_pw, "wrong-but-secret-guess", &token, &token2, &hash] {
        assert!(!logs.contains(secret), "a secret leaked into the logs");
    }
    assert!(!logs.to_lowercase().contains("bearer "), "no Authorization header values in logs");
}
