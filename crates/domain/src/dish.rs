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
    pub review: DishReview,
    /// Keyword(s) that triggered `PendingReview`, for the admin reviewer.
    pub review_flag: Option<String>,
}

/// Admin review state. A dish whose name/description matched a raw-seafood
/// keyword is created inactive and `PendingReview` until an admin decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DishReview {
    Clear,
    PendingReview,
    Approved,
    Rejected,
}

impl DishReview {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::PendingReview => "pending_review",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "clear" => Self::Clear,
            "pending_review" => Self::PendingReview,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewDish {
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    pub is_catering: bool,
    pub is_raw_seafood: bool,
}

/// Words and phrases that make a dish **flag for admin review** (never a hard
/// block on their own; only an explicit `is_raw_seafood: true` is rejected).
///
/// **This list is non-exhaustive.** It is a cheap tripwire for the common
/// cases, not a legal test: absence of a match does not mean a dish is allowed.
/// Matching is case-insensitive on whole words/phrases.
pub const RAW_SEAFOOD_REVIEW_TERMS: &[&str] = &[
    "sashimi",
    "ceviche",
    "crudo",
    "carpaccio",
    "tartare",
    "poke",
    "oysters",
    "raw oyster",
    "raw fish",
    "raw salmon",
    "raw tuna",
    "raw prawn",
    "raw shrimp",
    "raw scallop",
    "raw seafood",
];

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Whole-word/phrase containment (`poke` matches "poke bowl", not "spoken").
fn contains_term(haystack: &str, term: &str) -> bool {
    let mut from = 0;
    while let Some(i) = haystack[from..].find(term) {
        let start = from + i;
        let end = start + term.len();
        let before_ok = haystack[..start].chars().next_back().is_none_or(|c| !is_word_char(c));
        let after_ok = haystack[end..].chars().next().is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Review-list terms found in `text`.
pub(crate) fn raw_seafood_terms_in(text: &str) -> Vec<&'static str> {
    let t = text.to_lowercase();
    RAW_SEAFOOD_REVIEW_TERMS.iter().copied().filter(|term| contains_term(&t, term)).collect()
}

impl Dish {
    /// Create a dish. Fails with `RegulatoryViolation` when the cook may not
    /// sell or the dish is *declared* catering / raw seafood. A keyword match
    /// alone does not fail: the dish is created inactive and flagged for review.
    pub fn create(cook: &Cook, new: NewDish, now: DateTime<Utc>) -> Result<Self, DomainError> {
        cook.ensure_can_sell(now)?;

        if new.is_catering {
            return Err(RegulatoryViolation::Catering.into());
        }
        if new.is_raw_seafood {
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

        let mut terms = raw_seafood_terms_in(name);
        for t in raw_seafood_terms_in(&new.description) {
            if !terms.contains(&t) {
                terms.push(t);
            }
        }
        let flagged = !terms.is_empty();

        Ok(Self {
            id: Uuid::new_v4(),
            cook_id: cook.id,
            name: name.to_owned(),
            description: new.description.trim().to_owned(),
            price_cents: new.price_cents,
            is_catering: false,
            is_raw_seafood: false,
            is_active: !flagged,
            review: if flagged { DishReview::PendingReview } else { DishReview::Clear },
            review_flag: flagged.then(|| terms.join(", ")),
        })
    }
}

impl Dish {
    /// Admin cleared a flagged dish: it becomes listable.
    pub fn approve_review(&mut self) -> Result<(), DomainError> {
        if self.review != DishReview::PendingReview {
            return Err(DomainError::validation("dish is not pending review"));
        }
        self.review = DishReview::Approved;
        self.is_active = true;
        Ok(())
    }

    /// Admin upheld the flag: the dish stays unlisted.
    pub fn reject_review(&mut self) -> Result<(), DomainError> {
        if self.review != DishReview::PendingReview {
            return Err(DomainError::validation("dish is not pending review"));
        }
        self.review = DishReview::Rejected;
        self.is_active = false;
        Ok(())
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
    fn explicit_raw_seafood_flag_is_a_hard_violation() {
        let now = Utc::now();
        let flagged = Dish::create(&selling_cook(now), NewDish { is_raw_seafood: true, ..new_dish() }, now);
        assert_eq!(flagged.unwrap_err(), DomainError::RegulatoryViolation(RegulatoryViolation::RawSeafood));
        // Even with an innocuous name.
        let n = NewDish { name: "Chicken rice".into(), is_raw_seafood: true, ..new_dish() };
        assert!(Dish::create(&selling_cook(now), n, now).is_err());
    }

    #[test]
    fn keywords_flag_for_review_instead_of_rejecting() {
        let now = Utc::now();
        let cook = selling_cook(now);
        for (name, desc) in [
            ("Salmon Sashimi platter", ""),
            ("Fresh dish", "RAW oysters on ice"),
            ("Tuna Poke Bowl", ""),
            ("Beef Carpaccio", ""),
            ("Prawn crudo", ""),
            ("Steak tartare", ""),
            ("Dozen oysters", ""),
        ] {
            let d = Dish::create(&cook, NewDish { name: name.into(), description: desc.into(), ..new_dish() }, now)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(d.review, DishReview::PendingReview, "{name}");
            assert!(!d.is_active, "{name}: flagged dishes are not listed until approved");
            assert!(d.review_flag.is_some());
            assert!(!d.is_raw_seafood, "the declared flag stays false; only the declaration is authoritative");
        }
    }

    #[test]
    fn keyword_matching_is_whole_word() {
        let now = Utc::now();
        let cook = selling_cook(now);
        // "spoken", "pokemon", "oyster sauce" must not trip the scan.
        for name in ["Spoken word noodles", "Pokemon cake", "Oyster sauce chicken", "Smoked duck"] {
            let d = Dish::create(&cook, NewDish { name: name.into(), ..new_dish() }, now).unwrap();
            assert_eq!(d.review, DishReview::Clear, "{name}");
            assert!(d.is_active);
        }
    }

    #[test]
    fn review_decisions() {
        let now = Utc::now();
        let cook = selling_cook(now);
        let mk = || Dish::create(&cook, NewDish { name: "Poke bowl".into(), ..new_dish() }, now).unwrap();
        let mut d = mk();
        d.approve_review().unwrap();
        assert!(d.is_active && d.review == DishReview::Approved);
        assert!(d.approve_review().is_err(), "already decided");
        let mut d = mk();
        d.reject_review().unwrap();
        assert!(!d.is_active && d.review == DishReview::Rejected);
        let mut clear = Dish::create(&cook, new_dish(), now).unwrap();
        assert!(clear.approve_review().is_err());
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
