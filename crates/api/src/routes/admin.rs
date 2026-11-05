use axum::{
    Json,
    extract::{Path, State},
};
use twoeat_domain::{CertDecision, Cook};
use uuid::Uuid;

use crate::{
    auth::Admin,
    dto::*,
    error::{ApiError, ErrorBody},
    state::AppState,
};

fn status(c: &Cook, now: chrono::DateTime<chrono::Utc>) -> CookStatusResponse {
    CookStatusResponse {
        cook_id: c.id,
        display_name: c.display_name.clone(),
        is_verified: c.is_verified,
        food_safety_cert_expires_at: c.food_safety_cert_expires_at,
        can_sell: c.can_sell(now),
    }
}

/// The most recent certificate upload for a cook.
#[utoipa::path(
    get, path = "/admin/cooks/{id}/certification", tag = "admin",
    security(("admin_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = SubmissionResponse), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn get_submission(_: Admin, State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<SubmissionResponse>, ApiError> {
    let s = st.store.latest_submission(id).await?.ok_or(twoeat_persistence::StoreError::NotFound("certification submission"))?;
    Ok(Json(SubmissionResponse {
        cook_id: s.cook_id,
        document_ref: s.document_ref,
        content_type: s.content_type,
        status: s.status.as_str().to_owned(),
        submitted_at: s.submitted_at,
        reviewed_at: s.reviewed_at,
    }))
}

/// Mark the uploaded Food Safety Course Level 1 (WSQ) certificate as verified.
#[utoipa::path(
    post, path = "/admin/cooks/{id}/certification/approve", tag = "admin",
    security(("admin_token" = [])), params(("id" = Uuid, Path)), request_body = ApproveCertificationRequest,
    responses((status = 200, body = CookStatusResponse), (status = 400, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn approve(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ApproveCertificationRequest>,
) -> Result<Json<CookStatusResponse>, ApiError> {
    let now = st.clock.now();
    let cook = st.store.review_certification(id, CertDecision::Approve { expires_at: req.expires_at }, now).await?;
    Ok(Json(status(&cook, now)))
}

/// Reject the uploaded certificate; the cook stays unable to sell.
#[utoipa::path(
    post, path = "/admin/cooks/{id}/certification/reject", tag = "admin",
    security(("admin_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = CookStatusResponse), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn reject(_: Admin, State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<CookStatusResponse>, ApiError> {
    let now = st.clock.now();
    let cook = st.store.review_certification(id, CertDecision::Reject, now).await?;
    Ok(Json(status(&cook, now)))
}

/// Pull a cook off the platform. Orders that are not yet accepted can no longer be accepted.
#[utoipa::path(
    post, path = "/admin/cooks/{id}/revoke", tag = "admin",
    security(("admin_token" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = CookStatusResponse), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn revoke(_: Admin, State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<CookStatusResponse>, ApiError> {
    let cook = st.store.revoke_verification(id).await?;
    Ok(Json(status(&cook, st.clock.now())))
}
