use std::sync::Arc;

use anyhow::Context;
use twoeat_api::{
    AppConfig, AppState,
    state::{LocalFsDocuments, SystemClock},
};
use twoeat_domain::PricingPolicy;
use twoeat_payments::MockPaymentGateway;
use twoeat_persistence::{MemoryStore, PgStore, Store};
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()))
        .init();

    let store: Arc<dyn Store> = match std::env::var("DATABASE_URL") {
        Ok(url) => {
            let pg = PgStore::connect(&url).await.context("connecting to PostgreSQL")?;
            pg.migrate().await.context("running migrations")?;
            tracing::info!("using PostgreSQL");
            Arc::new(pg)
        }
        Err(_) => {
            tracing::warn!("DATABASE_URL not set: using the in-memory store (data is lost on exit)");
            Arc::new(MemoryStore::new())
        }
    };

    let admin_token = std::env::var("ADMIN_TOKEN").unwrap_or_else(|_| {
        let t = Uuid::new_v4().to_string();
        tracing::warn!("ADMIN_TOKEN not set; generated a one-off token for this run: {t}");
        t
    });

    let defaults = PricingPolicy::default();
    let env_num = |k: &str, d: i64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let pricing = PricingPolicy {
        platform_fee_bps: env_num("PLATFORM_FEE_BPS", defaults.platform_fee_bps.into()) as u32,
        delivery_fee_cents: env_num("DELIVERY_FEE_CENTS", defaults.delivery_fee_cents),
        max_portions_per_order: env_num("MAX_PORTIONS_PER_ORDER", defaults.max_portions_per_order.into()) as u32,
    };

    // Payments: the Stripe Connect gateway is a stub, so the mock is the only working gateway.
    tracing::warn!("using MockPaymentGateway: no real money moves");
    let state = AppState {
        store,
        payments: Arc::new(MockPaymentGateway::new()),
        documents: Arc::new(LocalFsDocuments::new(std::env::var("UPLOAD_DIR").unwrap_or_else(|_| "uploads".into()))),
        clock: Arc::new(SystemClock),
        config: AppConfig { admin_token, pricing },
    };

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on http://{addr} (OpenAPI at /openapi.json)");
    axum::serve(listener, twoeat_api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
