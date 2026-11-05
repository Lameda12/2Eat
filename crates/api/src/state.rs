use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use twoeat_domain::PricingPolicy;
use twoeat_payments::PaymentGateway;
use twoeat_persistence::Store;
use uuid::Uuid;

use crate::error::ApiError;

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
    pub admin_token: String,
    pub pricing: PricingPolicy,
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub payments: Arc<dyn PaymentGateway>,
    pub documents: Arc<dyn DocumentStore>,
    pub clock: Arc<dyn Clock>,
    pub config: AppConfig,
}
