use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgRow};
use twoeat_domain::{
    CertDecision, CertificationStatus, CertificationSubmission, Cook, DeliveryDetails, Dish, DomainError, Fulfillment, LatLng, Order,
    OrderLine, OrderStatus, Refund, RefundReason,
};
use twoeat_geo::{EARTH_RADIUS_M, NearbyQuery};
use uuid::Uuid;

use crate::{NearbyCook, OrderMutation, Store, StoreError, StoreResult};

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
}

const COOK_COLS: &str = "id, display_name, food_safety_cert_expires_at, is_verified, lat, lng";
const DISH_COLS: &str = "id, cook_id, name, description, price_cents, is_catering, is_raw_seafood, is_active";
const ORDER_COLS: &str = "id, eater_id, cook_id, fulfillment, delivery_address, delivery_postal, subtotal_cents, \
    platform_fee_cents, delivery_fee_cents, total_cents, status, payment_ref, refund_amount_cents, refund_reason, \
    refunded_at, placed_at, updated_at";

fn corrupt(what: &str) -> StoreError {
    StoreError::Db(format!("corrupt row: {what}"))
}

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
    Ok(Dish {
        id: r.try_get("id")?,
        cook_id: r.try_get("cook_id")?,
        name: r.try_get("name")?,
        description: r.try_get("description")?,
        price_cents: r.try_get("price_cents")?,
        is_catering: r.try_get("is_catering")?,
        is_raw_seafood: r.try_get("is_raw_seafood")?,
        is_active: r.try_get("is_active")?,
    })
}

fn submission_from_row(r: &PgRow) -> StoreResult<CertificationSubmission> {
    let status: String = r.try_get("status")?;
    Ok(CertificationSubmission {
        id: r.try_get("id")?,
        cook_id: r.try_get("cook_id")?,
        document_ref: r.try_get("document_ref")?,
        content_type: r.try_get("content_type")?,
        status: CertificationStatus::parse(&status).ok_or_else(|| corrupt("submission status"))?,
        submitted_at: r.try_get("submitted_at")?,
        reviewed_at: r.try_get("reviewed_at")?,
    })
}

fn order_from_row(r: &PgRow, lines: Vec<OrderLine>) -> StoreResult<Order> {
    let kind: String = r.try_get("fulfillment")?;
    let fulfillment = match kind.as_str() {
        "pickup" => Fulfillment::Pickup,
        "delivery" => Fulfillment::Delivery(DeliveryDetails {
            address: r.try_get::<Option<String>, _>("delivery_address")?.ok_or_else(|| corrupt("delivery_address"))?,
            postal_code: r.try_get::<Option<String>, _>("delivery_postal")?.ok_or_else(|| corrupt("delivery_postal"))?,
        }),
        _ => return Err(corrupt("fulfillment")),
    };
    let status: String = r.try_get("status")?;
    let total_cents: i64 = r.try_get("total_cents")?;
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
        total_cents,
        status: OrderStatus::parse(&status).ok_or_else(|| corrupt("order status"))?,
        payment_ref: r.try_get("payment_ref")?,
        refund,
        placed_at: r.try_get("placed_at")?,
        updated_at: r.try_get("updated_at")?,
    })
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
        .await?;
        sqlx::query(
            "INSERT INTO certification_submissions (id, cook_id, document_ref, content_type, status, submitted_at, reviewed_at) \
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
        sqlx::query(
            "SELECT id, cook_id, document_ref, content_type, status, submitted_at, reviewed_at \
             FROM certification_submissions WHERE cook_id = $1 ORDER BY submitted_at DESC LIMIT 1",
        )
        .bind(cook_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|r| submission_from_row(&r))
        .transpose()
    }

    async fn review_certification(&self, cook_id: Uuid, decision: CertDecision, now: DateTime<Utc>) -> StoreResult<Cook> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(&format!("SELECT {COOK_COLS} FROM cooks WHERE id = $1 FOR UPDATE"))
            .bind(cook_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("cook"))?;
        let mut cook = cook_from_row(&row)?;
        let status = match decision {
            CertDecision::Approve { expires_at } => {
                cook.approve_certification(expires_at, now)?;
                CertificationStatus::Approved
            }
            CertDecision::Reject => {
                cook.revoke_verification();
                CertificationStatus::Rejected
            }
        };
        sqlx::query("UPDATE cooks SET is_verified = $2, food_safety_cert_expires_at = $3 WHERE id = $1")
            .bind(cook_id)
            .bind(cook.is_verified)
            .bind(cook.food_safety_cert_expires_at)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE certification_submissions SET status = $2, reviewed_at = $3 WHERE id = (\
               SELECT id FROM certification_submissions WHERE cook_id = $1 AND status = 'pending' \
               ORDER BY submitted_at DESC LIMIT 1)",
        )
        .bind(cook_id)
        .bind(status.as_str())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(cook)
    }

    async fn revoke_verification(&self, cook_id: Uuid) -> StoreResult<Cook> {
        // The UPDATE takes the row lock, queueing behind / blocking order acceptance.
        let row = sqlx::query(&format!("UPDATE cooks SET is_verified = FALSE WHERE id = $1 RETURNING {COOK_COLS}"))
            .bind(cook_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound("cook"))?;
        cook_from_row(&row)
    }

    async fn nearby_cooks(&self, q: &NearbyQuery, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        let bb = q.bounding_box();
        // Bounding box hits the (lat, lng) partial index; haversine gives the exact distance.
        let sql = format!(
            "SELECT id, display_name, dist FROM ( \
               SELECT id, display_name, 2 * {EARTH_RADIUS_M:?}::float8 * asin(LEAST(1.0, sqrt( \
                   power(sin(radians(lat - $1::float8) / 2), 2) + \
                   cos(radians($1::float8)) * cos(radians(lat)) * power(sin(radians(lng - $2::float8) / 2), 2)))) AS dist \
               FROM cooks \
               WHERE is_verified AND food_safety_cert_expires_at > $3 \
                 AND lat BETWEEN $4 AND $5 AND lng BETWEEN $6 AND $7 \
             ) t WHERE dist <= $8 ORDER BY dist LIMIT $9"
        );
        let rows = sqlx::query(&sql)
            .bind(q.center.lat())
            .bind(q.center.lng())
            .bind(now)
            .bind(bb.min_lat)
            .bind(bb.max_lat)
            .bind(bb.min_lng)
            .bind(bb.max_lng)
            .bind(q.radius_m)
            .bind(i64::from(q.limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok(NearbyCook { id: r.try_get("id")?, display_name: r.try_get("display_name")?, distance_m: r.try_get("dist")? }))
            .collect()
    }

    async fn insert_dish(&self, d: &Dish) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO dishes (id, cook_id, name, description, price_cents, is_catering, is_raw_seafood, is_active) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(d.id)
        .bind(d.cook_id)
        .bind(&d.name)
        .bind(&d.description)
        .bind(d.price_cents)
        .bind(d.is_catering)
        .bind(d.is_raw_seafood)
        .bind(d.is_active)
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
                "INSERT INTO order_items (order_id, position, dish_id, dish_name, unit_price_cents, quantity) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
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

        // Lock order: cook (shared) before order (exclusive). Revocation only
        // ever touches the cook row, so there is no lock-order cycle.
        let cook_id: Uuid = sqlx::query_scalar("SELECT cook_id FROM orders WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("order"))?;
        // A concurrent revoke/approve holds this row; FOR SHARE waits for it
        // and then reads the committed, current verification status.
        let cook_row =
            sqlx::query(&format!("SELECT {COOK_COLS} FROM cooks WHERE id = $1 FOR SHARE")).bind(cook_id).fetch_one(&mut *tx).await?;
        let cook = cook_from_row(&cook_row)?;
        let order_row =
            sqlx::query(&format!("SELECT {ORDER_COLS} FROM orders WHERE id = $1 FOR UPDATE")).bind(id).fetch_one(&mut *tx).await?;
        let lines = load_lines(&mut tx, id).await?;
        let mut order = order_from_row(&order_row, lines)?;

        f(&cook, &mut order).map_err(|e: DomainError| StoreError::Domain(e))?;

        sqlx::query(
            "UPDATE orders SET status = $2, payment_ref = $3, refund_amount_cents = $4, refund_reason = $5, \
               refunded_at = $6, updated_at = $7 WHERE id = $1",
        )
        .bind(id)
        .bind(order.status.as_str())
        .bind(&order.payment_ref)
        .bind(order.refund.map(|r| r.amount_cents))
        .bind(order.refund.map(|r| r.reason.as_str()))
        .bind(order.refund.map(|r| r.refunded_at))
        .bind(order.updated_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(order)
    }
}
