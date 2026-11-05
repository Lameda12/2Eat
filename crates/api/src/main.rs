use std::{io::BufRead, net::SocketAddr, sync::Arc};

use anyhow::{Context, bail};
use twoeat_api::{
    AppConfig, AppState,
    auth::{
        limits::{RateLimitConfig, RateLimits},
        password::{MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS, hash_password},
    },
    state::{DocumentStore, LocalFsDocuments, SystemClock},
};
use twoeat_domain::{PricingPolicy, Role};
use twoeat_payments::MockPaymentGateway;
use twoeat_persistence::{AuthStore, MemoryStore, NewUser, PgStore};
use uuid::Uuid;

fn env_num(k: &str, d: i64) -> i64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn env_flag(k: &str) -> bool {
    matches!(std::env::var(k).as_deref(), Ok("1" | "true" | "TRUE" | "yes"))
}

async fn connect_pg() -> anyhow::Result<PgStore> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let pg = PgStore::connect(&url).await.context("connecting to PostgreSQL")?;
    pg.migrate().await.context("running migrations")?;
    Ok(pg)
}

/// `twoeat-api create-admin <email>`: creates an admin account. The password is
/// read from the first line of stdin so it never appears in argv or the environment.
async fn create_admin(email: &str) -> anyhow::Result<()> {
    let pg = connect_pg().await?;
    let mut password = String::new();
    std::io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']);
    if password.chars().count() < MIN_PASSWORD_CHARS || password.len() > MAX_PASSWORD_BYTES {
        bail!("password must be {MIN_PASSWORD_CHARS}-{MAX_PASSWORD_BYTES} bytes");
    }
    let password_hash = hash_password(password).await.map_err(|_| anyhow::anyhow!("hashing failed"))?;
    let user = NewUser {
        id: Uuid::new_v4(),
        email: email.trim().to_lowercase(),
        password_hash,
        role: Role::Admin,
        created_at: chrono::Utc::now(),
    };
    let id = user.id;
    pg.create_user(user).await.context("creating admin (is the email already registered?)")?;
    println!("created admin {id}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some("create-admin") = args.first().map(String::as_str) {
        let email = args.get(1).context("usage: twoeat-api create-admin <email>   (password on stdin)")?;
        return create_admin(email).await;
    }

    let defaults = PricingPolicy::default();
    let pricing = PricingPolicy {
        platform_fee_bps: env_num("PLATFORM_FEE_BPS", defaults.platform_fee_bps.into()) as u32,
        delivery_fee_cents: env_num("DELIVERY_FEE_CENTS", defaults.delivery_fee_cents),
        max_portions_per_order: env_num("MAX_PORTIONS_PER_ORDER", defaults.max_portions_per_order.into()) as u32,
    };
    let limits = RateLimits::new(RateLimitConfig { trust_forwarded_for: env_flag("TRUST_FORWARDED_FOR"), ..Default::default() });

    // Payments: the Stripe Connect gateway is a stub, so the mock is the only working gateway.
    tracing::warn!("using MockPaymentGateway: no real money moves");
    let payments = Arc::new(MockPaymentGateway::new());
    let documents: Arc<dyn DocumentStore> =
        Arc::new(LocalFsDocuments::new(std::env::var("UPLOAD_DIR").unwrap_or_else(|_| "uploads".into())));
    let clock = Arc::new(SystemClock);
    let config = AppConfig::new(pricing);

    let mut state = if std::env::var("DATABASE_URL").is_ok() {
        let pg = Arc::new(connect_pg().await?);
        tracing::info!("using PostgreSQL");
        #[allow(unused_mut)]
        let mut state = AppState::new(pg.clone(), payments, documents, clock, config);
        #[cfg(feature = "postgis")]
        {
            let repo = twoeat_persistence::PostgisCookLocationRepo::new(pg.pool().clone()).await.context("PostGIS discovery")?;
            tracing::info!("using PostGIS for cook discovery");
            state = state.with_locations(Arc::new(repo));
        }
        state
    } else {
        tracing::warn!("DATABASE_URL not set: using the in-memory store (data is lost on exit)");
        AppState::new(Arc::new(MemoryStore::new()), payments, documents, clock, config)
    };
    state = state.with_limits(limits);
    #[cfg(feature = "dev-email")]
    {
        state = state.with_mailer(Arc::new(twoeat_api::email::LogMailer));
    }

    // Refuses to start if `dev-auth` is compiled into a release build.
    let app = twoeat_api::try_router(state).map_err(|e| anyhow::anyhow!("{e}"))?;

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on http://{addr} (OpenAPI at /openapi.json)");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
