use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use twoeat_domain::{Dish, Fulfillment, Order, OrderPurpose, OrderStatus, RefundReason};
use utoipa::ToSchema;
use uuid::Uuid;

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
}

impl From<Dish> for DishResponse {
    fn from(d: Dish) -> Self {
        Self { id: d.id, cook_id: d.cook_id, name: d.name, description: d.description, price_cents: d.price_cents, is_active: d.is_active }
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
            FulfillmentDto::Delivery { address, postal_code } => {
                Fulfillment::Delivery(twoeat_domain::DeliveryDetails { address, postal_code })
            }
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

// ---- admin ----------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct ApproveCertificationRequest {
    /// Expiry date read from the verified certificate. Must be in the future.
    pub expires_at: DateTime<Utc>,
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
pub struct SubmissionResponse {
    pub cook_id: Uuid,
    pub document_ref: String,
    pub content_type: String,
    pub status: String,
    pub submitted_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
}
