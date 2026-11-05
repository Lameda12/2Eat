//! Every `CookLocationRepo` must return the same cooks in the same order, with
//! distances equal within floating-point tolerance, for the same fixtures.
//!
//! Always compares the in-memory ranking against the Postgres bounding-box +
//! haversine repo; with `--features postgis` it also compares PostGIS.
//! Needs `DATABASE_URL` for the Postgres legs (the in-memory leg always runs).

use chrono::{Duration, SubsecRound, Utc};
use twoeat_domain::*;
use twoeat_geo::{EARTH_RADIUS_M, haversine_m};
use twoeat_persistence::*;
use uuid::Uuid;

/// Distances may differ by float noise (and the ~1e-6 sphere-radius convention), never by more than this.
const TOLERANCE_M: f64 = 0.5;
/// Random fixture cooks; indices >= this are the always-eligible boundary pair.
const N_RANDOM: usize = 150;

fn sg(lat: f64, lng: f64) -> LatLng {
    LatLng::new_in_singapore(lat, lng).unwrap()
}

/// Deterministic pseudo-random points over the island (no external RNG needed).
/// A candidate is skipped if, for any query centre, its distance is within 3 m of an
/// already-accepted point's: that makes distance ordering unambiguous by construction.
fn fixture_points(n: usize) -> Vec<LatLng> {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 11) as f64 / (1u64 << 53) as f64
    };
    let centers: Vec<LatLng> = queries().0.iter().map(|q| q.0).collect();
    let mut out: Vec<LatLng> = Vec::with_capacity(n);
    while out.len() < n {
        let p = sg(1.22 + next() * 0.20, 103.62 + next() * 0.40);
        let clashes = out.iter().any(|o| centers.iter().any(|c| (haversine_m(*c, p) - haversine_m(*c, *o)).abs() < 3.0));
        if !clashes {
            out.push(p);
        }
    }
    out
}

/// A point `meters` due north of `p` (so we can sit just inside / outside a radius).
fn north(p: LatLng, meters: f64) -> LatLng {
    sg(p.lat() + (meters / EARTH_RADIUS_M).to_degrees(), p.lng())
}

struct Queries(Vec<(LatLng, f64, u32)>);

fn queries() -> Queries {
    Queries(vec![
        (sg(1.3521, 103.8198), 3.0, 20),                   // centre, default-ish radius
        (sg(1.3521, 103.8198), 25.0, 100),                 // max radius, whole island
        (sg(1.2840, 103.8510), 2.0, 5),                    // CBD, tight
        (sg(1.3644, 103.9915), 8.0, 7),                    // east, limit smaller than hits
        (sg(1.3329, 103.7436), 5.5, 50),                   // west
        (sg(1.4382, 103.7890), 12.0, 3),                   // north, limit 3
        (LatLng::new(1.2000, 103.5000).unwrap(), 1.0, 10), // outside the island: no hits
    ])
}

async fn seed(
    store: &(impl Store + AuthStore),
    points: &[LatLng],
    boundary_center: LatLng,
    boundary_radius_km: f64,
) -> Vec<(Uuid, LatLng)> {
    let t = Utc::now().trunc_subsecs(6);
    let admin = Uuid::new_v4();
    store
        .create_user(NewUser {
            id: admin,
            email: format!("{admin}@example.com"),
            password_hash: "x".into(),
            role: Role::Admin,
            created_at: t,
        })
        .await
        .unwrap();

    let mut all: Vec<LatLng> = points.to_vec();
    // One cook just inside the radius and one just outside it.
    all.push(north(boundary_center, boundary_radius_km * 1000.0 - 15.0));
    all.push(north(boundary_center, boundary_radius_km * 1000.0 + 15.0));

    let mut out = vec![];
    for (i, loc) in all.iter().enumerate() {
        let cook = Cook::register(Uuid::new_v4(), &format!("Cook {i}"), *loc, &HbfbAcknowledgements::all()).unwrap();
        let sub = CertificationSubmission::new(cook.id, "d".into(), "application/pdf".into(), t).unwrap();
        let review = sub.id;
        store.register_cook(&cook, &sub, t).await.unwrap();
        match if i < N_RANDOM { i % 11 } else { 0 } {
            // Never approved / rejected / already expired: must never be discoverable by any repo.
            3 => {}
            5 => {
                store
                    .decide_review(review, Some(admin), CertDecision::Reject { reason: RejectionReason::NameMismatch }, None, t)
                    .await
                    .unwrap();
            }
            _ => {
                store
                    .decide_review(review, Some(admin), CertDecision::Approve { expires_at: t + Duration::days(60) }, None, t)
                    .await
                    .unwrap();
            }
        }
        out.push((cook.id, *loc));
    }
    out
}

/// Expected ids/distances straight from the pure-Rust haversine over the seeded cooks.
fn expected(cooks: &[(Uuid, LatLng)], eligible: impl Fn(usize) -> bool, center: LatLng, radius_km: f64, limit: u32) -> Vec<(Uuid, f64)> {
    let mut v: Vec<_> = cooks
        .iter()
        .enumerate()
        .filter(|(i, _)| eligible(*i))
        .map(|(_, (id, loc))| (*id, haversine_m(center, *loc)))
        .filter(|(_, d)| *d <= radius_km * 1000.0)
        .collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1));
    v.truncate(limit as usize);
    v
}

fn eligible(i: usize) -> bool {
    i >= N_RANDOM || !matches!(i % 11, 3 | 5)
}

fn assert_parity(label: &str, got: &[NearbyCook], want: &[(Uuid, f64)], q: (LatLng, f64, u32)) {
    let got_ids: Vec<_> = got.iter().map(|c| c.id).collect();
    let want_ids: Vec<_> = want.iter().map(|(id, _)| *id).collect();
    assert_eq!(got_ids, want_ids, "{label}: distance-ordered ids differ for query {q:?}");
    for (g, (_, wd)) in got.iter().zip(want) {
        assert!((g.distance_m - wd).abs() <= TOLERANCE_M, "{label}: distance {} vs {} for {}", g.distance_m, wd, g.id);
    }
}

fn assert_fixture_is_not_tie_prone(want: &[(Uuid, f64)]) {
    for w in want.windows(2) {
        assert!(
            w[1].1 - w[0].1 > 2.0 * TOLERANCE_M,
            "fixture has near-tied distances ({} m vs {} m); ordering would be implementation-defined",
            w[0].1,
            w[1].1
        );
    }
}

#[tokio::test]
async fn memory_matches_reference_ranking() {
    let mem = MemoryStore::new();
    let pts = fixture_points(N_RANDOM);
    let cooks = seed(&mem, &pts, sg(1.3521, 103.8198), 3.0).await;
    let now = Utc::now();
    for q in queries().0 {
        let want = expected(&cooks, eligible, q.0, q.1, q.2);
        assert_fixture_is_not_tie_prone(&want);
        let got = mem.find_nearby(q.0, q.1, q.2, now).await.unwrap();
        assert_parity("memory", &got, &want, q);
    }
}

#[tokio::test]
async fn haversine_and_postgis_match_memory() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set; skipping Postgres parity legs");
        return;
    };
    let schema = format!("t_{}", Uuid::new_v4().simple());
    let admin_pool = sqlx::postgres::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin_pool).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _| {
            let s = schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {s}, public")).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let pg = PgStore::new(pool);
    pg.migrate().await.unwrap();

    let pts = fixture_points(N_RANDOM);
    let mem = MemoryStore::new();
    let mem_cooks = seed(&mem, &pts, sg(1.3521, 103.8198), 3.0).await;
    let pg_cooks = seed(&pg, &pts, sg(1.3521, 103.8198), 3.0).await;
    // Same fixtures, different ids: compare by fixture index.
    let idx_of = |cooks: &[(Uuid, LatLng)], id: Uuid| cooks.iter().position(|(c, _)| *c == id).unwrap();

    let haversine = pg.haversine_locations();
    #[cfg(feature = "postgis")]
    let postgis =
        PostgisCookLocationRepo::new(pg.pool().clone()).await.expect("PostGIS must be installed to run this test with --features postgis");

    let now = Utc::now();
    let mut nonempty = 0;
    for q in queries().0 {
        let want = expected(&mem_cooks, eligible, q.0, q.1, q.2);
        assert_fixture_is_not_tie_prone(&want);
        nonempty += usize::from(!want.is_empty());
        let want_idx: Vec<_> = want.iter().map(|(id, _)| idx_of(&mem_cooks, *id)).collect();

        let mem_got = mem.find_nearby(q.0, q.1, q.2, now).await.unwrap();
        assert_parity("memory", &mem_got, &want, q);

        let check = |label: &str, got: Vec<NearbyCook>| {
            let got_idx: Vec<_> = got.iter().map(|c| idx_of(&pg_cooks, c.id)).collect();
            assert_eq!(got_idx, want_idx, "{label}: distance-ordered cooks differ for query {q:?}");
            for (g, (_, wd)) in got.iter().zip(&want) {
                assert!((g.distance_m - wd).abs() <= TOLERANCE_M, "{label}: {} vs {wd} for query {q:?}", g.distance_m);
            }
        };
        check("haversine-sql", haversine.find_nearby(q.0, q.1, q.2, now).await.unwrap());
        #[cfg(feature = "postgis")]
        check("postgis", postgis.find_nearby(q.0, q.1, q.2, now).await.unwrap());
    }
    assert!(nonempty >= 6, "the query set must actually exercise results");

    // The just-inside / just-outside pair around the 3 km circle is split identically everywhere.
    let center = sg(1.3521, 103.8198);
    let inside_name = format!("Cook {}", pts.len());
    let outside_name = format!("Cook {}", pts.len() + 1);
    let names = |v: &[NearbyCook]| v.iter().map(|c| c.display_name.clone()).collect::<Vec<_>>();
    let h = names(&haversine.find_nearby(center, 3.0, 100, now).await.unwrap());
    assert!(h.contains(&inside_name) && !h.contains(&outside_name), "haversine boundary");
    #[cfg(feature = "postgis")]
    {
        let p = names(&postgis.find_nearby(center, 3.0, 100, now).await.unwrap());
        assert!(p.contains(&inside_name) && !p.contains(&outside_name), "postgis boundary");
    }

    // Expired certificates are excluded by every implementation.
    let later = now + Duration::days(61);
    assert!(haversine.find_nearby(center, 25.0, 100, later).await.unwrap().is_empty());
    #[cfg(feature = "postgis")]
    assert!(postgis.find_nearby(center, 25.0, 100, later).await.unwrap().is_empty());
    assert!(mem.find_nearby(center, 25.0, 100, later).await.unwrap().is_empty());

    // Invalid queries are rejected the same way (no silent clamping).
    assert!(haversine.find_nearby(center, 0.0, 10, now).await.is_err());
    assert!(haversine.find_nearby(center, 26.0, 10, now).await.is_err());
    #[cfg(feature = "postgis")]
    {
        assert!(postgis.find_nearby(center, 0.0, 10, now).await.is_err());
        assert!(postgis.find_nearby(center, 26.0, 10, now).await.is_err());
    }
}

#[cfg(feature = "postgis")]
#[tokio::test]
async fn postgis_repo_refuses_to_start_without_the_geography_column() {
    let Ok(url) = std::env::var("DATABASE_URL") else { return };
    // A schema with the cooks table but no migrations applied: no `location` column.
    let schema = format!("t_{}", Uuid::new_v4().simple());
    let admin_pool = sqlx::postgres::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin_pool).await.unwrap();
    sqlx::query(&format!("CREATE TABLE {schema}.cooks (id uuid primary key)")).execute(&admin_pool).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            let s = schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {s}, public")).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    let err = PostgisCookLocationRepo::new(pool).await.err().expect("must fail");
    assert!(err.to_string().contains("cooks.location"), "{err}");
}
