//! Pure domain logic for 2Eat (Singapore home-based food business marketplace).
//!
//! No I/O, no clocks (time is always passed in), no framework types. Every
//! regulatory rule lives here so no adapter can bypass it.

pub mod cook;
pub mod dish;
pub mod error;
pub mod location;
pub mod order;

pub use cook::{CertDecision, CertificationStatus, CertificationSubmission, Cook, FOOD_SAFETY_COURSE, HBFB_TERMS, HbfbAcknowledgements};
pub use dish::{Dish, NewDish};
pub use error::{DomainError, RegulatoryViolation};
pub use location::LatLng;
pub use order::{
    DeliveryDetails, Fulfillment, NewOrder, Order, OrderLine, OrderPurpose, OrderStatus, PricingPolicy, Refund, RefundReason, RequestedLine,
};
