use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use twoeat_domain::{CertDecision, CertificationStatus, Cook, CurrentUser, RefundStatus};
use twoeat_persistence::{AuditFilter, StoreError};
use uuid::Uuid;

use super::orders::pay_out_refund;
use crate::{
    dto::*,
    error::{ApiError, ErrorBody},
    state::AppState,
};

// All routes here sit behind the `require_admin` layer (role = admin).

fn cook_status(c: &Cook, now: DateTime<Utc>) -> CookStatusResponse {
    CookStatusResponse {
        cook_id: c.id,
        display_name: c.display_name.clone(),
        is_verified: c.is_verified,
        food_safety_cert_expires_at: c.food_safety_cert_expires_at,
        can_sell: c.can_sell(now),
    }
}

fn parse_status(s: &str) -> Result<CertificationStatus, ApiError> {
    CertificationStatus::parse(s).ok_or_else(|| ApiError::BadRequest("status must be pending, approved or rejected".into()))
}

fn page_args(limit: Option<i64>, offset: Option<i64>) -> Result<(i64, i64), ApiError> {
    PageParams { limit, offset }.resolve()
}

// ------------------------------------------------------- certificate reviews

/// Certificate review queue (oldest first).
#[utoipa::path(
    get, path = "/admin/reviews", tag = "admin", security(("bearer_auth" = [])), params(ReviewListParams),
    responses((status = 200, body = ReviewPage), (status = 400, body = ErrorBody), (status = 401, body = ErrorBody), (status = 403, body = ErrorBody))
)]
pub async fn list_reviews(State(st): State<AppState>, Query(p): Query<ReviewListParams>) -> Result<Json<ReviewPage>, ApiError> {
    let status = p.status.as_deref().map(parse_status).transpose()?;
    let (limit, offset) = page_args(p.limit, p.offset)?;
    let page = st.store.list_reviews(status, limit, offset).await?;
    Ok(Json(ReviewPage { items: page.items.into_iter().map(Into::into).collect(), total: page.total, limit, offset }))
}

/// Approve a pending review: the cook becomes verified until `expires_at`.
#[utoipa::path(
    post, path = "/admin/reviews/{id}/approve", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = ApproveCertificationRequest,
    responses((status = 200, body = ReviewItem), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody, description = "Already decided"))
)]
pub async fn approve_review(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
    Json(req): Json<ApproveCertificationRequest>,
) -> Result<Json<ReviewItem>, ApiError> {
    let (review, _cook) =
        st.store.decide_review(id, Some(admin.id), CertDecision::Approve { expires_at: req.expires_at }, req.notes, st.clock.now()).await?;
    Ok(Json(review.into()))
}

/// Reject a pending review with a structured reason. The cook cannot sell.
#[utoipa::path(
    post, path = "/admin/reviews/{id}/reject", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = RejectCertificationRequest,
    responses((status = 200, body = ReviewItem), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody, description = "Already decided"))
)]
pub async fn reject_review(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
    Json(req): Json<RejectCertificationRequest>,
) -> Result<Json<ReviewItem>, ApiError> {
    let (review, _cook) =
        st.store.decide_review(id, Some(admin.id), CertDecision::Reject { reason: req.reason }, req.notes, st.clock.now()).await?;
    Ok(Json(review.into()))
}

async fn pending_review_for_cook(st: &AppState, cook_id: Uuid) -> Result<Uuid, ApiError> {
    match st.store.latest_submission(cook_id).await? {
        Some(r) if r.status == CertificationStatus::Pending => Ok(r.id),
        Some(_) => Err(StoreError::Conflict("certificate review already decided").into()),
        None => Err(StoreError::NotFound("certificate review").into()),
    }
}

/// Latest certificate review of a cook.
#[utoipa::path(
    get, path = "/admin/cooks/{id}/certification", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = ReviewItem), (status = 404, body = ErrorBody))
)]
pub async fn get_submission(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<ReviewItem>, ApiError> {
    let s = st.store.latest_submission(id).await?.ok_or(StoreError::NotFound("certificate review"))?;
    Ok(Json(s.into()))
}

/// Cook-keyed shortcut for `POST /admin/reviews/{id}/approve` on the cook's pending review.
#[utoipa::path(
    post, path = "/admin/cooks/{id}/certification/approve", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = ApproveCertificationRequest,
    responses((status = 200, body = CookStatusResponse), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn approve_cook_certification(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(cook_id): Path<Uuid>,
    Json(req): Json<ApproveCertificationRequest>,
) -> Result<Json<CookStatusResponse>, ApiError> {
    let review_id = pending_review_for_cook(&st, cook_id).await?;
    let now = st.clock.now();
    let (_r, cook) =
        st.store.decide_review(review_id, Some(admin.id), CertDecision::Approve { expires_at: req.expires_at }, req.notes, now).await?;
    Ok(Json(cook_status(&cook, now)))
}

/// Cook-keyed shortcut for `POST /admin/reviews/{id}/reject`.
#[utoipa::path(
    post, path = "/admin/cooks/{id}/certification/reject", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = RejectCertificationRequest,
    responses((status = 200, body = CookStatusResponse), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn reject_cook_certification(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(cook_id): Path<Uuid>,
    Json(req): Json<RejectCertificationRequest>,
) -> Result<Json<CookStatusResponse>, ApiError> {
    let review_id = pending_review_for_cook(&st, cook_id).await?;
    let now = st.clock.now();
    let (_r, cook) = st.store.decide_review(review_id, Some(admin.id), CertDecision::Reject { reason: req.reason }, req.notes, now).await?;
    Ok(Json(cook_status(&cook, now)))
}

/// Pull a cook off the platform. Orders that are not yet accepted can no longer be accepted.
#[utoipa::path(
    post, path = "/admin/cooks/{id}/revoke", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = CookStatusResponse), (status = 404, body = ErrorBody))
)]
pub async fn revoke(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
) -> Result<Json<CookStatusResponse>, ApiError> {
    let now = st.clock.now();
    let cook = st.store.revoke_cook(Some(admin.id), id, now).await?;
    Ok(Json(cook_status(&cook, now)))
}

// ------------------------------------------------------------------- dishes

/// Dishes flagged by the raw-seafood keyword scan, awaiting a decision.
#[utoipa::path(
    get, path = "/admin/dishes/flagged", tag = "admin", security(("bearer_auth" = [])), params(PageParams),
    responses((status = 200, body = DishPage))
)]
pub async fn flagged_dishes(State(st): State<AppState>, Query(p): Query<PageParams>) -> Result<Json<DishPage>, ApiError> {
    let (limit, offset) = p.resolve()?;
    let page = st.store.list_flagged_dishes(limit, offset).await?;
    Ok(Json(DishPage { items: page.items.into_iter().map(Into::into).collect(), total: page.total, limit, offset }))
}

/// Clear a flagged dish: it becomes listable.
#[utoipa::path(
    post, path = "/admin/dishes/{id}/approve", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = DishResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn approve_dish(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
) -> Result<Json<DishResponse>, ApiError> {
    Ok(Json(st.store.decide_dish_review(id, admin.id, true, st.clock.now()).await?.into()))
}

/// Uphold the flag: the dish stays unlisted.
#[utoipa::path(
    post, path = "/admin/dishes/{id}/reject", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = DishResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn reject_dish(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
) -> Result<Json<DishResponse>, ApiError> {
    Ok(Json(st.store.decide_dish_review(id, admin.id, false, st.clock.now()).await?.into()))
}

// ------------------------------------------------------------------ refunds

/// Refund requests, `pending_review` by default.
#[utoipa::path(
    get, path = "/admin/refunds", tag = "admin", security(("bearer_auth" = [])), params(RefundListParams),
    responses((status = 200, body = RefundPage), (status = 400, body = ErrorBody))
)]
pub async fn list_refunds(State(st): State<AppState>, Query(p): Query<RefundListParams>) -> Result<Json<RefundPage>, ApiError> {
    let status = match p.status.as_deref() {
        None => Some(RefundStatus::PendingReview),
        Some("all") => None,
        Some(s) => Some(
            RefundStatus::parse(s).ok_or_else(|| ApiError::BadRequest("status must be pending_review, approved, denied or all".into()))?,
        ),
    };
    let (limit, offset) = page_args(p.limit, p.offset)?;
    let page = st.store.list_refunds(status, limit, offset).await?;
    Ok(Json(RefundPage { items: page.items.into_iter().map(Into::into).collect(), total: page.total, limit, offset }))
}

/// Approve a pending refund and pay it out in full: items, delivery fee **and**
/// platform fee.
#[utoipa::path(
    post, path = "/admin/refunds/{id}/approve", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = NotesRequest,
    responses((status = 200, body = RefundAccepted), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody, description = "Not pending, or no longer refundable"))
)]
pub async fn approve_refund(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
    body: Option<Json<NotesRequest>>,
) -> Result<Json<RefundAccepted>, ApiError> {
    let notes = body.and_then(|Json(b)| b.notes);
    let rec = st.store.decide_refund(id, admin.id, true, notes, st.clock.now(), st.refund_policy.as_ref()).await?;
    pay_out_refund(&st, &rec).await?;
    Ok(Json(RefundAccepted { status: "approved".into(), refund_id: rec.id }))
}

/// Deny a pending refund. Nothing is paid; the eater cannot re-request for this order.
#[utoipa::path(
    post, path = "/admin/refunds/{id}/deny", tag = "admin", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = NotesRequest,
    responses((status = 200, body = RefundItem), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn deny_refund(
    State(st): State<AppState>,
    Extension(admin): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
    body: Option<Json<NotesRequest>>,
) -> Result<Json<RefundItem>, ApiError> {
    let notes = body.and_then(|Json(b)| b.notes);
    let rec = st.store.decide_refund(id, admin.id, false, notes, st.clock.now(), st.refund_policy.as_ref()).await?;
    Ok(Json(rec.into()))
}

// -------------------------------------------------------------------- audit

/// Read the append-only audit log (oldest first).
#[utoipa::path(
    get, path = "/admin/audit", tag = "admin", security(("bearer_auth" = [])), params(AuditParams),
    responses((status = 200, body = Vec<AuditItem>))
)]
pub async fn audit(State(st): State<AppState>, Query(p): Query<AuditParams>) -> Result<(StatusCode, Json<Vec<AuditItem>>), ApiError> {
    let limit = p.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::BadRequest("limit must be 1..=200".into()));
    }
    let rows = st.store.audit_entries(AuditFilter { target_type: p.target_type, target_id: p.target_id, action: p.action }, limit).await?;
    Ok((StatusCode::OK, Json(rows.into_iter().map(Into::into).collect())))
}
