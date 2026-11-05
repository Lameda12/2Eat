use serde::{Deserialize, Serialize};

use crate::error::DomainError;

/// WGS84 coordinate. Constructed only through [`LatLng::new`] (also on deserialize).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawLatLng")]
pub struct LatLng {
    lat: f64,
    lng: f64,
}

#[derive(Deserialize)]
struct RawLatLng {
    lat: f64,
    lng: f64,
}

impl TryFrom<RawLatLng> for LatLng {
    type Error = DomainError;
    fn try_from(r: RawLatLng) -> Result<Self, Self::Error> {
        LatLng::new(r.lat, r.lng)
    }
}

// Approximate bounding box of Singapore incl. Tuas, Changi, Pulau Ubin/Tekong.
const SG_LAT: (f64, f64) = (1.13, 1.48);
const SG_LNG: (f64, f64) = (103.59, 104.09);

impl LatLng {
    pub fn new(lat: f64, lng: f64) -> Result<Self, DomainError> {
        if !lat.is_finite() || !lng.is_finite() || !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
            return Err(DomainError::validation("coordinates out of range"));
        }
        Ok(Self { lat, lng })
    }

    /// Like [`new`](Self::new) but also requires the point to be inside Singapore.
    pub fn new_in_singapore(lat: f64, lng: f64) -> Result<Self, DomainError> {
        let p = Self::new(lat, lng)?;
        if !p.is_in_singapore() {
            return Err(DomainError::validation("location must be within Singapore"));
        }
        Ok(p)
    }

    pub fn is_in_singapore(&self) -> bool {
        (SG_LAT.0..=SG_LAT.1).contains(&self.lat) && (SG_LNG.0..=SG_LNG.1).contains(&self.lng)
    }

    pub fn lat(&self) -> f64 {
        self.lat
    }
    pub fn lng(&self) -> f64 {
        self.lng
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_ranges_and_singapore() {
        assert!(LatLng::new(91.0, 0.0).is_err());
        assert!(LatLng::new(f64::NAN, 0.0).is_err());
        assert!(LatLng::new_in_singapore(1.3521, 103.8198).is_ok());
        assert!(LatLng::new_in_singapore(37.77, -122.42).is_err());
    }

    #[test]
    fn deserialize_validates() {
        assert!(serde_json::from_str::<LatLng>(r#"{"lat":1.35,"lng":103.8}"#).is_ok());
        assert!(serde_json::from_str::<LatLng>(r#"{"lat":123.0,"lng":103.8}"#).is_err());
    }
}
