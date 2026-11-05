//! Email is out of scope for Phase 2: sending is stubbed. With the `dev-email`
//! feature the would-be message is logged so a developer can see it fire.

use async_trait::async_trait;

#[async_trait]
pub trait Mailer: Send + Sync {
    /// Verification email for a newly registered account. (Email verification
    /// is not enforced yet; this marks where it will plug in.)
    async fn send_verification(&self, to: &str);
}

/// Drops every message.
pub struct NoopMailer;

#[async_trait]
impl Mailer for NoopMailer {
    async fn send_verification(&self, _to: &str) {}
}

/// Logs messages. Dev only: addresses are personal data and must not reach production logs.
#[cfg(feature = "dev-email")]
pub struct LogMailer;

#[cfg(feature = "dev-email")]
#[async_trait]
impl Mailer for LogMailer {
    async fn send_verification(&self, to: &str) {
        tracing::info!(to = %to, "[dev-email] would send verification email");
    }
}
