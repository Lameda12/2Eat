use axum::{
    Extension, Json,
    extract::State,
    http::{Extensions, HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use twoeat_domain::{CurrentUser, Role};
use twoeat_persistence::{NewSession, NewUser, StoreError};
use uuid::Uuid;

use crate::{
    auth::{
        SessionId,
        password::{MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS, hash_password, verify_password},
        token::BearerToken,
    },
    dto::{LoginRequest, MessageResponse, RegisterRequest, TokenResponse, UserSummary},
    error::{ApiError, ErrorBody},
    state::AppState,
};

fn normalize_email(raw: &str) -> String {
    raw.trim().to_lowercase()
}

fn plausible_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else { return false };
    e.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn too_many(retry: std::time::Duration) -> ApiError {
    ApiError::RateLimited { retry_after_secs: retry.as_secs() + 1 }
}

async fn issue(st: &AppState, user_id: Uuid, role: Role, now: DateTime<Utc>) -> Result<(BearerToken, NewSession, TokenResponse), ApiError> {
    let token = BearerToken::generate();
    let session =
        NewSession { id: Uuid::new_v4(), user_id, token_hash: token.digest(), expires_at: now + st.config.session_ttl, created_at: now };
    let resp = TokenResponse { token: token.expose().to_owned(), expires_at: session.expires_at, user: UserSummary { id: user_id, role } };
    Ok((token, session, resp))
}

/// Create an account. Responds identically whether or not the email was already
/// registered, so the endpoint cannot be used to discover accounts.
#[utoipa::path(
    post, path = "/auth/register", tag = "auth", request_body = RegisterRequest,
    responses(
        (status = 202, body = MessageResponse, description = "Accepted (same response for new and existing emails)"),
        (status = 400, body = ErrorBody),
        (status = 403, body = ErrorBody, description = "Admin accounts cannot be self-registered"),
        (status = 429, body = ErrorBody),
    )
)]
pub async fn register(
    State(st): State<AppState>,
    headers: HeaderMap,
    ext: Extensions,
    Json(req): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), ApiError> {
    let email = normalize_email(&req.email);
    if !plausible_email(&email) {
        return Err(ApiError::BadRequest("a valid email is required".into()));
    }
    if req.password.chars().count() < MIN_PASSWORD_CHARS || req.password.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::BadRequest(format!("password must be {MIN_PASSWORD_CHARS}-{MAX_PASSWORD_BYTES} bytes long")));
    }
    if req.role == Role::Admin {
        return Err(ApiError::Forbidden("admin accounts cannot be self-registered"));
    }

    let ip = st.limits.client_ip(&headers, &ext);
    st.limits.check_register(&ip, &email).map_err(too_many)?;

    // Always pay the hashing cost, whether or not the email exists.
    let password_hash = hash_password(&req.password).await?;
    let now = st.clock.now();
    let created =
        st.auth.create_user(NewUser { id: Uuid::new_v4(), email: email.clone(), password_hash, role: req.role, created_at: now }).await;
    match created {
        Ok(()) => st.mailer.send_verification(&email).await,
        Err(StoreError::Conflict(_)) => {} // existing account: say nothing
        Err(e) => return Err(e.into()),
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(MessageResponse { message: "If this email can be registered, the account has been created. Log in to continue.".into() }),
    ))
}

/// Exchange email + password for a bearer token (valid 30 days).
/// A wrong password and an unknown email are indistinguishable.
#[utoipa::path(
    post, path = "/auth/login", tag = "auth", request_body = LoginRequest,
    responses(
        (status = 200, body = TokenResponse),
        (status = 401, body = ErrorBody, description = "Invalid email or password"),
        (status = 429, body = ErrorBody),
    )
)]
pub async fn login(
    State(st): State<AppState>,
    headers: HeaderMap,
    ext: Extensions,
    Json(req): Json<LoginRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let email = normalize_email(&req.email);
    let ip = st.limits.client_ip(&headers, &ext);
    st.limits.check_login(&ip, &email).map_err(too_many)?;

    let user = st.auth.find_user_by_email(&email).await?;
    let oversized = req.password.len() > MAX_PASSWORD_BYTES;
    let ok = !oversized && verify_password(&req.password, user.as_ref().map(|u| u.password_hash.as_str())).await;
    let Some(user) = user.filter(|_| ok) else {
        return Err(ApiError::InvalidCredentials);
    };

    let now = st.clock.now();
    let (_token, session, resp) = issue(&st, user.id, user.role, now).await?;
    st.auth.create_session(session).await?;
    Ok(Json(resp))
}

/// Revoke the token used for this request.
#[utoipa::path(
    post, path = "/auth/logout", tag = "auth", security(("bearer_auth" = [])),
    responses((status = 204, description = "Token revoked"), (status = 401, body = ErrorBody))
)]
pub async fn logout(State(st): State<AppState>, session: Option<Extension<SessionId>>) -> Result<StatusCode, ApiError> {
    if let Some(Extension(SessionId(id))) = session {
        st.auth.revoke_session(id, st.clock.now()).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Rotate the token: the presented one is revoked and a new one issued, atomically.
#[utoipa::path(
    post, path = "/auth/refresh", tag = "auth", security(("bearer_auth" = [])),
    responses((status = 200, body = TokenResponse), (status = 401, body = ErrorBody))
)]
pub async fn refresh(
    State(st): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    session: Option<Extension<SessionId>>,
) -> Result<Json<TokenResponse>, ApiError> {
    let Some(Extension(SessionId(old))) = session else {
        return Err(ApiError::Unauthorized("refresh requires a bearer token"));
    };
    let now = st.clock.now();
    let (_token, new_session, resp) = issue(&st, user.id, user.role, now).await?;
    match st.auth.rotate_session(old, new_session, now).await {
        Ok(()) => Ok(Json(resp)),
        // Lost a race with logout/another refresh: the presented token is no longer valid.
        Err(StoreError::NotFound(_)) => Err(ApiError::Unauthorized("token is no longer valid")),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_checks() {
        assert!(plausible_email("a@b.co"));
        for bad in ["", "a", "a@b", "@b.co", "a@@b.co", "a b@c.co", "a@.co", "a@b."] {
            assert!(!plausible_email(bad), "{bad}");
        }
        assert_eq!(normalize_email("  Foo@Bar.COM "), "foo@bar.com");
    }
}
