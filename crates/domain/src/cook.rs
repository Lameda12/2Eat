use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{DomainError, RegulatoryViolation};
use crate::location::LatLng;

/// The only certification 2Eat requires from home cooks.
pub const FOOD_SAFETY_COURSE: &str = "Food Safety Course Level 1 (WSQ)";

/// Plain-language conditions shown during cook onboarding and in the terms.
/// The platform never tells cooks they need an SFA licence for a home-based
/// food business; it enforces the real conditions below instead.
///
/// NOTE: wording should be reviewed by counsel before launch.
pub const HBFB_TERMS: &[&str] = &[
    "Home-based food businesses do not need an SFA licence, but you must have completed the Food Safety Course Level 1 (WSQ). 2Eat verifies your certificate before you can list dishes or accept orders.",
    "You must not offer catering, sell food to food retailers, or sell ready-to-eat raw seafood.",
    "You must not put up external signage advertising your home business.",
    "You must not employ anyone who does not live in your household.",
    "Your cooking must not cause significant smoke, grease or strong odours, and the business must remain secondary to residential use.",
    "If your food is not delivered or is not what the customer ordered, the customer receives a full refund, including the platform fee.",
];

/// Cook as specified: `food_safety_cert_expires_at == None` means cannot sell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cook {
    pub id: Uuid,
    pub display_name: String,
    pub food_safety_cert_expires_at: Option<DateTime<Utc>>,
    pub is_verified: bool,
    pub location: LatLng,
}

/// Explicit acknowledgement of every Home-Based Business Scheme condition we
/// cannot enforce in code (signage, non-household employees) plus the ones we do.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct HbfbAcknowledgements {
    pub no_external_signage: bool,
    pub no_non_household_employees: bool,
    pub no_catering_or_retail_sales: bool,
    pub no_raw_seafood: bool,
    pub refund_policy: bool,
}

impl HbfbAcknowledgements {
    pub fn all() -> Self {
        Self {
            no_external_signage: true,
            no_non_household_employees: true,
            no_catering_or_retail_sales: true,
            no_raw_seafood: true,
            refund_policy: true,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.no_external_signage
            && self.no_non_household_employees
            && self.no_catering_or_retail_sales
            && self.no_raw_seafood
            && self.refund_policy
    }
}

impl Cook {
    /// Onboarding. A new cook is unverified and cannot sell until an admin
    /// approves their certificate document.
    pub fn register(id: Uuid, display_name: &str, location: LatLng, acks: &HbfbAcknowledgements) -> Result<Self, DomainError> {
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.chars().count() > 80 {
            return Err(DomainError::validation("display_name must be 1-80 characters"));
        }
        if !location.is_in_singapore() {
            return Err(DomainError::validation("location must be within Singapore"));
        }
        if !acks.is_complete() {
            return Err(RegulatoryViolation::HbfbTermsNotAccepted.into());
        }
        Ok(Self { id, display_name: display_name.to_owned(), food_safety_cert_expires_at: None, is_verified: false, location })
    }

    /// Why this cook may not sell right now, if they may not.
    pub fn ensure_can_sell(&self, now: DateTime<Utc>) -> Result<(), RegulatoryViolation> {
        if !self.is_verified {
            return Err(RegulatoryViolation::CertificationNotVerified);
        }
        match self.food_safety_cert_expires_at {
            None => Err(RegulatoryViolation::CertificationMissing),
            Some(exp) if exp <= now => Err(RegulatoryViolation::CertificationExpired),
            Some(_) => Ok(()),
        }
    }

    /// `true` only when the certificate is verified and unexpired at `now`.
    pub fn can_sell(&self, now: DateTime<Utc>) -> bool {
        self.ensure_can_sell(now).is_ok()
    }

    /// Admin approved the uploaded certificate.
    pub fn approve_certification(&mut self, expires_at: DateTime<Utc>, now: DateTime<Utc>) -> Result<(), DomainError> {
        if expires_at <= now {
            return Err(DomainError::validation("certificate expiry must be in the future"));
        }
        self.food_safety_cert_expires_at = Some(expires_at);
        self.is_verified = true;
        Ok(())
    }

    /// Pull the cook off the platform (e.g. certificate found invalid).
    pub fn revoke_verification(&mut self) {
        self.is_verified = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificationStatus {
    Pending,
    Approved,
    Rejected,
}

impl CertificationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            _ => return None,
        })
    }
}

/// An uploaded certificate document awaiting (or past) human review.
#[derive(Debug, Clone, PartialEq)]
pub struct CertificationSubmission {
    pub id: Uuid,
    pub cook_id: Uuid,
    pub document_ref: String,
    pub content_type: String,
    pub status: CertificationStatus,
    pub submitted_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
}

impl CertificationSubmission {
    pub fn new(cook_id: Uuid, document_ref: String, content_type: String, now: DateTime<Utc>) -> Result<Self, DomainError> {
        if document_ref.is_empty() {
            return Err(DomainError::validation("certificate document is required"));
        }
        Ok(Self {
            id: Uuid::new_v4(),
            cook_id,
            document_ref,
            content_type,
            status: CertificationStatus::Pending,
            submitted_at: now,
            reviewed_at: None,
        })
    }
}

/// Outcome of a human review of a certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertDecision {
    Approve { expires_at: DateTime<Utc> },
    Reject,
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn cook() -> Cook {
        Cook::register(Uuid::new_v4(), "Auntie Mei", LatLng::new_in_singapore(1.3521, 103.8198).unwrap(), &HbfbAcknowledgements::all())
            .unwrap()
    }

    #[test]
    fn new_cook_cannot_sell() {
        let c = cook();
        assert!(!c.can_sell(Utc::now()));
        assert_eq!(c.ensure_can_sell(Utc::now()), Err(RegulatoryViolation::CertificationNotVerified));
    }

    #[test]
    fn verified_with_future_expiry_can_sell_until_expiry() {
        let now = Utc::now();
        let mut c = cook();
        c.approve_certification(now + Duration::days(30), now).unwrap();
        assert!(c.can_sell(now));
        assert!(c.can_sell(now + Duration::days(29)));
        assert!(!c.can_sell(now + Duration::days(30)), "expiry instant is exclusive");
        assert_eq!(c.ensure_can_sell(now + Duration::days(31)), Err(RegulatoryViolation::CertificationExpired));
    }

    #[test]
    fn missing_cert_blocks_even_if_flagged_verified() {
        let mut c = cook();
        c.is_verified = true;
        assert_eq!(c.ensure_can_sell(Utc::now()), Err(RegulatoryViolation::CertificationMissing));
    }

    #[test]
    fn unverified_blocks_even_with_valid_dates() {
        let now = Utc::now();
        let mut c = cook();
        c.food_safety_cert_expires_at = Some(now + Duration::days(365));
        assert!(!c.can_sell(now));
    }

    #[test]
    fn revoke_stops_selling() {
        let now = Utc::now();
        let mut c = cook();
        c.approve_certification(now + Duration::days(30), now).unwrap();
        c.revoke_verification();
        assert!(!c.can_sell(now));
    }

    #[test]
    fn cannot_approve_already_expired_cert() {
        let now = Utc::now();
        assert!(cook().approve_certification(now - Duration::days(1), now).is_err());
    }

    #[test]
    fn registration_requires_all_acknowledgements() {
        let loc = LatLng::new_in_singapore(1.35, 103.82).unwrap();
        let mut acks = HbfbAcknowledgements::all();
        acks.no_external_signage = false;
        assert_eq!(
            Cook::register(Uuid::new_v4(), "X", loc, &acks),
            Err(DomainError::RegulatoryViolation(RegulatoryViolation::HbfbTermsNotAccepted))
        );
    }

    #[test]
    fn registration_rejects_blank_name_and_overseas_location() {
        let sg = LatLng::new_in_singapore(1.35, 103.82).unwrap();
        assert!(Cook::register(Uuid::new_v4(), "  ", sg, &HbfbAcknowledgements::all()).is_err());
        let sf = LatLng::new(37.77, -122.42).unwrap();
        assert!(Cook::register(Uuid::new_v4(), "Cook", sf, &HbfbAcknowledgements::all()).is_err());
    }

    #[test]
    fn terms_never_claim_an_sfa_licence_is_required() {
        let all = HBFB_TERMS.join(" ").to_lowercase();
        assert!(all.contains("do not need an sfa licence"));
        assert!(all.contains("external signage"));
        assert!(all.contains("live in your household"));
    }
}
