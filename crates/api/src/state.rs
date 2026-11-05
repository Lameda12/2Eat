use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use twoeat_domain::{PricingPolicy, RefundPolicy, WindowedRefundPolicy};
use twoeat_payments::PaymentGateway;
use twoeat_persistence::{AuthStore, CookLocationRepo, Store};
use uuid::Uuid;

use crate::{
    auth::limits::RateLimits,
    email::{Mailer, NoopMailer},
    error::ApiError,
};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Settable clock for tests (e.g. to expire a certificate).
pub struct ManualClock(Mutex<DateTime<Utc>>);
impl ManualClock {
    pub fn new(t: DateTime<Utc>) -> Self {
        Self(Mutex::new(t))
    }
    pub fn set(&self, t: DateTime<Utc>) {
        *self.0.lock().unwrap() = t;
    }
}
impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// Where uploaded certificate documents live.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    /// Persist and return an opaque reference.
    async fn put(&self, content_type: &str, bytes: Vec<u8>) -> Result<String, ApiError>;
}

pub struct LocalFsDocuments {
    dir: std::path::PathBuf,
}

impl LocalFsDocuments {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

fn extension(content_type: &str) -> &'static str {
    match content_type {
        "application/pdf" => "pdf",
        "image/png" => "png",
        _ => "jpg",
    }
}

#[async_trait]
impl DocumentStore for LocalFsDocuments {
    async fn put(&self, content_type: &str, bytes: Vec<u8>) -> Result<String, ApiError> {
        tokio::fs::create_dir_all(&self.dir).await.map_err(|e| ApiError::Internal(e.to_string()))?;
        // Server-generated name: never trust a client filename.
        let name = format!("{}.{}", Uuid::new_v4(), extension(content_type));
        tokio::fs::write(self.dir.join(&name), bytes).await.map_err(|e| ApiError::Internal(e.to_string()))?;
        Ok(name)
    }
}

#[derive(Default)]
pub struct MemoryDocuments(pub Mutex<Vec<(String, Vec<u8>)>>);

#[async_trait]
impl DocumentStore for MemoryDocuments {
    async fn put(&self, content_type: &str, bytes: Vec<u8>) -> Result<String, ApiError> {
        let name = format!("{}.{}", Uuid::new_v4(), extension(content_type));
        self.0.lock().unwrap().push((name.clone(), bytes));
        Ok(name)
    }
}

#[derive(Clone)]
pub struct AppConfig {
    pub pricing: PricingPolicy,
    /// Session lifetime (30 days).
    pub session_ttl: chrono::Duration,
}

impl AppConfig {
    pub fn new(pricing: PricingPolicy) -> Self {
        Self { pricing, session_ttl: chrono::Duration::days(crate::auth::token::SESSION_TTL_DAYS) }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub auth: Arc<dyn AuthStore>,
    pub locations: Arc<dyn CookLocationRepo>,
    pub payments: Arc<dyn PaymentGateway>,
    pub documents: Arc<dyn DocumentStore>,
    pub clock: Arc<dyn Clock>,
    pub mailer: Arc<dyn Mailer>,
    pub limits: Arc<RateLimits>,
    pub refund_policy: Arc<dyn RefundPolicy>,
    pub config: AppConfig,
}

impl AppState {
    /// One backend implementing all three persistence ports (both `PgStore` and
    /// `MemoryStore` do). Swap individual parts with the `with_*` methods.
    pub fn new<S>(
        backend: Arc<S>,
        payments: Arc<dyn PaymentGateway>,
        documents: Arc<dyn DocumentStore>,
        clock: Arc<dyn Clock>,
        config: AppConfig,
    ) -> Self
    where
        S: Store + AuthStore + CookLocationRepo + 'static,
    {
        Self {
            store: backend.clone(),
            auth: backend.clone(),
            locations: backend,
            payments,
            documents,
            clock,
            mailer: Arc::new(NoopMailer),
            limits: Arc::new(RateLimits::default()),
            refund_policy: Arc::new(WindowedRefundPolicy::default()),
            config,
        }
    }

    pub fn with_locations(mut self, l: Arc<dyn CookLocationRepo>) -> Self {
        self.locations = l;
        self
    }
    pub fn with_mailer(mut self, m: Arc<dyn Mailer>) -> Self {
        self.mailer = m;
        self
    }
    pub fn with_limits(mut self, l: RateLimits) -> Self {
        self.limits = Arc::new(l);
        self
    }
    pub fn with_refund_policy(mut self, p: Arc<dyn RefundPolicy>) -> Self {
        self.refund_policy = p;
        self
    }
}
