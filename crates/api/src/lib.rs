//! Axum HTTP layer for 2Eat.

pub mod auth;
pub mod dto;
pub mod error;
pub mod openapi;
pub mod routes;
pub mod state;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
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

pub fn router(state: AppState) -> Router {
    use routes::{admin, cooks, dishes, orders};

    // Certificate upload is the only large body (5 MB doc + multipart overhead).
    let register =
        Router::new().route("/cooks/register", post(cooks::register)).layer(DefaultBodyLimit::max(cooks::MAX_DOCUMENT_BYTES + 64 * 1024));

    Router::new()
        .merge(register)
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_json))
        .route("/cooks/onboarding-terms", get(cooks::onboarding_terms))
        .route("/cooks/nearby", get(cooks::nearby))
        .route("/cooks/{id}/dishes", get(cooks::cook_dishes))
        .route("/dishes", post(dishes::create))
        .route("/orders", post(orders::create))
        .route("/orders/{id}", get(orders::get))
        .route("/orders/{id}/accept", post(orders::accept))
        .route("/orders/{id}/ready", post(orders::ready))
        .route("/orders/{id}/complete", post(orders::complete))
        .route("/orders/{id}/cancel", post(orders::cancel))
        .route("/orders/{id}/refund", post(orders::refund))
        .route("/admin/cooks/{id}/certification", get(admin::get_submission))
        .route("/admin/cooks/{id}/certification/approve", post(admin::approve))
        .route("/admin/cooks/{id}/certification/reject", post(admin::reject))
        .route("/admin/cooks/{id}/revoke", post(admin::revoke))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
