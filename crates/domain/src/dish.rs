use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::cook::Cook;
use crate::error::{DomainError, RegulatoryViolation};

#[derive(Debug, Clone, PartialEq)]
pub struct Dish {
    pub id: Uuid,
    pub cook_id: Uuid,
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    /// Must always be false for HBFBs; kept on the record for auditability.
    pub is_catering: bool,
    /// Must always be false for HBFBs.
    pub is_raw_seafood: bool,
    pub is_active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewDish {
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    pub is_catering: bool,
    pub is_raw_seafood: bool,
}

/// Defence-in-depth for the self-declared `is_raw_seafood` flag. Deliberately
/// narrow to limit false positives; it is a heuristic, not a legal test.
const RAW_SEAFOOD_MARKERS: &[&str] = &[
    "sashimi",
    "ceviche",
    "crudo",
    "raw fish",
    "raw salmon",
    "raw tuna",
    "raw oyster",
    "raw prawn",
    "raw shrimp",
    "raw scallop",
    "raw seafood",
    "tuna tartare",
    "salmon tartare",
    "fish tartare",
];

pub(crate) fn mentions_raw_seafood(text: &str) -> bool {
    let t = text.to_lowercase();
    RAW_SEAFOOD_MARKERS.iter().any(|m| t.contains(m))
}

impl Dish {
    /// Create a dish. Fails with `RegulatoryViolation` when the cook may not
    /// sell, or when the dish is catering / ready-to-eat raw seafood.
    pub fn create(cook: &Cook, new: NewDish, now: DateTime<Utc>) -> Result<Self, DomainError> {
        cook.ensure_can_sell(now)?;

        if new.is_catering {
            return Err(RegulatoryViolation::Catering.into());
        }
        if new.is_raw_seafood || mentions_raw_seafood(&new.name) || mentions_raw_seafood(&new.description) {
            return Err(RegulatoryViolation::RawSeafood.into());
        }

        let name = new.name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(DomainError::validation("name must be 1-120 characters"));
        }
        if new.description.chars().count() > 1000 {
            return Err(DomainError::validation("description must be at most 1000 characters"));
        }
        if new.price_cents <= 0 {
            return Err(DomainError::validation("price_cents must be positive"));
        }

        Ok(Self {
            id: Uuid::new_v4(),
            cook_id: cook.id,
            name: name.to_owned(),
            description: new.description.trim().to_owned(),
            price_cents: new.price_cents,
            is_catering: false,
            is_raw_seafood: false,
            is_active: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::{cook::HbfbAcknowledgements, location::LatLng};

    fn selling_cook(now: DateTime<Utc>) -> Cook {
        let mut c =
            Cook::register(Uuid::new_v4(), "Cook", LatLng::new_in_singapore(1.35, 103.82).unwrap(), &HbfbAcknowledgements::all()).unwrap();
        c.approve_certification(now + Duration::days(90), now).unwrap();
        c
    }

    fn new_dish() -> NewDish {
        NewDish {
            name: "Chicken rice".into(),
            description: "Poached chicken, fragrant rice".into(),
            price_cents: 650,
            is_catering: false,
            is_raw_seafood: false,
        }
    }

    #[test]
    fn creates_valid_dish() {
        let now = Utc::now();
        let cook = selling_cook(now);
        let d = Dish::create(&cook, new_dish(), now).unwrap();
        assert_eq!(d.cook_id, cook.id);
        assert!(d.is_active && !d.is_catering && !d.is_raw_seafood);
    }

    #[test]
    fn rejects_catering_flag() {
        let now = Utc::now();
        let err = Dish::create(&selling_cook(now), NewDish { is_catering: true, ..new_dish() }, now).unwrap_err();
        assert_eq!(err, DomainError::RegulatoryViolation(RegulatoryViolation::Catering));
    }

    #[test]
    fn rejects_raw_seafood_flag_and_keywords() {
        let now = Utc::now();
        let cook = selling_cook(now);
        let flagged = Dish::create(&cook, NewDish { is_raw_seafood: true, ..new_dish() }, now);
        assert_eq!(flagged.unwrap_err(), DomainError::RegulatoryViolation(RegulatoryViolation::RawSeafood));

        let by_name = Dish::create(&cook, NewDish { name: "Salmon Sashimi platter".into(), ..new_dish() }, now);
        assert_eq!(by_name.unwrap_err(), DomainError::RegulatoryViolation(RegulatoryViolation::RawSeafood));

        let by_desc = Dish::create(&cook, NewDish { description: "Fresh RAW oysters on ice".into(), ..new_dish() }, now);
        assert_eq!(by_desc.unwrap_err(), DomainError::RegulatoryViolation(RegulatoryViolation::RawSeafood));
    }

    #[test]
    fn cooked_seafood_is_fine() {
        let now = Utc::now();
        let d = NewDish { name: "Chilli crab".into(), description: "Wok-fried mud crab".into(), ..new_dish() };
        assert!(Dish::create(&selling_cook(now), d, now).is_ok());
    }

    #[test]
    fn uncertified_or_expired_cook_cannot_list() {
        let now = Utc::now();
        let mut cook = selling_cook(now);
        assert!(Dish::create(&cook, new_dish(), now + Duration::days(91)).is_err());
        cook.revoke_verification();
        assert_eq!(
            Dish::create(&cook, new_dish(), now).unwrap_err(),
            DomainError::RegulatoryViolation(RegulatoryViolation::CertificationNotVerified)
        );
    }

    #[test]
    fn rejects_bad_price_and_name() {
        let now = Utc::now();
        let cook = selling_cook(now);
        assert!(Dish::create(&cook, NewDish { price_cents: 0, ..new_dish() }, now).is_err());
        assert!(Dish::create(&cook, NewDish { name: " ".into(), ..new_dish() }, now).is_err());
    }
}
