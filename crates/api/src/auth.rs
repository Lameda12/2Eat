use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, request::Parts},
};
use uuid::Uuid;

use crate::{error::ApiError, state::AppState};

/// The caller, taken from the `x-user-id` header.
///
/// MVP stand-in for real authentication (JWT/OIDC): the header is trusted.
/// A cook's user id is their cook id. Replace this extractor, nothing else
/// needs to change.
pub struct Actor(pub Uuid);

pub fn actor_id(headers: &HeaderMap) -> Result<Uuid, ApiError> {
    let v = headers.get("x-user-id").ok_or(ApiError::Unauthorized("missing x-user-id header"))?;
    v.to_str().ok().and_then(|s| Uuid::parse_str(s).ok()).ok_or(ApiError::Unauthorized("x-user-id must be a UUID"))
}

impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        actor_id(&parts.headers).map(Actor)
    }
}

pub fn is_admin(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(given) = headers.get("x-admin-token").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let want = state.config.admin_token.as_bytes();
    let given = given.as_bytes();
    // Constant-time comparison.
    want.len() == given.len() && want.iter().zip(given).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Requires a valid `x-admin-token` header.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if is_admin(&parts.headers, state) { Ok(Admin) } else { Err(ApiError::Forbidden("admin token required")) }
    }
}
