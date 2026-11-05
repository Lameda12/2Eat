use axum::{Json, extract::State, http::StatusCode};
use twoeat_domain::{Dish, NewDish};

use crate::{
    auth::Actor,
    dto::{CreateDishRequest, DishResponse},
    error::{ApiError, ErrorBody},
    state::AppState,
};

/// Create a dish as the authenticated cook (`x-user-id` = cook id). The domain
/// layer rejects uncertified cooks, catering and ready-to-eat raw seafood.
#[utoipa::path(
    post, path = "/dishes", tag = "dishes",
    security(("user_id" = [])),
    request_body = CreateDishRequest,
    responses(
        (status = 201, body = DishResponse),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody, description = "Caller is not a registered cook"),
        (status = 422, body = ErrorBody, description = "Regulatory violation (certification, catering, raw seafood)"),
    )
)]
pub async fn create(
    State(st): State<AppState>,
    Actor(cook_id): Actor,
    Json(req): Json<CreateDishRequest>,
) -> Result<(StatusCode, Json<DishResponse>), ApiError> {
    let cook = st.store.get_cook(cook_id).await?;
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
