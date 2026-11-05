//! Axum HTTP layer for 2Eat.

pub mod auth;
pub mod dto;
pub mod email;
pub mod error;
pub mod openapi;
pub mod routes;
pub mod state;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

pub use state::{AppConfig, AppState};

async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi::ApiDoc::openapi())
}

async fn health() -> &'static str {
    "ok"
}

/// Build the router, refusing (with an error) to run a release build that has the
/// `dev-auth` header identity compiled in.
pub fn try_router(state: AppState) -> Result<Router, auth::StartupError> {
    auth::check_dev_auth_allowed(cfg!(feature = "dev-auth"), !cfg!(debug_assertions))?;
    if cfg!(feature = "dev-auth") {
        tracing::warn!("dev-auth is ON: x-user-id / x-user-role headers are trusted as identity. Never deploy this build.");
    }
    Ok(build_router(state))
}

/// Like [`try_router`], panicking on a refused configuration.
pub fn router(state: AppState) -> Router {
    try_router(state).unwrap_or_else(|e| panic!("{e}"))
}

fn build_router(state: AppState) -> Router {
    use routes::{admin, auth as auth_routes, cooks, dishes, orders};

    // Certificate upload is the only large body (5 MB doc + multipart overhead).
    let register_cook =
        Router::new().route("/cooks/register", post(cooks::register)).layer(DefaultBodyLimit::max(cooks::MAX_DOCUMENT_BYTES + 64 * 1024));

    let public = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_json))
        .route("/auth/register", post(auth_routes::register))
        .route("/auth/login", post(auth_routes::login))
        .route("/cooks/onboarding-terms", get(cooks::onboarding_terms))
        .route("/cooks/nearby", get(cooks::nearby))
        .route("/cooks/{id}/dishes", get(cooks::cook_dishes))
        // Needs a cook account; the handler answers 401/403 itself (and allows the dev-auth anonymous path).
        .merge(register_cook);

    let protected = Router::new()
        .route("/auth/logout", post(auth_routes::logout))
        .route("/auth/refresh", post(auth_routes::refresh))
        .route("/dishes", post(dishes::create))
        .route("/orders", post(orders::create))
        .route("/orders/{id}", get(orders::get))
        .route("/orders/{id}/accept", post(orders::accept))
        .route("/orders/{id}/ready", post(orders::ready))
        .route("/orders/{id}/complete", post(orders::complete))
        .route("/orders/{id}/cancel", post(orders::cancel))
        .route("/orders/{id}/refund", post(orders::refund))
        .route_layer(middleware::from_fn(auth::require_auth));

    let admin = Router::new()
        .route("/admin/reviews", get(admin::list_reviews))
        .route("/admin/reviews/{id}/approve", post(admin::approve_review))
        .route("/admin/reviews/{id}/reject", post(admin::reject_review))
        .route("/admin/cooks/{id}/certification", get(admin::get_submission))
        .route("/admin/cooks/{id}/certification/approve", post(admin::approve_cook_certification))
        .route("/admin/cooks/{id}/certification/reject", post(admin::reject_cook_certification))
        .route("/admin/cooks/{id}/revoke", post(admin::revoke))
        .route("/admin/dishes/flagged", get(admin::flagged_dishes))
        .route("/admin/dishes/{id}/approve", post(admin::approve_dish))
        .route("/admin/dishes/{id}/reject", post(admin::reject_dish))
        .route("/admin/refunds", get(admin::list_refunds))
        .route("/admin/refunds/{id}/approve", post(admin::approve_refund))
        .route("/admin/refunds/{id}/deny", post(admin::deny_refund))
        .route("/admin/audit", get(admin::audit))
        .route_layer(middleware::from_fn(auth::require_admin));

    Router::new()
        .merge(public)
        .merge(protected)
        .merge(admin)
        // Outermost: resolve the caller before any route-level guard runs.
        .layer(middleware::from_fn_with_state(state.clone(), auth::authenticate))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
