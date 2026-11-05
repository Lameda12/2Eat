use axum::{Extension, Json, extract::State, http::StatusCode};
use twoeat_domain::{CurrentUser, Dish, NewDish, Role};

use crate::{
    dto::{CreateDishRequest, DishResponse},
    error::{ApiError, ErrorBody},
    state::AppState,
};

/// Create a dish as the authenticated cook. The domain layer rejects uncertified
/// cooks, catering and a dish *declared* as raw seafood (422). A dish whose
/// name/description merely matches a raw-seafood keyword is created **inactive**
/// with `review_status: pending_review` until an admin approves it.
#[utoipa::path(
    post, path = "/dishes", tag = "dishes",
    security(("bearer_auth" = [])),
    request_body = CreateDishRequest,
    responses(
        (status = 201, body = DishResponse, description = "Created (possibly pending_review)"),
        (status = 400, body = ErrorBody),
        (status = 403, body = ErrorBody, description = "Caller is not a cook"),
        (status = 404, body = ErrorBody, description = "Cook profile not found"),
        (status = 422, body = ErrorBody, description = "Regulatory violation (certification, catering, declared raw seafood)"),
    )
)]
pub async fn create(
    State(st): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<CreateDishRequest>,
) -> Result<(StatusCode, Json<DishResponse>), ApiError> {
    if user.role != Role::Cook {
        return Err(ApiError::Forbidden("only cooks can create dishes"));
    }
    let cook = st.store.get_cook(user.id).await?;
    let dish = Dish::create(
        &cook,
        NewDish {
            name: req.name,
            description: req.description,
            price_cents: req.price_cents,
            is_catering: req.is_catering,
            is_raw_seafood: req.is_raw_seafood,
        },
        st.clock.now(),
    )?;
    st.store.insert_dish(&dish).await?;
    Ok((StatusCode::CREATED, Json(dish.into())))
}
