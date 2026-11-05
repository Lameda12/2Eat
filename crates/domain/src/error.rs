use thiserror::Error;

use crate::order::OrderStatus;

/// A specific Singapore HBFB rule that was broken. Stable `code()`s are part of
/// the public API contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RegulatoryViolation {
    #[error("the cook's Food Safety Course Level 1 (WSQ) certificate has not been verified")]
    CertificationNotVerified,
    #[error("the cook has no Food Safety Course Level 1 (WSQ) certificate on record")]
    CertificationMissing,
    #[error("the cook's Food Safety Course Level 1 (WSQ) certificate has expired")]
    CertificationExpired,
    #[error("catering is not permitted for home-based food businesses")]
    Catering,
    #[error("selling to food retailers is not permitted for home-based food businesses")]
    SaleToFoodRetailer,
    #[error("ready-to-eat raw seafood may not be sold by home-based food businesses")]
    RawSeafood,
    #[error("the home-based food business conditions must be accepted before registering")]
    HbfbTermsNotAccepted,
}

impl RegulatoryViolation {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CertificationNotVerified => "certification_not_verified",
            Self::CertificationMissing => "certification_missing",
            Self::CertificationExpired => "certification_expired",
            Self::Catering => "catering_prohibited",
            Self::SaleToFoodRetailer => "sale_to_food_retailer_prohibited",
            Self::RawSeafood => "raw_seafood_prohibited",
            Self::HbfbTermsNotAccepted => "hbfb_terms_not_accepted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    #[error("regulatory violation: {0}")]
    RegulatoryViolation(RegulatoryViolation),
    #[error("invalid input: {0}")]
    Validation(String),
    #[error("cannot {action} an order that is {from:?}")]
    InvalidTransition { from: OrderStatus, action: &'static str },
}

impl DomainError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
}

impl From<RegulatoryViolation> for DomainError {
    fn from(v: RegulatoryViolation) -> Self {
        Self::RegulatoryViolation(v)
    }
}
