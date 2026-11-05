use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use twoeat_domain::LatLng;
use twoeat_geo::{EARTH_RADIUS_M, NearbyQuery};

use crate::{NearbyCook, StoreError, StoreResult};

/// Geospatial discovery port. Implementations must agree on results (see the
/// parity test) and must never expose exact coordinates.
#[async_trait]
pub trait CookLocationRepo: Send + Sync {
    /// Cooks that are verified with an unexpired certificate at `now`, within
    /// `radius_km` of `center`, nearest first, at most `limit`. Distances are exact
    /// metres here; rounding for display is the API's job.
    async fn find_nearby(&self, center: LatLng, radius_km: f64, limit: u32, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>>;
}

pub(crate) fn query_of(center: LatLng, radius_km: f64, limit: u32) -> StoreResult<NearbyQuery> {
    NearbyQuery::new(center, Some(radius_km * 1000.0), Some(limit)).map_err(StoreError::from)
}

fn map_rows(rows: Vec<sqlx::postgres::PgRow>) -> StoreResult<Vec<NearbyCook>> {
    rows.iter()
        .map(|r| Ok(NearbyCook { id: r.try_get("id")?, display_name: r.try_get("display_name")?, distance_m: r.try_get("dist")? }))
        .collect()
}

/// Default implementation: bounding box on the `(lat, lng)` index, then haversine in SQL.
#[derive(Clone)]
pub struct HaversineCookLocationRepo {
    pool: PgPool,
}

impl HaversineCookLocationRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CookLocationRepo for HaversineCookLocationRepo {
    async fn find_nearby(&self, center: LatLng, radius_km: f64, limit: u32, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        let q = query_of(center, radius_km, limit)?;
        let bb = q.bounding_box();
        let sql = format!(
            "SELECT id, display_name, dist FROM ( \
               SELECT id, display_name, 2 * {EARTH_RADIUS_M:?}::float8 * asin(LEAST(1.0, sqrt( \
                   power(sin(radians(lat - $1::float8) / 2), 2) + \
                   cos(radians($1::float8)) * cos(radians(lat)) * power(sin(radians(lng - $2::float8) / 2), 2)))) AS dist \
               FROM cooks \
               WHERE is_verified AND food_safety_cert_expires_at > $3 \
                 AND lat BETWEEN $4 AND $5 AND lng BETWEEN $6 AND $7 \
             ) t WHERE dist <= $8 ORDER BY dist, id LIMIT $9"
        );
        let rows = sqlx::query(&sql)
            .bind(center.lat())
            .bind(center.lng())
            .bind(now)
            .bind(bb.min_lat)
            .bind(bb.max_lat)
            .bind(bb.min_lng)
            .bind(bb.max_lng)
            .bind(q.radius_m)
            .bind(i64::from(q.limit))
            .fetch_all(&self.pool)
            .await?;
        map_rows(rows)
    }
}

/// PostGIS implementation (`cooks.location geography(Point,4326)`, GiST-indexed).
///
/// Uses the **spherical** distance model (`use_spheroid = false`) so results
/// match the haversine implementation instead of differing by up to ~0.5 %
/// from the WGS84 spheroid. For the 25 km maximum radius this is well inside
/// what eaters can perceive.
#[cfg(feature = "postgis")]
#[derive(Clone)]
pub struct PostgisCookLocationRepo {
    pool: PgPool,
    /// Schema PostGIS is installed in (database-wide, may not be on the search_path).
    schema: String,
}

#[cfg(feature = "postgis")]
impl PostgisCookLocationRepo {
    /// Fails if PostGIS or the `cooks.location` column is missing, so a
    /// misconfigured deployment refuses to start instead of silently degrading.
    pub async fn new(pool: PgPool) -> StoreResult<Self> {
        let schema: Option<String> = sqlx::query_scalar(
            "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'postgis'",
        )
        .fetch_optional(&pool)
        .await?;
        let schema = schema.ok_or_else(|| StoreError::Db("PostGIS extension is not installed in this database".into()))?;
        let has_col: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
               WHERE table_schema = current_schema() AND table_name = 'cooks' AND column_name = 'location')",
        )
        .fetch_one(&pool)
        .await?;
        if !has_col {
            return Err(StoreError::Db("cooks.location is missing: run migrations on a PostGIS-enabled database".into()));
        }
        Ok(Self { pool, schema })
    }
}

#[cfg(feature = "postgis")]
#[async_trait]
impl CookLocationRepo for PostgisCookLocationRepo {
    async fn find_nearby(&self, center: LatLng, radius_km: f64, limit: u32, now: DateTime<Utc>) -> StoreResult<Vec<NearbyCook>> {
        let q = query_of(center, radius_km, limit)?;
        let s = format!("\"{}\"", self.schema.replace('"', "\"\""));
        let point = format!("{s}.ST_SetSRID({s}.ST_MakePoint($2::float8, $1::float8), 4326)::{s}.geography");
        let sql = format!(
            "SELECT id, display_name, {s}.ST_Distance(location, {point}, false) AS dist \
             FROM cooks \
             WHERE is_verified AND food_safety_cert_expires_at > $3 \
               AND {s}.ST_DWithin(location, {point}, $4::float8, false) \
             ORDER BY dist, id LIMIT $5"
        );
        let rows = sqlx::query(&sql)
            .bind(center.lat())
            .bind(center.lng())
            .bind(now)
            .bind(q.radius_m)
            .bind(i64::from(q.limit))
            .fetch_all(&self.pool)
            .await?;
        map_rows(rows)
    }
}
