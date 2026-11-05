//! Authentication: opaque bearer tokens backed by the `sessions` table.
//!
//! The middleware turns `Authorization: Bearer <token>` into a
//! [`CurrentUser`] request extension. Handlers on protected routes take
//! `Extension<CurrentUser>`; the [`require_auth`] / [`require_admin`] layers
//! guarantee it is present.

pub mod limits;
pub mod password;
pub mod token;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use twoeat_domain::{CurrentUser, Role};
use uuid::Uuid;

use crate::{error::ApiError, state::AppState};

/// Id of the session the request authenticated with (for logout/refresh).
#[derive(Debug, Clone, Copy)]
pub struct SessionId(pub Uuid);

/// Refuse to run a release build that has `x-user-id` support compiled in.
pub fn check_dev_auth_allowed(dev_auth_compiled: bool, release_build: bool) -> Result<(), StartupError> {
    if dev_auth_compiled && release_build {
        return Err(StartupError::DevAuthInRelease);
    }
    Ok(())
}

#[derive(Debug)]
pub enum StartupError {
    DevAuthInRelease,
}

impl std::error::Error for StartupError {}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DevAuthInRelease => f.write_str(
                "the `dev-auth` feature (x-user-id header identity) is compiled into a release build; \
                 rebuild without it. Refusing to start.",
            ),
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

#[cfg(feature = "dev-auth")]
async fn dev_identity(st: &AppState, headers: &HeaderMap) -> Option<CurrentUser> {
    let id: Uuid = headers.get("x-user-id")?.to_str().ok()?.parse().ok()?;
    let role = match headers.get("x-user-role").and_then(|v| v.to_str().ok()).and_then(Role::parse) {
        Some(r) => r,
        None if st.store.get_cook(id).await.is_ok() => Role::Cook,
        None => Role::Eater,
    };
    Some(CurrentUser { id, role })
}

/// Resolve the caller, if any. An unknown, expired or revoked token is simply "anonymous"
/// here; protected routes then answer 401.
pub async fn authenticate(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let now = st.clock.now();
    let mut resolved: Option<(CurrentUser, Option<SessionId>)> = None;

    if let Some(tok) = bearer(req.headers()) {
        match st.auth.find_session(&token::digest_of(tok)).await {
            Ok(Some(s)) if s.is_active(now) => {
                resolved = Some((CurrentUser { id: s.user_id, role: s.role }, Some(SessionId(s.session_id))));
            }
            Ok(_) => {}
            Err(e) => return ApiError::from(e).into_response(),
        }
    }

    #[cfg(feature = "dev-auth")]
    if resolved.is_none()
        && let Some(u) = dev_identity(&st, req.headers()).await
    {
        resolved = Some((u, None));
    }

    if let Some((user, session)) = resolved {
        req.extensions_mut().insert(user);
        if let Some(s) = session {
            req.extensions_mut().insert(s);
        }
    }
    next.run(req).await
}

fn unauthorized() -> Response {
    let mut res = ApiError::Unauthorized("authentication required").into_response();
    res.headers_mut().insert(header::WWW_AUTHENTICATE, "Bearer".parse().expect("static header"));
    res
}

/// 401 unless [`authenticate`] found a valid caller.
pub async fn require_auth(req: Request, next: Next) -> Response {
    if req.extensions().get::<CurrentUser>().is_none() {
        return unauthorized();
    }
    next.run(req).await
}

/// 401 if anonymous, 403 unless the caller is an admin.
pub async fn require_admin(req: Request, next: Next) -> Response {
    match req.extensions().get::<CurrentUser>() {
        None => unauthorized(),
        Some(u) if u.role != Role::Admin => ApiError::Forbidden("admin role required").into_response(),
        Some(_) => next.run(req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_auth_in_release_is_refused() {
        assert!(check_dev_auth_allowed(true, true).is_err());
        assert!(check_dev_auth_allowed(true, false).is_ok(), "debug builds may use dev-auth");
        assert!(check_dev_auth_allowed(false, true).is_ok());
        assert!(check_dev_auth_allowed(false, false).is_ok());
        assert!(check_dev_auth_allowed(true, true).unwrap_err().to_string().contains("Refusing to start"));
    }

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }
}
