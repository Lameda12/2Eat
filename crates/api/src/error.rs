use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use twoeat_domain::DomainError;
use twoeat_payments::PaymentError;
use twoeat_persistence::StoreError;
use utoipa::ToSchema;

#[derive(Debug)]
pub enum ApiError {
    Domain(DomainError),
    Store(StoreError),
    Payment(PaymentError),
    BadRequest(String),
    Unauthorized(&'static str),
    Forbidden(&'static str),
    InvalidCredentials,
    RateLimited { retry_after_secs: u64 },
    Internal(String),
}

impl From<DomainError> for ApiError {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}
impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<PaymentError> for ApiError {
    fn from(e: PaymentError) -> Self {
        Self::Payment(e)
    }
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Stable machine-readable code, e.g. `certification_expired`, `catering_prohibited`.
    pub code: String,
    /// `regulatory_violation`, `validation`, `conflict`, `not_found`, `payment`, `auth` or `internal`.
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        use DomainError as D;
        let (status, code, kind, message) = match self {
            ApiError::Domain(d) | ApiError::Store(StoreError::Domain(d)) => match d {
                D::RegulatoryViolation(v) => (StatusCode::UNPROCESSABLE_ENTITY, v.code().to_owned(), "regulatory_violation", v.to_string()),
                D::Validation(m) => (StatusCode::BAD_REQUEST, "validation_error".into(), "validation", m),
                m @ D::InvalidTransition { .. } => (StatusCode::CONFLICT, "invalid_transition".into(), "conflict", m.to_string()),
            },
            ApiError::Store(StoreError::Conflict(m)) => (StatusCode::CONFLICT, "conflict".into(), "conflict", m.into()),
            ApiError::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, "invalid_credentials".into(), "auth", "invalid email or password".into())
            }
            ApiError::RateLimited { retry_after_secs } => {
                let body = ErrorBody {
                    error: ErrorDetail {
                        code: "rate_limited".into(),
                        kind: "rate_limit".into(),
                        message: "too many attempts, slow down".into(),
                    },
                };
                let mut res = (StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
                res.headers_mut().insert(axum::http::header::RETRY_AFTER, retry_after_secs.max(1).into());
                return res;
            }
            ApiError::Store(StoreError::NotFound(what)) => {
                (StatusCode::NOT_FOUND, "not_found".into(), "not_found", format!("{what} not found"))
            }
            ApiError::Store(StoreError::Db(m)) => {
                tracing::error!(error = %m, "database error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error".into(), "internal", "internal error".into())
            }
            ApiError::Payment(PaymentError::Declined(m)) => (StatusCode::PAYMENT_REQUIRED, "payment_declined".into(), "payment", m),
            ApiError::Payment(e) => {
                tracing::error!(error = %e, "payment error");
                (StatusCode::BAD_GATEWAY, "payment_error".into(), "payment", "payment provider error".into())
            }
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request".into(), "validation", m),
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized".into(), "auth", m.into()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden".into(), "auth", m.into()),
            ApiError::Internal(m) => {
                tracing::error!(error = %m, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error".into(), "internal", "internal error".into())
            }
        };
        (status, Json(ErrorBody { error: ErrorDetail { code, kind: kind.into(), message } })).into_response()
    }
}
