use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use twoeat_domain::{
    CertDecision, CertificationStatus, CertificationSubmission, Cook, DeliveredBy, DeliveryDetails, Dish, DishReview, DomainError,
    Fulfillment, LatLng, Order, OrderLine, OrderStatus, Refund, RefundDecision, RefundHistory, RefundInitiator, RefundPolicy, RefundReason,
    RefundRecord, RefundSettlement, RefundStatus, RegulatoryViolation, RejectionReason, Role,
};
use twoeat_geo::NearbyQuery;
use uuid::Uuid;

use crate::{
    AuditEntry, AuditFilter, AuditRecord, AuthStore, CookLocationRepo, HaversineCookLocationRepo, NearbyCook, NewRefund, NewSession,
    NewUser, OrderMutation, Page, RefundOutcome, SessionRecord, Store, StoreError, StoreResult, UserRecord,
};

#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn connect(database_url: &str) -> StoreResult<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(10).connect(database_url).await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> StoreResult<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await.map_err(|e| StoreError::Db(e.to_string()))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The default (bounding box + haversine) location repository over this pool.
    pub fn haversine_locations(&self) -> HaversineCookLocationRepo {
        HaversineCookLocationRepo::new(self.pool.clone())
    }
}

const COOK_COLS: &str = "id, display_name, food_safety_cert_expires_at, is_verified, lat, lng";
const DISH_COLS: &str = "id, cook_id, name, description, price_cents, is_catering, is_raw_seafood, is_active, review_state, review_flag";
const ORDER_COLS: &str = "id, eater_id, cook_id, fulfillment, delivery_address, delivery_postal, subtotal_cents, \
    platform_fee_cents, delivery_fee_cents, total_cents, status, payment_ref, refund_amount_cents, refund_reason, \
    refunded_at, placed_at, updated_at";
const REVIEW_COLS: &str = "id, cook_id, document_ref, content_type, status, submitted_at, reviewed_at, reviewer_id, \
    rejection_reason, rejection_detail, notes";
const REFUND_COLS: &str = "id, order_id, initiated_by, reason, amount_cents, settlement, decision, decided_by, decided_at, created_at";

fn corrupt(what: &str) -> StoreError {
    StoreError::Db(format!("corrupt row: {what}"))
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(|d| d.code()).map(|c| c.into_owned())
}

// ---------------------------------------------------------------- mappers

fn cook_from_row(r: &PgRow) -> StoreResult<Cook> {
    Ok(Cook {
        id: r.try_get("id")?,
        display_name: r.try_get("display_name")?,
        food_safety_cert_expires_at: r.try_get("food_safety_cert_expires_at")?,
        is_verified: r.try_get("is_verified")?,
        location: LatLng::new(r.try_get("lat")?, r.try_get("lng")?)?,
    })
}

fn dish_from_row(r: &PgRow) -> StoreResult<Dish> {
    let review: String = r.try_get("review_state")?;
    Ok(Dish {
        id: r.try_get("id")?,
        cook_id: r.try_get("cook_id")?,
        name: r.try_get("name")?,
        description: r.try_get("description")?,
        price_cents: r.try_get("price_cents")?,
        is_catering: r.try_get("is_catering")?,
        is_raw_seafood: r.try_get("is_raw_seafood")?,
        is_active: r.try_get("is_active")?,
        review: DishReview::parse(&review).ok_or_else(|| corrupt("dish review_state"))?,
        review_flag: r.try_get("review_flag")?,
    })
}

fn review_from_row(r: &PgRow) -> StoreResult<CertificationSubmission> {
    let status: String = r.try_get("status")?;
    let reason = match r.try_get::<Option<String>, _>("rejection_reason")? {
        None => None,
        Some(kind) => Some(RejectionReason::from_parts(&kind, r.try_get("rejection_detail")?).ok_or_else(|| corrupt("rejection_reason"))?),
    };
    Ok(CertificationSubmission {
        id: r.try_get("id")?,
        cook_id: r.try_get("cook_id")?,
        document_ref: r.try_get("document_ref")?,
        content_type: r.try_get("content_type")?,
        status: CertificationStatus::parse(&status).ok_or_else(|| corrupt("review status"))?,
        submitted_at: r.try_get("submitted_at")?,
        reviewed_at: r.try_get("reviewed_at")?,
        reviewer_id: r.try_get("reviewer_id")?,
        rejection_reason: reason,
        notes: r.try_get("notes")?,
    })
}

fn order_from_row(r: &PgRow, lines: Vec<OrderLine>) -> StoreResult<Order> {
    let kind: String = r.try_get("fulfillment")?;
    let fulfillment = match kind.as_str() {
        "pickup" => Fulfillment::Pickup,
        "delivery" => Fulfillment::Delivery(DeliveryDetails {
            address: r.try_get::<Option<String>, _>("delivery_address")?.ok_or_else(|| corrupt("delivery_address"))?,
            postal_code: r.try_get::<Option<String>, _>("delivery_postal")?.ok_or_else(|| corrupt("delivery_postal"))?,
            // Persisted orders are always cook-delivered: there is no courier model.
            delivered_by: DeliveredBy::Cook,
        }),
        _ => return Err(corrupt("fulfillment")),
    };
    let status: String = r.try_get("status")?;
    let refund = match r.try_get::<Option<i64>, _>("refund_amount_cents")? {
        None => None,
        Some(amount_cents) => {
            let reason: String = r.try_get::<Option<String>, _>("refund_reason")?.ok_or_else(|| corrupt("refund_reason"))?;
            Some(Refund {
                amount_cents,
                reason: RefundReason::parse(&reason).ok_or_else(|| corrupt("refund_reason"))?,
                refunded_at: r.try_get::<Option<DateTime<Utc>>, _>("refunded_at")?.ok_or_else(|| corrupt("refunded_at"))?,
            })
        }
    };
    Ok(Order {
        id: r.try_get("id")?,
        eater_id: r.try_get("eater_id")?,
        cook_id: r.try_get("cook_id")?,
        fulfillment,
        lines,
        subtotal_cents: r.try_get("subtotal_cents")?,
        platform_fee_cents: r.try_get("platform_fee_cents")?,
        delivery_fee_cents: r.try_get("delivery_fee_cents")?,
        total_cents: r.try_get("total_cents")?,
        status: OrderStatus::parse(&status).ok_or_else(|| corrupt("order status"))?,
        payment_ref: r.try_get("payment_ref")?,
        refund,
        placed_at: r.try_get("placed_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

fn refund_from_row(r: &PgRow) -> StoreResult<RefundRecord> {
    let reason: String = r.try_get("reason")?;
    let settlement: String = r.try_get("settlement")?;
    let decision = match r.try_get::<Option<String>, _>("decision")? {
        None => None,
        Some(d) => Some(RefundDecision::parse(&d).ok_or_else(|| corrupt("refund decision"))?),
    };
    Ok(RefundRecord {
        id: r.try_get("id")?,
        order_id: r.try_get("order_id")?,
        initiated_by: r.try_get("initiated_by")?,
        reason: RefundReason::parse(&reason).ok_or_else(|| corrupt("refund reason"))?,
        amount_cents: r.try_get("amount_cents")?,
        settlement: RefundSettlement::parse(&settlement).ok_or_else(|| corrupt("refund settlement"))?,
        decision,
        decided_by: r.try_get("decided_by")?,
        decided_at: r.try_get("decided_at")?,
        created_at: r.try_get("created_at")?,
    })
}

fn user_from_row(r: &PgRow) -> StoreResult<UserRecord> {
    let role: String = r.try_get("role")?;
    Ok(UserRecord {
        id: r.try_get("id")?,
        email: r.try_get("email")?,
        password_hash: r.try_get("password_hash")?,
        role: Role::parse(&role).ok_or_else(|| corrupt("user role"))?,
        created_at: r.try_get("created_at")?,
    })
}

// ---------------------------------------------------------------- helpers

/// Append an audit row inside the caller's transaction, so it commits or rolls
/// back together with the mutation it describes.
pub(crate) async fn write_audit(tx: &mut Transaction<'_, Postgres>, e: &AuditEntry) -> StoreResult<()> {
    sqlx::query("INSERT INTO audit_log (actor_id, action, target_type, target_id, metadata, created_at) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(e.actor_id)
        .bind(&e.action)
        .bind(&e.target_type)
        .bind(e.target_id)
        .bind(&e.metadata)
        .bind(e.at)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn load_lines(tx: &mut Transaction<'_, Postgres>, order_id: Uuid) -> StoreResult<Vec<OrderLine>> {
    let rows = sqlx::query("SELECT dish_id, dish_name, unit_price_cents, quantity FROM order_items WHERE order_id = $1 ORDER BY position")
        .bind(order_id)
        .fetch_all(&mut **tx)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(OrderLine {
                dish_id: r.try_get("dish_id")?,
                dish_name: r.try_get("dish_name")?,
                unit_price_cents: r.try_get("unit_price_cents")?,
                quantity: u32::try_from(r.try_get::<i32, _>("quantity")?).map_err(|_| corrupt("quantity"))?,
            })
        })
        .collect()
}

/// Cook (shared) before order (exclusive): the one lock order used everywhere,
/// so revocations (which only lock the cook) can never deadlock with these.
async fn lock_cook_and_order(tx: &mut Transaction<'_, Postgres>, order_id: Uuid) -> StoreResult<(Cook, Order)> {
    let cook_id: Uuid = sqlx::query_scalar("SELECT cook_id FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(StoreError::NotFound("order"))?;
    let cook_row =
        sqlx::query(&format!("SELECT {COOK_COLS} FROM cooks WHERE id = $1 FOR SHARE")).bind(cook_id).fetch_one(&mut **tx).await?;
    let order_row =
        sqlx::query(&format!("SELECT {ORDER_COLS} FROM orders WHERE id = $1 FOR UPDATE")).bind(order_id).fetch_one(&mut **tx).await?;
    let lines = load_lines(tx, order_id).await?;
    Ok((cook_from_row(&cook_row)?, order_from_row(&order_row, lines)?))
}

async fn save_order(tx: &mut Transaction<'_, Postgres>, order: &Order) -> StoreResult<()> {
    sqlx::query(
        "UPDATE orders SET status = $2, payment_ref = $3, refund_amount_cents = $4, refund_reason = $5, \
           refunded_at = $6, updated_at = $7 WHERE id = $1",
    )
    .bind(order.id)
    .bind(order.status.as_str())
    .bind(&order.payment_ref)
    .bind(order.refund.map(|r| r.amount_cents))
    .bind(order.refund.map(|r| r.reason.as_str()))
    .bind(order.refund.map(|r| r.refunded_at))
    .bind(order.updated_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Latest *decided* certificate review is `approved`.
async fn has_approved_review(tx: &mut Transaction<'_, Postgres>, cook_id: Uuid) -> StoreResult<bool> {
    let latest: Option<String> = sqlx::query_scalar(
        "SELECT status FROM certificate_reviews WHERE cook_id = $1 AND status <> 'pending' \
         ORDER BY reviewed_at DESC NULLS LAST, submitted_at DESC LIMIT 1",
    )
    .bind(cook_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(latest.as_deref() == Some("approved"))
}

async fn count_u32(tx: &mut Transaction<'_, Postgres>, sql: &str, a: Uuid, b: Option<Uuid>, since: DateTime<Utc>) -> StoreResult<u32> {
    let q = sqlx::query_scalar::<_, i64>(sql).bind(a);
    let q = if let Some(b) = b { q.bind(b) } else { q };
    let n = q.bind(since).fetch_one(&mut **tx).await?;
    Ok(n as u32)
}

const EATER_COUNT_SQL: &str = "SELECT count(*) FROM refunds WHERE initiated_by = $1 AND order_id <> $2 AND settlement = 'refund' \
    AND decision IN ('auto', 'admin_approved') AND created_at > $3";
const COOK_COUNT_SQL: &str = "SELECT count(*) FROM refunds r JOIN orders o ON o.id = r.order_id WHERE o.cook_id = $1 \
    AND r.settlement = 'refund' AND r.decision IN ('auto', 'admin_approved') AND r.created_at > $2";

/// Flag the cook for admin review (audit entry only; never auto-suspends) when
/// the policy says they have too many refunds against them.
async fn maybe_flag_cook(
    tx: &mut Transaction<'_, Postgres>,
    cook_id: Uuid,
    policy: &dyn RefundPolicy,
    now: DateTime<Utc>,
    trigger_refund: Uuid,
) -> StoreResult<()> {
    let count = count_u32(tx, COOK_COUNT_SQL, cook_id, None, now - policy.window()).await?;
    let h = RefundHistory { eater_refunds_in_window: 0, cook_refunds_in_window: count };
    if policy.should_flag_cook(&h, cook_id, now) {
        write_audit(
            tx,
            &AuditEntry::new(
                None,
                "cook.flagged_for_review",
                "cook",
                cook_id,
                json!({"refunds_in_window": count, "window_days": policy.window().num_days(), "triggered_by_refund": trigger_refund}),
                now,
            ),
        )
        .await?;
    }
    Ok(())
}

// ------------------------------------------------------------------ Store

#[async_trait]
impl Store for PgStore {
    async fn register_cook(&self, cook: &Cook, s: &CertificationSubmission, terms_accepted_at: DateTime<Utc>) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO cooks (id, display_name, food_safety_cert_expires_at, is_verified, lat, lng, hbfb_terms_accepted_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(cook.id)
        .bind(&cook.display_name)
        .bind(cook.food_safety_cert_expires_at)
        .bind(cook.is_verified)
        .bind(cook.location.lat())
        .bind(cook.location.lng())
        .bind(terms_accepted_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if sqlstate(&e).as_deref() == Some("23505") { StoreError::Conflict("cook profile already exists") } else { e.into() }
        })?;
        sqlx::query(
            "INSERT INTO certificate_reviews (id, cook_id, document_ref, content_type, status, submitted_at, reviewed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(s.id)
        .bind(s.cook_id)
        .bind(&s.document_ref)
        .bind(&s.content_type)
        .bind(s.status.as_str())
        .bind(s.submitted_at)
        .bind(s.reviewed_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_cook(&self, id: Uuid) -> StoreResult<Cook> {
        let row = sqlx::query(&format!("SELECT {COOK_COLS} FROM cooks WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("cook"))?;
        cook_from_row(&row)
    }

    async fn latest_submission(&self, cook_id: Uuid) -> StoreResult<Option<CertificationSubmission>> {
        sqlx::query(&format!("SELECT {REVIEW_COLS} FROM certificate_reviews WHERE cook_id = $1 ORDER BY submitted_at DESC LIMIT 1"))
            .bind(cook_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| review_from_row(&r))
            .transpose()
    }

    async fn get_review(&self, id: Uuid) -> StoreResult<CertificationSubmission> {
        let row = sqlx::query(&format!("SELECT {REVIEW_COLS} FROM certificate_reviews WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("certificate review"))?;
        review_from_row(&row)
    }

    async fn list_reviews(
        &self,
        status: Option<CertificationStatus>,
        limit: i64,
        offset: i64,
    ) -> StoreResult<Page<CertificationSubmission>> {
        let st = status.map(|s| s.as_str());
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM certificate_reviews WHERE ($1::text IS NULL OR status = $1)")
            .bind(st)
            .fetch_one(&self.pool)
            .await?;
        let rows = sqlx::query(&format!(
            "SELECT {REVIEW_COLS} FROM certificate_reviews WHERE ($1::text IS NULL OR status = $1) \
             ORDER BY submitted_at, id LIMIT $2 OFFSET $3"
        ))
        .bind(st)
        .bind(limit.max(0))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(Page { items: rows.iter().map(review_from_row).collect::<StoreResult<_>>()?, total })
    }

    async fn decide_review(
        &self,
        review_id: Uuid,
        actor: Option<Uuid>,
        decision: CertDecision,
        notes: Option<String>,
        now: DateTime<Utc>,
    ) -> StoreResult<(CertificationSubmission, Cook)> {
        let mut tx = self.pool.begin().await?;
        let cook_id: Uuid = sqlx::query_scalar("SELECT cook_id FROM certificate_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("certificate review"))?;
        // Cook first (exclusive: this is what makes in-flight acceptances wait), then the review.
        let cook_row =
            sqlx::query(&format!("SELECT {COOK_COLS} FROM cooks WHERE id = $1 FOR UPDATE")).bind(cook_id).fetch_one(&mut *tx).await?;
        let rev_row = sqlx::query(&format!("SELECT {REVIEW_COLS} FROM certificate_reviews WHERE id = $1 FOR UPDATE"))
            .bind(review_id)
            .fetch_one(&mut *tx)
            .await?;
        let mut cook = cook_from_row(&cook_row)?;
        let mut review = review_from_row(&rev_row)?;
        if review.status != CertificationStatus::Pending {
            return Err(StoreError::Conflict("certificate review already decided"));
        }

        let (action, metadata) = match &decision {
            CertDecision::Approve { expires_at } => {
                cook.approve_certification(*expires_at, now)?;
                review.status = CertificationStatus::Approved;
                ("certificate.approved", json!({"cook_id": cook_id, "expires_at": expires_at, "notes": notes}))
            }
            CertDecision::Reject { reason } => {
                reason.validate()?;
                cook.revoke_verification();
                review.status = CertificationStatus::Rejected;
                review.rejection_reason = Some(reason.clone());
                let (kind, detail) = reason.to_parts();
                ("certificate.rejected", json!({"cook_id": cook_id, "reason": kind, "reason_detail": detail, "notes": notes}))
            }
        };
        review.reviewed_at = Some(now);
        review.reviewer_id = actor;
        review.notes = notes;

        // Audit first, mutation after: if anything below fails, the whole transaction (audit included) rolls back.
        write_audit(&mut tx, &AuditEntry::new(actor, action, "certificate_review", review_id, metadata, now)).await?;
        sqlx::query("UPDATE cooks SET is_verified = $2, food_safety_cert_expires_at = $3 WHERE id = $1")
            .bind(cook_id)
            .bind(cook.is_verified)
            .bind(cook.food_safety_cert_expires_at)
            .execute(&mut *tx)
            .await?;
        let (reason_kind, reason_detail) = review.rejection_reason.as_ref().map(|r| r.to_parts()).unzip();
        sqlx::query(
            "UPDATE certificate_reviews SET status = $2, reviewed_at = $3, reviewer_id = $4, rejection_reason = $5, \
               rejection_detail = $6, notes = $7 WHERE id = $1",
        )
        .bind(review_id)
        .bind(review.status.as_str())
        .bind(review.reviewed_at)
        .bind(review.reviewer_id)
        .bind(reason_kind)
        .bind(reason_detail.flatten())
        .bind(&review.notes)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((review, cook))
    }

    async fn revoke_cook(&self, actor: Option<Uuid>, cook_id: Uuid, now: DateTime<Utc>) -> StoreResult<Cook> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!("SELECT {COOK_COLS} FROM cooks WHERE id = $1 FOR UPDATE"))
            .bind(cook_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("cook"))?;
        let mut cook = cook_from_row(&row)?;
        cook.revoke_verification();
        write_audit(&mut tx, &AuditEntry::new(actor, "cook.revoked", "cook", cook_id, json!({}), now)).await?;
        sqlx::query("UPDATE cooks SET is_verified = FALSE WHERE id = $1").bind(cook_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(cook)
    }

    async fn review_certification(&self, cook_id: Uuid, decision: CertDecision, now: DateTime<Utc>) -> StoreResult<Cook> {
        let review_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM certificate_reviews WHERE cook_id = $1 AND status = 'pending' ORDER BY submitted_at DESC LIMIT 1",
        )
        .bind(cook_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound("pending certificate review"))?;
        Ok(self.decide_review(review_id, None, decision, None, now).await?.1)
    }

    async fn revoke_verification(&self, cook_id: Uuid) -> StoreResult<Cook> {
        self.revoke_cook(None, cook_id, Utc::now()).await
    }

    async fn nearby_cooks(&self, q: &NearbyQuery, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        self.haversine_locations().find_nearby(q.center, q.radius_m / 1000.0, q.limit, now).await
    }

    async fn insert_dish(&self, d: &Dish) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO dishes (id, cook_id, name, description, price_cents, is_catering, is_raw_seafood, is_active, review_state, review_flag) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(d.id)
        .bind(d.cook_id)
        .bind(&d.name)
        .bind(&d.description)
        .bind(d.price_cents)
        .bind(d.is_catering)
        .bind(d.is_raw_seafood)
        .bind(d.is_active)
        .bind(d.review.as_str())
        .bind(&d.review_flag)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_dish(&self, id: Uuid) -> StoreResult<Dish> {
        let row = sqlx::query(&format!("SELECT {DISH_COLS} FROM dishes WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("dish"))?;
        dish_from_row(&row)
    }

    async fn list_dishes(&self, cook_id: Uuid) -> StoreResult<Vec<Dish>> {
        let rows = sqlx::query(&format!("SELECT {DISH_COLS} FROM dishes WHERE cook_id = $1 ORDER BY name"))
            .bind(cook_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(dish_from_row).collect()
    }

    async fn list_flagged_dishes(&self, limit: i64, offset: i64) -> StoreResult<Page<Dish>> {
        let total: i64 =
            sqlx::query_scalar("SELECT count(*) FROM dishes WHERE review_state = 'pending_review'").fetch_one(&self.pool).await?;
        let rows = sqlx::query(&format!(
            "SELECT {DISH_COLS} FROM dishes WHERE review_state = 'pending_review' ORDER BY created_at, id LIMIT $1 OFFSET $2"
        ))
        .bind(limit.max(0))
        .bind(offset.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(Page { items: rows.iter().map(dish_from_row).collect::<StoreResult<_>>()?, total })
    }

    async fn decide_dish_review(&self, dish_id: Uuid, actor: Uuid, approve: bool, now: DateTime<Utc>) -> StoreResult<Dish> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!("SELECT {DISH_COLS} FROM dishes WHERE id = $1 FOR UPDATE"))
            .bind(dish_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("dish"))?;
        let mut dish = dish_from_row(&row)?;
        let flag = dish.review_flag.clone();
        if approve {
            dish.approve_review()?
        } else {
            dish.reject_review()?
        }
        let action = if approve { "dish.review_approved" } else { "dish.review_rejected" };
        write_audit(&mut tx, &AuditEntry::new(Some(actor), action, "dish", dish_id, json!({"cook_id": dish.cook_id, "flag": flag}), now))
            .await?;
        sqlx::query("UPDATE dishes SET is_active = $2, review_state = $3 WHERE id = $1")
            .bind(dish_id)
            .bind(dish.is_active)
            .bind(dish.review.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(dish)
    }

    async fn insert_order(&self, o: &Order) -> StoreResult<()> {
        let (kind, addr, postal) = match &o.fulfillment {
            Fulfillment::Pickup => ("pickup", None, None),
            Fulfillment::Delivery(d) => ("delivery", Some(d.address.as_str()), Some(d.postal_code.as_str())),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO orders (id, eater_id, cook_id, fulfillment, delivery_address, delivery_postal, subtotal_cents, \
               platform_fee_cents, delivery_fee_cents, total_cents, status, payment_ref, placed_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(o.id)
        .bind(o.eater_id)
        .bind(o.cook_id)
        .bind(kind)
        .bind(addr)
        .bind(postal)
        .bind(o.subtotal_cents)
        .bind(o.platform_fee_cents)
        .bind(o.delivery_fee_cents)
        .bind(o.total_cents)
        .bind(o.status.as_str())
        .bind(&o.payment_ref)
        .bind(o.placed_at)
        .bind(o.updated_at)
        .execute(&mut *tx)
        .await?;
        for (i, l) in o.lines.iter().enumerate() {
            sqlx::query(
                "INSERT INTO order_items (order_id, position, dish_id, dish_name, unit_price_cents, quantity) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(o.id)
            .bind(i as i32)
            .bind(l.dish_id)
            .bind(&l.dish_name)
            .bind(l.unit_price_cents)
            .bind(l.quantity as i32)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn get_order(&self, id: Uuid) -> StoreResult<Order> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!("SELECT {ORDER_COLS} FROM orders WHERE id = $1"))
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("order"))?;
        let lines = load_lines(&mut tx, id).await?;
        order_from_row(&row, lines)
    }

    async fn transition_order(&self, id: Uuid, f: OrderMutation) -> StoreResult<Order> {
        let mut tx = self.pool.begin().await?;
        let (cook, mut order) = lock_cook_and_order(&mut tx, id).await?;
        let before = order.status;

        f(&cook, &mut order)?;

        // Application-level twin of the DB trigger: never persist an accepted
        // order for a cook without an approved certificate review.
        if before != OrderStatus::AcceptedByCook
            && order.status == OrderStatus::AcceptedByCook
            && !has_approved_review(&mut tx, cook.id).await?
        {
            return Err(DomainError::from(RegulatoryViolation::CertificationNotVerified).into());
        }

        save_order(&mut tx, &order).await?;
        tx.commit().await?;
        Ok(order)
    }

    async fn request_refund(&self, req: NewRefund, policy: &dyn RefundPolicy) -> StoreResult<RefundOutcome> {
        let mut tx = self.pool.begin().await?;

        // Serialise refund decisions per eater so two concurrent requests cannot
        // both read "2 refunds so far" and both auto-approve.
        let eater_id: Uuid = sqlx::query_scalar("SELECT eater_id FROM orders WHERE id = $1")
            .bind(req.order_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("order"))?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))").bind(eater_id).execute(&mut *tx).await?;

        let (_cook, mut order) = lock_cook_and_order(&mut tx, req.order_id).await?;

        if let Some(row) =
            sqlx::query(&format!("SELECT {REFUND_COLS} FROM refunds WHERE order_id = $1")).bind(order.id).fetch_optional(&mut *tx).await?
        {
            return Ok(RefundOutcome { record: refund_from_row(&row)?, created: false, order_cancelled: false });
        }

        let (settlement, cancel) = if order.status == OrderStatus::Placed {
            (RefundSettlement::Void, true)
        } else {
            order.check_refundable(req.reason)?;
            (RefundSettlement::Refund, false)
        };

        let since = req.now - policy.window();
        let (decision, decided_by) = match req.initiator {
            RefundInitiator::Admin => (Some(RefundDecision::AdminApproved), Some(req.initiated_by)),
            RefundInitiator::Cook => (Some(RefundDecision::Auto), None),
            // Nothing was charged, so there is nothing to abuse.
            RefundInitiator::Eater if settlement == RefundSettlement::Void => (Some(RefundDecision::Auto), None),
            RefundInitiator::Eater => {
                let h = RefundHistory {
                    eater_refunds_in_window: count_u32(&mut tx, EATER_COUNT_SQL, order.eater_id, Some(order.id), since).await?,
                    cook_refunds_in_window: count_u32(&mut tx, COOK_COUNT_SQL, order.cook_id, None, since).await?,
                };
                (policy.should_auto_approve(&h, order.eater_id, order.cook_id, req.now).then_some(RefundDecision::Auto), None)
            }
        };

        let record = RefundRecord {
            id: Uuid::new_v4(),
            order_id: order.id,
            initiated_by: req.initiated_by,
            reason: req.reason,
            amount_cents: order.total_cents,
            settlement,
            decision,
            decided_by,
            decided_at: decision.map(|_| req.now),
            created_at: req.now,
        };

        write_audit(
            &mut tx,
            &AuditEntry::new(
                Some(req.initiated_by),
                "refund.requested",
                "refund",
                record.id,
                json!({"order_id": order.id, "status": record.status().as_str(), "reason": req.reason.as_str(),
                       "settlement": settlement.as_str(), "amount_cents": record.amount_cents}),
                req.now,
            ),
        )
        .await?;
        sqlx::query(
            "INSERT INTO refunds (id, order_id, initiated_by, reason, amount_cents, settlement, decision, decided_by, decided_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(record.id)
        .bind(record.order_id)
        .bind(record.initiated_by)
        .bind(record.reason.as_str())
        .bind(record.amount_cents)
        .bind(record.settlement.as_str())
        .bind(record.decision.map(|d| d.as_str()))
        .bind(record.decided_by)
        .bind(record.decided_at)
        .bind(record.created_at)
        .execute(&mut *tx)
        .await?;
        if cancel {
            order.cancel(req.now)?;
            save_order(&mut tx, &order).await?;
        }
        if settlement == RefundSettlement::Refund && record.status() == RefundStatus::Approved {
            maybe_flag_cook(&mut tx, order.cook_id, policy, req.now, record.id).await?;
        }
        tx.commit().await?;
        Ok(RefundOutcome { record, created: true, order_cancelled: cancel })
    }

    async fn get_refund(&self, id: Uuid) -> StoreResult<RefundRecord> {
        let row = sqlx::query(&format!("SELECT {REFUND_COLS} FROM refunds WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("refund"))?;
        refund_from_row(&row)
    }

    async fn get_refund_for_order(&self, order_id: Uuid) -> StoreResult<Option<RefundRecord>> {
        sqlx::query(&format!("SELECT {REFUND_COLS} FROM refunds WHERE order_id = $1"))
            .bind(order_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| refund_from_row(&r))
            .transpose()
    }

    async fn list_refunds(&self, status: Option<RefundStatus>, limit: i64, offset: i64) -> StoreResult<Page<RefundRecord>> {
        let where_clause = match status {
            None => "TRUE",
            Some(RefundStatus::PendingReview) => "decision IS NULL",
            Some(RefundStatus::Approved) => "decision IN ('auto', 'admin_approved')",
            Some(RefundStatus::Denied) => "decision = 'admin_denied'",
        };
        let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM refunds WHERE {where_clause}")).fetch_one(&self.pool).await?;
        let rows =
            sqlx::query(&format!("SELECT {REFUND_COLS} FROM refunds WHERE {where_clause} ORDER BY created_at, id LIMIT $1 OFFSET $2"))
                .bind(limit.max(0))
                .bind(offset.max(0))
                .fetch_all(&self.pool)
                .await?;
        Ok(Page { items: rows.iter().map(refund_from_row).collect::<StoreResult<_>>()?, total })
    }

    async fn decide_refund(
        &self,
        refund_id: Uuid,
        admin: Uuid,
        approve: bool,
        notes: Option<String>,
        now: DateTime<Utc>,
        policy: &dyn RefundPolicy,
    ) -> StoreResult<RefundRecord> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!("SELECT {REFUND_COLS} FROM refunds WHERE id = $1 FOR UPDATE"))
            .bind(refund_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("refund"))?;
        let mut record = refund_from_row(&row)?;
        if record.decision.is_some() {
            return Err(StoreError::Conflict("refund already decided"));
        }
        record.decision = Some(if approve { RefundDecision::AdminApproved } else { RefundDecision::AdminDenied });
        record.decided_by = Some(admin);
        record.decided_at = Some(now);

        write_audit(
            &mut tx,
            &AuditEntry::new(
                Some(admin),
                if approve { "refund.admin_approved" } else { "refund.admin_denied" },
                "refund",
                refund_id,
                json!({"order_id": record.order_id, "notes": notes}),
                now,
            ),
        )
        .await?;
        sqlx::query("UPDATE refunds SET decision = $2, decided_by = $3, decided_at = $4 WHERE id = $1")
            .bind(refund_id)
            .bind(record.decision.map(|d| d.as_str()))
            .bind(admin)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        if approve {
            let cook_id: Uuid =
                sqlx::query_scalar("SELECT cook_id FROM orders WHERE id = $1").bind(record.order_id).fetch_one(&mut *tx).await?;
            maybe_flag_cook(&mut tx, cook_id, policy, now, refund_id).await?;
        }
        tx.commit().await?;
        Ok(record)
    }

    async fn settle_refund(&self, refund_id: Uuid, now: DateTime<Utc>) -> StoreResult<(RefundRecord, Order)> {
        let mut tx = self.pool.begin().await?;
        let order_id: Uuid = sqlx::query_scalar("SELECT order_id FROM refunds WHERE id = $1")
            .bind(refund_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("refund"))?;
        let (_cook, mut order) = lock_cook_and_order(&mut tx, order_id).await?;
        let row =
            sqlx::query(&format!("SELECT {REFUND_COLS} FROM refunds WHERE id = $1 FOR UPDATE")).bind(refund_id).fetch_one(&mut *tx).await?;
        let record = refund_from_row(&row)?;
        if record.status() != RefundStatus::Approved || record.settlement != RefundSettlement::Refund {
            return Err(StoreError::Conflict("refund is not an approved payout"));
        }
        if order.status == OrderStatus::Refunded {
            return Ok((record, order));
        }
        order.refund(record.reason, now)?;
        write_audit(
            &mut tx,
            &AuditEntry::new(
                None,
                "refund.settled",
                "refund",
                refund_id,
                json!({"order_id": order_id, "amount_cents": order.total_cents}),
                now,
            ),
        )
        .await?;
        save_order(&mut tx, &order).await?;
        tx.commit().await?;
        Ok((record, order))
    }

    async fn audit_entries(&self, f: AuditFilter, limit: i64) -> StoreResult<Vec<AuditRecord>> {
        let rows = sqlx::query(
            "SELECT id, actor_id, action, target_type, target_id, metadata, created_at FROM audit_log \
             WHERE ($1::text IS NULL OR target_type = $1) AND ($2::uuid IS NULL OR target_id = $2) AND ($3::text IS NULL OR action = $3) \
             ORDER BY seq LIMIT $4",
        )
        .bind(&f.target_type)
        .bind(f.target_id)
        .bind(&f.action)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AuditRecord {
                    id: r.try_get("id")?,
                    entry: AuditEntry {
                        actor_id: r.try_get("actor_id")?,
                        action: r.try_get("action")?,
                        target_type: r.try_get("target_type")?,
                        target_id: r.try_get("target_id")?,
                        metadata: r.try_get("metadata")?,
                        at: r.try_get("created_at")?,
                    },
                })
            })
            .collect()
    }
}

// -------------------------------------------------------------- AuthStore

#[async_trait]
impl AuthStore for PgStore {
    async fn create_user(&self, u: NewUser) -> StoreResult<()> {
        sqlx::query("INSERT INTO users (id, email, password_hash, role, created_at) VALUES ($1, $2::citext, $3, $4, $5)")
            .bind(u.id)
            .bind(&u.email)
            .bind(&u.password_hash)
            .bind(u.role.as_str())
            .bind(u.created_at)
            .execute(&self.pool)
            .await
            .map_err(
                |e| if sqlstate(&e).as_deref() == Some("23505") { StoreError::Conflict("email already registered") } else { e.into() },
            )?;
        Ok(())
    }

    async fn find_user_by_email(&self, email: &str) -> StoreResult<Option<UserRecord>> {
        // Explicit ::citext cast: comparing citext with a plain text parameter would be case-sensitive.
        sqlx::query("SELECT id, email::text AS email, password_hash, role, created_at FROM users WHERE email = $1::citext")
            .bind(email)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| user_from_row(&r))
            .transpose()
    }

    async fn get_user(&self, id: Uuid) -> StoreResult<UserRecord> {
        let row = sqlx::query("SELECT id, email::text AS email, password_hash, role, created_at FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("user"))?;
        user_from_row(&row)
    }

    async fn create_session(&self, s: NewSession) -> StoreResult<()> {
        sqlx::query("INSERT INTO sessions (id, user_id, token_hash, expires_at, created_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(s.id)
            .bind(s.user_id)
            .bind(&s.token_hash[..])
            .bind(s.expires_at)
            .bind(s.created_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn find_session(&self, token_hash: &[u8; 32]) -> StoreResult<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT s.id, s.user_id, u.role, s.expires_at, s.revoked_at FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = $1",
        )
        .bind(&token_hash[..])
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let role: String = r.try_get("role")?;
            Ok(SessionRecord {
                session_id: r.try_get("id")?,
                user_id: r.try_get("user_id")?,
                role: Role::parse(&role).ok_or_else(|| corrupt("user role"))?,
                expires_at: r.try_get("expires_at")?,
                revoked_at: r.try_get("revoked_at")?,
            })
        })
        .transpose()
    }

    async fn revoke_session(&self, session_id: Uuid, now: DateTime<Utc>) -> StoreResult<bool> {
        let r = sqlx::query("UPDATE sessions SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
            .bind(session_id)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    async fn rotate_session(&self, old: Uuid, new: NewSession, now: DateTime<Utc>) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query("UPDATE sessions SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL AND expires_at > $2")
            .bind(old)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() != 1 {
            return Err(StoreError::NotFound("active session"));
        }
        sqlx::query("INSERT INTO sessions (id, user_id, token_hash, expires_at, created_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(new.id)
            .bind(new.user_id)
            .bind(&new.token_hash[..])
            .bind(new.expires_at)
            .bind(new.created_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[async_trait]
impl CookLocationRepo for PgStore {
    async fn find_nearby(&self, center: LatLng, radius_km: f64, limit: u32, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        self.haversine_locations().find_nearby(center, radius_km, limit, now).await
    }
}
