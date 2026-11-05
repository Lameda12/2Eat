use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use twoeat_domain::{DomainError, NewOrder, Order, OrderStatus, RequestedLine};
use twoeat_payments::AuthorizeRequest;
use uuid::Uuid;

use crate::{
    auth::{Actor, actor_id, is_admin},
    dto::*,
    error::{ApiError, ErrorBody},
    state::AppState,
};

type OrderReply = Result<Json<OrderResponse>, ApiError>;

fn payment_ref(o: &Order) -> Result<&str, ApiError> {
    o.payment_ref.as_deref().ok_or_else(|| ApiError::Internal(format!("order {} has no payment reference", o.id)))
}

async fn owned_by_cook(st: &AppState, id: Uuid, actor: Uuid) -> Result<Order, ApiError> {
    let o = st.store.get_order(id).await?;
    if o.cook_id == actor { Ok(o) } else { Err(ApiError::Forbidden("only the order's cook may do this")) }
}

/// Place an order as the authenticated eater (`x-user-id`). The payment is
/// authorized for the full amount; nothing is captured until the cook accepts.
#[utoipa::path(
    post, path = "/orders", tag = "orders",
    security(("user_id" = [])), request_body = CreateOrderRequest,
    responses(
        (status = 201, body = OrderResponse),
        (status = 400, body = ErrorBody),
        (status = 402, body = ErrorBody, description = "Payment declined"),
        (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody, description = "Regulatory violation (catering, retail sale, raw seafood, cook not certified)"),
    )
)]
pub async fn create(
    State(st): State<AppState>,
    Actor(eater_id): Actor,
    Json(req): Json<CreateOrderRequest>,
) -> Result<(StatusCode, Json<OrderResponse>), ApiError> {
    let now = st.clock.now();
    let cook = st.store.get_cook(req.cook_id).await?;
    let catalogue = st.store.list_dishes(cook.id).await?;

    let mut order = Order::place(
        &cook,
        &catalogue,
        NewOrder {
            eater_id,
            items: req.items.iter().map(|i| RequestedLine { dish_id: i.dish_id, quantity: i.quantity }).collect(),
            fulfillment: req.fulfillment.into(),
            purpose: req.purpose,
        },
        &st.config.pricing,
        now,
    )?;

    let auth = st
        .payments
        .authorize(AuthorizeRequest {
            order_id: order.id,
            amount_cents: order.total_cents,
            platform_fee_cents: order.platform_fee_cents,
            cook_account: None,
        })
        .await?;
    order.attach_payment(auth.id.clone());

    if let Err(e) = st.store.insert_order(&order).await {
        if let Err(v) = st.payments.void(&auth.id).await {
            tracing::error!(error = %v, payment = %auth.id, "failed to void authorization after insert failure");
        }
        return Err(e.into());
    }
    Ok((StatusCode::CREATED, Json(order.into())))
}

/// Fetch an order (its eater, its cook, or an admin).
#[utoipa::path(
    get, path = "/orders/{id}", tag = "orders", security(("user_id" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn get(State(st): State<AppState>, headers: HeaderMap, Path(id): Path<Uuid>) -> OrderReply {
    let o = st.store.get_order(id).await?;
    let actor = actor_id(&headers).ok();
    if !(is_admin(&headers, &st) || actor == Some(o.eater_id) || actor == Some(o.cook_id)) {
        return Err(ApiError::Forbidden("not your order"));
    }
    Ok(Json(o.into()))
}

/// Cook accepts the order. Re-checks the cook's certification under a row lock
/// and then captures the payment. Fails with 422 if the certificate is not
/// verified and current.
#[utoipa::path(
    post, path = "/orders/{id}/accept", tag = "orders", security(("user_id" = [])), params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OrderResponse),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "Order not in a state that can be accepted"),
        (status = 422, body = ErrorBody, description = "Cook's certification is not verified or has expired"),
    )
)]
pub async fn accept(State(st): State<AppState>, Actor(actor): Actor, Path(id): Path<Uuid>) -> OrderReply {
    owned_by_cook(&st, id, actor).await?;
    let now = st.clock.now();
    let accepted = st.store.transition_order(id, Box::new(move |cook, o| o.accept(cook, now))).await?;
    let pay = payment_ref(&accepted)?.to_owned();

    if let Err(e) = st.payments.capture(&pay).await {
        // Nothing was captured: unwind the acceptance and release the hold.
        tracing::error!(error = %e, order = %id, "capture failed after acceptance; cancelling");
        if let Err(c) = st.store.transition_order(id, Box::new(move |_, o| o.cancel(now))).await {
            tracing::error!(error = %c, order = %id, "failed to cancel order after capture failure");
        }
        if let Err(v) = st.payments.void(&pay).await {
            tracing::error!(error = %v, order = %id, "failed to void authorization after capture failure");
        }
        return Err(e.into());
    }
    Ok(Json(accepted.into()))
}

/// Food is ready: `ready_for_pickup` (pickup) or `out_for_delivery` (delivery).
#[utoipa::path(
    post, path = "/orders/{id}/ready", tag = "orders", security(("user_id" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn ready(State(st): State<AppState>, Actor(actor): Actor, Path(id): Path<Uuid>) -> OrderReply {
    owned_by_cook(&st, id, actor).await?;
    let now = st.clock.now();
    Ok(Json(st.store.transition_order(id, Box::new(move |_, o| o.mark_ready(now))).await?.into()))
}

/// Order handed over / delivered.
#[utoipa::path(
    post, path = "/orders/{id}/complete", tag = "orders", security(("user_id" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn complete(State(st): State<AppState>, Actor(actor): Actor, Path(id): Path<Uuid>) -> OrderReply {
    owned_by_cook(&st, id, actor).await?;
    let now = st.clock.now();
    Ok(Json(st.store.transition_order(id, Box::new(move |_, o| o.complete(now))).await?.into()))
}

async fn cancel_unaccepted(st: &AppState, id: Uuid, now: DateTime<Utc>) -> Result<Order, ApiError> {
    let order = st
        .store
        .transition_order(
            id,
            Box::new(move |_, o| {
                // Via the API only an order the cook has not yet accepted can be cancelled; later it's a refund.
                if o.status != OrderStatus::Placed {
                    return Err(DomainError::InvalidTransition { from: o.status, action: "cancel" });
                }
                o.cancel(now)
            }),
        )
        .await?;
    // Release the hold in full. The state change is already committed; if the
    // void fails the authorization simply expires, so log rather than fail.
    if let Some(p) = &order.payment_ref
        && let Err(e) = st.payments.void(p).await
    {
        tracing::error!(error = %e, order = %id, "failed to void authorization on cancel");
    }
    Ok(order)
}

/// Cancel an order that the cook has not accepted yet. The authorization is
/// released in full (nothing was charged).
#[utoipa::path(
    post, path = "/orders/{id}/cancel", tag = "orders", security(("user_id" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn cancel(State(st): State<AppState>, Actor(actor): Actor, Path(id): Path<Uuid>) -> OrderReply {
    let o = st.store.get_order(id).await?;
    if actor != o.eater_id && actor != o.cook_id {
        return Err(ApiError::Forbidden("not your order"));
    }
    Ok(Json(cancel_unaccepted(&st, id, st.clock.now()).await?.into()))
}

/// Full refund, platform fee (and delivery fee) included, for an unfulfilled
/// or incorrect order. Callable by the eater, the cook or an admin. For an
/// order the cook has not accepted yet this cancels it and releases the hold,
/// which also returns everything.
///
/// Idempotent towards the payment provider: a retry after a partial failure
/// does not refund twice.
#[utoipa::path(
    post, path = "/orders/{id}/refund", tag = "orders", security(("user_id" = [])), params(("id" = Uuid, Path)),
    request_body = RefundRequest,
    responses(
        (status = 200, body = OrderResponse),
        (status = 400, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "Already refunded / cancelled"),
    )
)]
pub async fn refund(State(st): State<AppState>, headers: HeaderMap, Path(id): Path<Uuid>, Json(req): Json<RefundRequest>) -> OrderReply {
    let order = st.store.get_order(id).await?;
    let actor = actor_id(&headers).ok();
    if !(is_admin(&headers, &st) || actor == Some(order.eater_id) || actor == Some(order.cook_id)) {
        return Err(ApiError::Forbidden("not your order"));
    }
    let now = st.clock.now();

    if order.status == OrderStatus::Placed {
        return Ok(Json(cancel_unaccepted(&st, id, now).await?.into()));
    }

    // Validate first so we never move money for an order the domain would refuse to refund.
    order.check_refundable(req.reason)?;
    let pay = payment_ref(&order)?;
    st.payments.refund(pay, order.total_cents, &format!("refund-{id}")).await?;

    let reason = req.reason;
    let updated = st.store.transition_order(id, Box::new(move |_, o| o.refund(reason, now).map(|_| ()))).await?;
    Ok(Json(updated.into()))
}
