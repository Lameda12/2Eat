use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use twoeat_domain::{
    CurrentUser, DomainError, NewOrder, Order, OrderStatus, RefundInitiator, RefundRecord, RefundSettlement, RefundStatus, RequestedLine,
    Role,
};
use twoeat_payments::AuthorizeRequest;
use twoeat_persistence::NewRefund;
use uuid::Uuid;

use crate::{
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
    security(("bearer_auth" = [])), request_body = CreateOrderRequest,
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
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<(StatusCode, Json<OrderResponse>), ApiError> {
    let eater_id = user.id;
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
    get, path = "/orders/{id}", tag = "orders", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody))
)]
pub async fn get(State(st): State<AppState>, Extension(user): Extension<CurrentUser>, Path(id): Path<Uuid>) -> OrderReply {
    let o = st.store.get_order(id).await?;
    if !(user.role == Role::Admin || user.id == o.eater_id || user.id == o.cook_id) {
        return Err(ApiError::Forbidden("not your order"));
    }
    Ok(Json(o.into()))
}

/// Cook accepts the order. Re-checks the cook's certification under a row lock
/// and then captures the payment. Fails with 422 if the certificate is not
/// verified and current.
#[utoipa::path(
    post, path = "/orders/{id}/accept", tag = "orders", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OrderResponse),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "Order not in a state that can be accepted"),
        (status = 422, body = ErrorBody, description = "Cook's certification is not verified or has expired"),
    )
)]
pub async fn accept(State(st): State<AppState>, Extension(user): Extension<CurrentUser>, Path(id): Path<Uuid>) -> OrderReply {
    owned_by_cook(&st, id, user.id).await?;
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
    post, path = "/orders/{id}/ready", tag = "orders", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn ready(State(st): State<AppState>, Extension(user): Extension<CurrentUser>, Path(id): Path<Uuid>) -> OrderReply {
    owned_by_cook(&st, id, user.id).await?;
    let now = st.clock.now();
    Ok(Json(st.store.transition_order(id, Box::new(move |_, o| o.mark_ready(now))).await?.into()))
}

/// Order handed over / delivered.
#[utoipa::path(
    post, path = "/orders/{id}/complete", tag = "orders", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn complete(State(st): State<AppState>, Extension(user): Extension<CurrentUser>, Path(id): Path<Uuid>) -> OrderReply {
    owned_by_cook(&st, id, user.id).await?;
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
    void_hold(st, &order).await;
    Ok(order)
}

/// Release the authorization in full. The state change is already committed; if the
/// void fails the authorization simply expires, so log rather than fail.
async fn void_hold(st: &AppState, order: &Order) {
    if let Some(p) = &order.payment_ref
        && let Err(e) = st.payments.void(p).await
    {
        tracing::error!(error = %e, order = %order.id, "failed to void authorization");
    }
}

/// Pay out an approved refund: return the order's FULL total (items + delivery fee +
/// platform fee) to the customer, then mark the order refunded. Safe to retry: the
/// payment provider call is idempotent per order and settlement is idempotent.
pub(crate) async fn pay_out_refund(st: &AppState, rec: &RefundRecord) -> Result<(), ApiError> {
    let order = st.store.get_order(rec.order_id).await?;
    if order.status == OrderStatus::Refunded {
        return Ok(());
    }
    if rec.amount_cents != order.total_cents {
        return Err(ApiError::Internal(format!("refund {} amount does not match order total", rec.id)));
    }
    // Re-validate at payout time: an admin may approve long after the request.
    order.check_refundable(rec.reason)?;
    let pay = payment_ref(&order)?;
    st.payments.refund(pay, order.total_cents, &format!("refund-{}", order.id)).await?;
    st.store.settle_refund(rec.id, st.clock.now()).await?;
    Ok(())
}

/// Cancel an order that the cook has not accepted yet. The authorization is
/// released in full (nothing was charged).
#[utoipa::path(
    post, path = "/orders/{id}/cancel", tag = "orders", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    responses((status = 200, body = OrderResponse), (status = 403, body = ErrorBody), (status = 409, body = ErrorBody))
)]
pub async fn cancel(State(st): State<AppState>, Extension(user): Extension<CurrentUser>, Path(id): Path<Uuid>) -> OrderReply {
    let o = st.store.get_order(id).await?;
    if user.id != o.eater_id && user.id != o.cook_id {
        return Err(ApiError::Forbidden("not your order"));
    }
    Ok(Json(cancel_unaccepted(&st, id, st.clock.now()).await?.into()))
}

/// Request a full refund for an unfulfilled or incorrect order. Callable by the
/// order's eater, its cook, or an admin.
///
/// * `approved`: the entire total, platform and delivery fee included, is returned.
///   (For an order the cook has not accepted yet this cancels it and releases the hold.)
/// * `pending_review` (202): an eater who already has 3 refunds in the trailing 30 days
///   is routed to an admin; **no money moves until an admin decides**.
///
/// One refund request per order, ever; repeating the call returns the same refund
/// (and retries a payout that previously failed).
#[utoipa::path(
    post, path = "/orders/{id}/refund", tag = "orders", security(("bearer_auth" = [])), params(("id" = Uuid, Path)),
    request_body = RefundRequest,
    responses(
        (status = 200, body = RefundAccepted, description = "Approved"),
        (status = 202, body = RefundAccepted, description = "Pending admin review"),
        (status = 400, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "Not refundable in this state, or an admin denied it"),
    )
)]
pub async fn refund(
    State(st): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<Uuid>,
    Json(req): Json<RefundRequest>,
) -> Result<(StatusCode, Json<RefundAccepted>), ApiError> {
    let order = st.store.get_order(id).await?;
    let initiator = if user.role == Role::Admin {
        RefundInitiator::Admin
    } else if user.id == order.cook_id {
        RefundInitiator::Cook
    } else if user.id == order.eater_id {
        RefundInitiator::Eater
    } else {
        return Err(ApiError::Forbidden("not your order"));
    };

    let outcome = st
        .store
        .request_refund(
            NewRefund { order_id: id, initiated_by: user.id, initiator, reason: req.reason, now: st.clock.now() },
            st.refund_policy.as_ref(),
        )
        .await?;
    let rec = outcome.record;
    let accepted = |status: &str| RefundAccepted { status: status.to_owned(), refund_id: rec.id };

    if outcome.order_cancelled {
        // Never accepted, never charged: the order was cancelled in the same transaction.
        void_hold(&st, &st.store.get_order(id).await?).await;
        return Ok((StatusCode::OK, Json(accepted("approved"))));
    }
    match rec.status() {
        RefundStatus::PendingReview => Ok((StatusCode::ACCEPTED, Json(accepted("pending_review")))),
        RefundStatus::Denied => Err(ApiError::Store(twoeat_persistence::StoreError::Conflict("an admin denied this refund"))),
        RefundStatus::Approved => {
            if rec.settlement == RefundSettlement::Refund {
                pay_out_refund(&st, &rec).await?;
            }
            Ok((StatusCode::OK, Json(accepted("approved"))))
        }
    }
}
