//! Geospatial discovery primitives.
//!
//! Pure-Rust haversine + bounding box. The Postgres adapter uses the same
//! bounding box as an index prefilter and the same formula for exact distance,
//! so results match the in-memory implementation. Swapping the SQL for PostGIS
//! (`ST_DWithin` on a `geography` column) only touches the persistence crate.

use twoeat_domain::{DomainError, LatLng};

pub const EARTH_RADIUS_M: f64 = 6_371_000.0;
pub const MAX_RADIUS_M: f64 = 25_000.0;
pub const DEFAULT_RADIUS_M: f64 = 3_000.0;
pub const MAX_LIMIT: u32 = 100;

/// Great-circle distance in metres.
pub fn haversine_m(a: LatLng, b: LatLng) -> f64 {
    let (lat1, lat2) = (a.lat().to_radians(), b.lat().to_radians());
    let dlat = lat2 - lat1;
    let dlng = (b.lng() - a.lng()).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lng: f64,
    pub max_lng: f64,
}

impl BoundingBox {
    /// Box that fully contains the circle of `radius_m` around `center`
    /// (valid away from the poles / antimeridian, which is fine for Singapore).
    pub fn around(center: LatLng, radius_m: f64) -> Self {
        let dlat = (radius_m / EARTH_RADIUS_M).to_degrees();
        let dlng = dlat / center.lat().to_radians().cos().max(1e-6);
        Self { min_lat: center.lat() - dlat, max_lat: center.lat() + dlat, min_lng: center.lng() - dlng, max_lng: center.lng() + dlng }
    }

    pub fn contains(&self, p: LatLng) -> bool {
        (self.min_lat..=self.max_lat).contains(&p.lat()) && (self.min_lng..=self.max_lng).contains(&p.lng())
    }
}

/// A validated "who is near me" query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NearbyQuery {
    pub center: LatLng,
    pub radius_m: f64,
    pub limit: u32,
}

impl NearbyQuery {
    pub fn new(center: LatLng, radius_m: Option<f64>, limit: Option<u32>) -> Result<Self, DomainError> {
        let radius_m = radius_m.unwrap_or(DEFAULT_RADIUS_M);
        if !radius_m.is_finite() || radius_m <= 0.0 || radius_m > MAX_RADIUS_M {
            return Err(DomainError::validation(format!("radius_m must be in (0, {MAX_RADIUS_M}]")));
        }
        let limit = limit.unwrap_or(20);
        if limit == 0 || limit > MAX_LIMIT {
            return Err(DomainError::validation(format!("limit must be in 1..={MAX_LIMIT}")));
        }
        Ok(Self { center, radius_m, limit })
    }

    pub fn bounding_box(&self) -> BoundingBox {
        BoundingBox::around(self.center, self.radius_m)
    }
}

/// Filter to the radius, sort nearest-first, truncate to the limit.
pub fn rank_nearby<T>(items: impl IntoIterator<Item = T>, q: &NearbyQuery, loc: impl Fn(&T) -> LatLng) -> Vec<(T, f64)> {
    let mut hits: Vec<(T, f64)> = items
        .into_iter()
        .map(|t| {
            let d = haversine_m(q.center, loc(&t));
            (t, d)
        })
        .filter(|(_, d)| *d <= q.radius_m)
        .collect();
    hits.sort_by(|a, b| a.1.total_cmp(&b.1));
    hits.truncate(q.limit as usize);
    hits
}

/// Distances are shown to eaters rounded up to the nearest 100 m so a cook's
/// exact home position can't be triangulated from the public API.
pub fn coarse_distance_m(d: f64) -> u32 {
    ((d / 100.0).ceil() as u32).max(1) * 100
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(lat: f64, lng: f64) -> LatLng {
        LatLng::new(lat, lng).unwrap()
    }

    #[test]
    fn haversine_known_distance() {
        // One degree of latitude is R * pi / 180 = ~111.19 km.
        let d = haversine_m(p(1.0, 103.8), p(2.0, 103.8));
        assert!((d - 111_194.9).abs() < 5.0, "{d}");
        // Raffles Place -> Changi Airport T1, ~18 km (8.9 km north-south, 15.6 km east-west).
        let d = haversine_m(p(1.2840, 103.8510), p(1.3644, 103.9915));
        assert!((17_800.0..=18_200.0).contains(&d), "{d}");
        assert_eq!(haversine_m(p(1.3, 103.8), p(1.3, 103.8)), 0.0);
    }

    #[test]
    fn bounding_box_contains_circle() {
        let c = p(1.3521, 103.8198);
        let bb = BoundingBox::around(c, 3000.0);
        // A point ~2.9 km due east / north must be inside the box.
        assert!(bb.contains(p(1.3521, 103.8198 + 0.0261)));
        assert!(bb.contains(p(1.3521 + 0.0261, 103.8198)));
        assert!(!bb.contains(p(1.40, 103.8198)));
    }

    #[test]
    fn rank_filters_sorts_and_limits() {
        let q = NearbyQuery::new(p(1.3521, 103.8198), Some(2000.0), Some(2)).unwrap();
        let items =
            vec![("far", p(1.40, 103.82)), ("mid", p(1.360, 103.8198)), ("near", p(1.3525, 103.8198)), ("mid2", p(1.365, 103.8198))];
        let out = rank_nearby(items, &q, |(_, l)| *l);
        let names: Vec<_> = out.iter().map(|((n, _), _)| *n).collect();
        assert_eq!(names, ["near", "mid"]);
    }

    #[test]
    fn query_validation() {
        let c = p(1.35, 103.8);
        assert!(NearbyQuery::new(c, Some(0.0), None).is_err());
        assert!(NearbyQuery::new(c, Some(MAX_RADIUS_M + 1.0), None).is_err());
        assert!(NearbyQuery::new(c, None, Some(0)).is_err());
        assert!(NearbyQuery::new(c, Some(f64::NAN), None).is_err());
        assert_eq!(NearbyQuery::new(c, None, None).unwrap().radius_m, DEFAULT_RADIUS_M);
    }

    #[test]
    fn coarse_distance_rounds_up() {
        assert_eq!(coarse_distance_m(0.0), 100);
        assert_eq!(coarse_distance_m(101.0), 200);
        assert_eq!(coarse_distance_m(1200.0), 1200);
    }
}
