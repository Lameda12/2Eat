use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
    http::{Extensions, StatusCode},
};
use twoeat_domain::{CertificationSubmission, Cook, CurrentUser, FOOD_SAFETY_COURSE, HBFB_TERMS, HbfbAcknowledgements, LatLng, Role};
use twoeat_geo::coarse_distance_m;
use uuid::Uuid;

use crate::{
    dto::*,
    error::{ApiError, ErrorBody},
    state::AppState,
};

pub const MAX_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;

/// What cooks must accept at onboarding.
#[utoipa::path(
    get, path = "/cooks/onboarding-terms", tag = "cooks",
    responses((status = 200, body = OnboardingTerms))
)]
pub async fn onboarding_terms() -> Json<OnboardingTerms> {
    Json(OnboardingTerms {
        required_certification: FOOD_SAFETY_COURSE.to_owned(),
        conditions: HBFB_TERMS.iter().map(|s| (*s).to_owned()).collect(),
    })
}

fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError::BadRequest(e.to_string())
}

fn sniff(content_type: &str, bytes: &[u8]) -> bool {
    match content_type {
        "application/pdf" => bytes.starts_with(b"%PDF-"),
        "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        _ => false,
    }
}

/// Create the cook profile for the authenticated account (which must have been
/// registered with role `cook`; the cook id is the user id). The certificate
/// upload is required; the cook cannot list dishes or accept orders until an
/// admin approves it.
#[utoipa::path(
    post, path = "/cooks/register", tag = "cooks",
    security(("bearer_auth" = [])),
    request_body(content = RegisterCookForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, body = RegisterCookResponse),
        (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody, description = "Account is not a cook account"),
        (status = 409, body = ErrorBody, description = "Cook profile already exists"),
        (status = 400, body = ErrorBody, description = "Missing/invalid field or document"),
        (status = 422, body = ErrorBody, description = "HBFB conditions not accepted"),
    )
)]
pub async fn register(
    State(st): State<AppState>,
    ext: Extensions,
    mut mp: Multipart,
) -> Result<(StatusCode, Json<RegisterCookResponse>), ApiError> {
    let cook_id = match ext.get::<CurrentUser>() {
        Some(u) if u.role == Role::Cook => u.id,
        Some(_) => return Err(ApiError::Forbidden("only accounts registered with role 'cook' can create a cook profile")),
        // `dev-auth` only: Phase 1 allowed anonymous registration with a server-made id.
        #[cfg(feature = "dev-auth")]
        None => Uuid::new_v4(),
        #[cfg(not(feature = "dev-auth"))]
        None => return Err(ApiError::Unauthorized("authentication required")),
    };

    let (mut name, mut lat, mut lng, mut accepted) = (None::<String>, None::<f64>, None::<f64>, false);
    let mut doc: Option<(String, Vec<u8>)> = None;

    while let Some(field) = mp.next_field().await.map_err(bad)? {
        let field_name = field.name().unwrap_or_default().to_owned();
        match field_name.as_str() {
            "display_name" => name = Some(field.text().await.map_err(bad)?),
            "lat" => lat = Some(field.text().await.map_err(bad)?.trim().parse().map_err(|_| bad("lat must be a number"))?),
            "lng" => lng = Some(field.text().await.map_err(bad)?.trim().parse().map_err(|_| bad("lng must be a number"))?),
            "accept_hbfb_terms" => accepted = field.text().await.map_err(bad)?.trim().eq_ignore_ascii_case("true"),
            "cert_document" => {
                let ct = field.content_type().unwrap_or_default().to_owned();
                let bytes = field.bytes().await.map_err(bad)?;
                if bytes.len() > MAX_DOCUMENT_BYTES {
                    return Err(bad("cert_document exceeds 5 MB"));
                }
                if !sniff(&ct, &bytes) {
                    return Err(bad("cert_document must be a PDF, PNG or JPEG file"));
                }
                doc = Some((ct, bytes.to_vec()));
            }
            _ => {}
        }
    }

    let name = name.ok_or_else(|| bad("display_name is required"))?;
    let location = LatLng::new(lat.ok_or_else(|| bad("lat is required"))?, lng.ok_or_else(|| bad("lng is required"))?)?;
    let (content_type, bytes) =
        doc.ok_or_else(|| bad("cert_document is required: upload your Food Safety Course Level 1 (WSQ) certificate"))?;

    let now = st.clock.now();
    let acks = if accepted { HbfbAcknowledgements::all() } else { HbfbAcknowledgements::default() };
    let cook = Cook::register(cook_id, &name, location, &acks)?;

    let document_ref = st.documents.put(&content_type, bytes).await?;
    let submission = CertificationSubmission::new(cook.id, document_ref, content_type, now)?;
    st.store.register_cook(&cook, &submission, now).await?;

    Ok((
        StatusCode::CREATED,
        Json(RegisterCookResponse {
            cook_id: cook.id,
            is_verified: false,
            can_sell: false,
            certification_status: "pending".into(),
            message: format!("Thanks! You can list dishes and accept orders once your {FOOD_SAFETY_COURSE} certificate is verified."),
        }),
    ))
}

/// Discovery: verified cooks with a current certificate near a point, nearest first.
#[utoipa::path(
    get, path = "/cooks/nearby", tag = "cooks", params(NearbyParams),
    responses((status = 200, body = Vec<NearbyCookResponse>), (status = 400, body = ErrorBody))
)]
pub async fn nearby(State(st): State<AppState>, Query(p): Query<NearbyParams>) -> Result<Json<Vec<NearbyCookResponse>>, ApiError> {
    let center = LatLng::new(p.lat, p.lng)?;
    let radius_m = p.radius_m.unwrap_or(twoeat_geo::DEFAULT_RADIUS_M);
    // Validate once here (fixed error messages), the repo validates again defensively.
    twoeat_geo::NearbyQuery::new(center, Some(radius_m), p.limit)?;
    let hits = st.locations.find_nearby(center, radius_m / 1000.0, p.limit.unwrap_or(20), st.clock.now()).await?;
    Ok(Json(
        hits.into_iter()
            .map(|h| NearbyCookResponse { id: h.id, display_name: h.display_name, distance_m: coarse_distance_m(h.distance_m) })
            .collect(),
    ))
}

/// Active dishes of a cook. Empty unless the cook is currently allowed to sell.
#[utoipa::path(
    get, path = "/cooks/{id}/dishes", tag = "cooks",
    params(("id" = Uuid, Path, description = "Cook id")),
    responses((status = 200, body = Vec<DishResponse>), (status = 404, body = ErrorBody))
)]
pub async fn cook_dishes(State(st): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Vec<DishResponse>>, ApiError> {
    let cook = st.store.get_cook(id).await?;
    if !cook.can_sell(st.clock.now()) {
        return Ok(Json(vec![]));
    }
    let dishes = st.store.list_dishes(id).await?;
    Ok(Json(dishes.into_iter().filter(|d| d.is_active).map(Into::into).collect()))
}
