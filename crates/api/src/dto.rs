use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use twoeat_domain::{
    CertificationSubmission, Dish, DishReview, Fulfillment, Order, OrderPurpose, OrderStatus, RefundReason, RefundRecord, RejectionReason,
    Role,
};
use twoeat_persistence::AuditRecord;
use utoipa::ToSchema;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

// ---- auth -----------------------------------------------------------------

#[derive(Deserialize, ToSchema, Zeroize, ZeroizeOnDrop)]
pub struct RegisterRequest {
    #[zeroize(skip)]
    pub email: String,
    /// At least 10 characters.
    #[schema(format = Password)]
    pub password: String,
    /// `eater` or `cook`. Admin accounts cannot be self-registered.
    #[zeroize(skip)]
    #[schema(value_type = String, example = "eater")]
    pub role: Role,
}

#[derive(Deserialize, ToSchema, Zeroize, ZeroizeOnDrop)]
pub struct LoginRequest {
    #[zeroize(skip)]
    pub email: String,
    #[schema(format = Password)]
    pub password: String,
}

#[derive(Serialize, ToSchema)]
pub struct UserSummary {
    pub id: Uuid,
    #[schema(value_type = String, example = "eater")]
    pub role: Role,
}

/// Returned once, on login/refresh. Holds the plaintext token, so it is wiped on drop.
#[derive(Serialize, ToSchema, Zeroize, ZeroizeOnDrop)]
pub struct TokenResponse {
    /// Opaque bearer token. Send as `Authorization: Bearer <token>`. Shown only once.
    pub token: String,
    #[zeroize(skip)]
    pub expires_at: DateTime<Utc>,
    #[zeroize(skip)]
    pub user: UserSummary,
}

#[derive(Serialize, ToSchema)]
pub struct MessageResponse {
    pub message: String,
}

// ---- pagination -----------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
pub struct PageParams {
    /// Default 20, max 100.
    pub limit: Option<i64>,
    /// Default 0.
    pub offset: Option<i64>,
}

impl PageParams {
    pub fn resolve(&self) -> Result<(i64, i64), crate::error::ApiError> {
        let (limit, offset) = (self.limit.unwrap_or(20), self.offset.unwrap_or(0));
        if !(1..=100).contains(&limit) || offset < 0 {
            return Err(crate::error::ApiError::BadRequest("limit must be 1..=100 and offset >= 0".into()));
        }
        Ok((limit, offset))
    }
}

macro_rules! page_type {
    ($name:ident, $item:ty) => {
        #[derive(Serialize, ToSchema)]
        pub struct $name {
            pub items: Vec<$item>,
            pub total: i64,
            pub limit: i64,
            pub offset: i64,
        }
    };
}

// ---- cooks ----------------------------------------------------------------

/// `multipart/form-data` body of `POST /cooks/register`.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct RegisterCookForm {
    pub display_name: String,
    /// Home latitude (must be inside Singapore). Never shown publicly.
    pub lat: f64,
    /// Home longitude (must be inside Singapore). Never shown publicly.
    pub lng: f64,
    /// Must be `true`: confirms the conditions from `GET /cooks/onboarding-terms`.
    pub accept_hbfb_terms: bool,
    /// Food Safety Course Level 1 (WSQ) certificate: PDF, PNG or JPEG, max 5 MB.
    #[schema(value_type = String, format = Binary)]
    pub cert_document: Vec<u8>,
}

#[derive(Serialize, ToSchema)]
pub struct RegisterCookResponse {
    pub cook_id: Uuid,
    pub is_verified: bool,
    pub can_sell: bool,
    pub certification_status: String,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct OnboardingTerms {
    pub required_certification: String,
    pub conditions: Vec<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct NearbyParams {
    pub lat: f64,
    pub lng: f64,
    /// Search radius in metres (default 3000, max 25000).
    pub radius_m: Option<f64>,
    /// Max results (default 20, max 100).
    pub limit: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub struct NearbyCookResponse {
    pub id: Uuid,
    pub display_name: String,
    /// Distance rounded up to the nearest 100 m; exact home location is never exposed.
    pub distance_m: u32,
}

// ---- dishes ---------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct CreateDishRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Price in SGD cents.
    pub price_cents: i64,
    /// Declared flags. `true` is always rejected for home-based food businesses.
    #[serde(default)]
    pub is_catering: bool,
    #[serde(default)]
    pub is_raw_seafood: bool,
}

#[derive(Serialize, ToSchema)]
pub struct DishResponse {
    pub id: Uuid,
    pub cook_id: Uuid,
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    pub is_active: bool,
    /// `clear`, `pending_review` (flagged by the raw-seafood keyword scan; not listed until an admin approves), `approved` or `rejected`.
    pub review_status: String,
    /// Keyword(s) that triggered the review, shown to the cook and the reviewer.
    pub review_flag: Option<String>,
}

impl From<Dish> for DishResponse {
    fn from(d: Dish) -> Self {
        Self {
            id: d.id,
            cook_id: d.cook_id,
            name: d.name,
            description: d.description,
            price_cents: d.price_cents,
            is_active: d.is_active,
            review_status: d.review.as_str().to_owned(),
            review_flag: d.review_flag,
        }
    }
}

impl DishResponse {
    pub fn is_pending(&self) -> bool {
        self.review_status == DishReview::PendingReview.as_str()
    }
}

// ---- orders ---------------------------------------------------------------

#[derive(Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FulfillmentDto {
    Pickup,
    Delivery { address: String, postal_code: String },
}

impl From<FulfillmentDto> for Fulfillment {
    fn from(f: FulfillmentDto) -> Self {
        match f {
            FulfillmentDto::Pickup => Fulfillment::Pickup,
            FulfillmentDto::Delivery { address, postal_code } => Fulfillment::Delivery(twoeat_domain::DeliveryDetails {
                address,
                postal_code,
                delivered_by: twoeat_domain::DeliveredBy::Cook,
            }),
        }
    }
}

impl From<&Fulfillment> for FulfillmentDto {
    fn from(f: &Fulfillment) -> Self {
        match f {
            Fulfillment::Pickup => FulfillmentDto::Pickup,
            Fulfillment::Delivery(d) => FulfillmentDto::Delivery { address: d.address.clone(), postal_code: d.postal_code.clone() },
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct OrderItemRequest {
    pub dish_id: Uuid,
    pub quantity: u32,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateOrderRequest {
    pub cook_id: Uuid,
    pub items: Vec<OrderItemRequest>,
    pub fulfillment: FulfillmentDto,
    /// What the food is for. Anything but `personal_consumption` is rejected.
    #[serde(default)]
    #[schema(value_type = String, example = "personal_consumption")]
    pub purpose: OrderPurpose,
}

#[derive(Deserialize, ToSchema)]
pub struct RefundRequest {
    #[schema(value_type = String, example = "unfulfilled")]
    pub reason: RefundReason,
}

#[derive(Serialize, ToSchema)]
pub struct OrderItemResponse {
    pub dish_id: Uuid,
    pub name: String,
    pub unit_price_cents: i64,
    pub quantity: u32,
}

#[derive(Serialize, ToSchema)]
pub struct RefundResponse {
    /// Always the full order total, platform fee included.
    pub amount_cents: i64,
    #[schema(value_type = String)]
    pub reason: RefundReason,
    pub refunded_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct OrderResponse {
    pub id: Uuid,
    pub eater_id: Uuid,
    pub cook_id: Uuid,
    #[schema(value_type = String, example = "placed")]
    pub status: OrderStatus,
    pub fulfillment: FulfillmentDto,
    pub items: Vec<OrderItemResponse>,
    pub subtotal_cents: i64,
    pub platform_fee_cents: i64,
    pub delivery_fee_cents: i64,
    pub total_cents: i64,
    pub refund: Option<RefundResponse>,
    pub placed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Order> for OrderResponse {
    fn from(o: Order) -> Self {
        Self {
            id: o.id,
            eater_id: o.eater_id,
            cook_id: o.cook_id,
            status: o.status,
            fulfillment: (&o.fulfillment).into(),
            items: o
                .lines
                .into_iter()
                .map(|l| OrderItemResponse {
                    dish_id: l.dish_id,
                    name: l.dish_name,
                    unit_price_cents: l.unit_price_cents,
                    quantity: l.quantity,
                })
                .collect(),
            subtotal_cents: o.subtotal_cents,
            platform_fee_cents: o.platform_fee_cents,
            delivery_fee_cents: o.delivery_fee_cents,
            total_cents: o.total_cents,
            refund: o.refund.map(|r| RefundResponse { amount_cents: r.amount_cents, reason: r.reason, refunded_at: r.refunded_at }),
            placed_at: o.placed_at,
            updated_at: o.updated_at,
        }
    }
}

// ---- refunds --------------------------------------------------------------

/// Result of `POST /orders/{id}/refund`.
#[derive(Serialize, ToSchema)]
pub struct RefundAccepted {
    /// `approved`: the full total (platform and delivery fee included) is being / has been returned.
    /// `pending_review`: an admin must decide; no money moves until then.
    pub status: String,
    pub refund_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct RefundItem {
    pub refund_id: Uuid,
    pub order_id: Uuid,
    pub initiated_by: Uuid,
    #[schema(value_type = String)]
    pub reason: RefundReason,
    pub amount_cents: i64,
    /// `pending_review`, `approved` or `denied`.
    pub status: String,
    /// `auto`, `admin_approved` or `admin_denied`; absent while pending.
    pub decision: Option<String>,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<RefundRecord> for RefundItem {
    fn from(r: RefundRecord) -> Self {
        Self {
            refund_id: r.id,
            order_id: r.order_id,
            initiated_by: r.initiated_by,
            reason: r.reason,
            amount_cents: r.amount_cents,
            status: r.status().as_str().to_owned(),
            decision: r.decision.map(|d| d.as_str().to_owned()),
            decided_by: r.decided_by,
            decided_at: r.decided_at,
            created_at: r.created_at,
        }
    }
}

page_type!(RefundPage, RefundItem);

#[derive(Deserialize, utoipa::IntoParams)]
pub struct RefundListParams {
    /// `pending_review` (default), `approved` or `denied`.
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ---- admin ----------------------------------------------------------------

#[derive(Deserialize, ToSchema, Default)]
pub struct NotesRequest {
    pub notes: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct ApproveCertificationRequest {
    /// Expiry date read from the verified certificate. Must be in the future.
    pub expires_at: DateTime<Utc>,
    pub notes: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct RejectCertificationRequest {
    /// One of `illegible_document`, `expired_certificate`, `wrong_certificate_type`, `name_mismatch`,
    /// `suspected_fraud`, or `{"other": "free text"}`.
    #[schema(value_type = Object, example = "illegible_document")]
    pub reason: RejectionReason,
    pub notes: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CookStatusResponse {
    pub cook_id: Uuid,
    pub display_name: String,
    pub is_verified: bool,
    pub food_safety_cert_expires_at: Option<DateTime<Utc>>,
    pub can_sell: bool,
}

#[derive(Serialize, ToSchema)]
pub struct ReviewItem {
    pub review_id: Uuid,
    pub cook_id: Uuid,
    pub document_ref: String,
    pub content_type: String,
    /// `pending`, `approved` or `rejected`.
    pub status: String,
    pub submitted_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub reviewer_id: Option<Uuid>,
    #[schema(value_type = Option<Object>)]
    pub rejection_reason: Option<RejectionReason>,
    pub notes: Option<String>,
}

impl From<CertificationSubmission> for ReviewItem {
    fn from(s: CertificationSubmission) -> Self {
        Self {
            review_id: s.id,
            cook_id: s.cook_id,
            document_ref: s.document_ref,
            content_type: s.content_type,
            status: s.status.as_str().to_owned(),
            submitted_at: s.submitted_at,
            reviewed_at: s.reviewed_at,
            reviewer_id: s.reviewer_id,
            rejection_reason: s.rejection_reason,
            notes: s.notes,
        }
    }
}

page_type!(ReviewPage, ReviewItem);

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ReviewListParams {
    /// `pending`, `approved` or `rejected`. Omit for all.
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

page_type!(DishPage, DishResponse);

#[derive(Deserialize, utoipa::IntoParams)]
pub struct AuditParams {
    pub target_type: Option<String>,
    pub target_id: Option<Uuid>,
    pub action: Option<String>,
    /// Default 50, max 200.
    pub limit: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct AuditItem {
    pub id: Uuid,
    pub actor_id: Option<Uuid>,
    pub action: String,
    pub target_type: String,
    pub target_id: Uuid,
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl From<AuditRecord> for AuditItem {
    fn from(a: AuditRecord) -> Self {
        Self {
            id: a.id,
            actor_id: a.entry.actor_id,
            action: a.entry.action,
            target_type: a.entry.target_type,
            target_id: a.entry.target_id,
            metadata: a.entry.metadata,
            created_at: a.entry.at,
        }
    }
}
