//! Rate limits for `/auth/login` and `/auth/register`, per client IP *and* per
//! (normalised) email. The email limiter applies whether or not the account
//! exists, so it leaks nothing.

use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use axum::{
    extract::ConnectInfo,
    http::{Extensions, HeaderMap},
};
use governor::{
    DefaultKeyedRateLimiter, Quota,
    clock::{Clock, DefaultClock},
};

#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    pub login_per_ip_per_min: u32,
    pub login_per_email_per_min: u32,
    pub register_per_ip_per_min: u32,
    pub register_per_email_per_min: u32,
    /// Take the client IP from the right-most `X-Forwarded-For` entry. Only
    /// enable behind a proxy you control that appends the real peer address.
    pub trust_forwarded_for: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            login_per_ip_per_min: 20,
            login_per_email_per_min: 5,
            register_per_ip_per_min: 5,
            register_per_email_per_min: 3,
            trust_forwarded_for: false,
        }
    }
}

pub struct RateLimits {
    cfg: RateLimitConfig,
    login_ip: DefaultKeyedRateLimiter<String>,
    login_email: DefaultKeyedRateLimiter<String>,
    register_ip: DefaultKeyedRateLimiter<String>,
    register_email: DefaultKeyedRateLimiter<String>,
    calls: AtomicU64,
}

fn limiter(per_min: u32) -> DefaultKeyedRateLimiter<String> {
    let n = NonZeroU32::new(per_min.max(1)).expect("max(1)");
    governor::RateLimiter::keyed(Quota::per_minute(n))
}

impl Default for RateLimits {
    fn default() -> Self {
        Self::new(RateLimitConfig::default())
    }
}

impl RateLimits {
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            login_ip: limiter(cfg.login_per_ip_per_min),
            login_email: limiter(cfg.login_per_email_per_min),
            register_ip: limiter(cfg.register_per_ip_per_min),
            register_email: limiter(cfg.register_per_email_per_min),
            cfg,
            calls: AtomicU64::new(0),
        }
    }

    pub fn client_ip(&self, headers: &HeaderMap, ext: &Extensions) -> String {
        if self.cfg.trust_forwarded_for
            && let Some(ip) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return ip.to_string();
        }
        ext.get::<ConnectInfo<SocketAddr>>().map_or_else(|| "unknown".to_owned(), |c| c.0.ip().to_string())
    }

    fn gc(&self) {
        // Keep the keyed state from growing without bound.
        if self.calls.fetch_add(1, Ordering::Relaxed) % 1024 == 1023 {
            for l in [&self.login_ip, &self.login_email, &self.register_ip, &self.register_email] {
                l.retain_recent();
                l.shrink_to_fit();
            }
        }
    }

    fn check(a: &DefaultKeyedRateLimiter<String>, b: &DefaultKeyedRateLimiter<String>, ip: &str, email: &str) -> Result<(), Duration> {
        let clock = DefaultClock::default();
        let wait = |e: governor::NotUntil<_>| e.wait_time_from(clock.now());
        a.check_key(&ip.to_owned()).map_err(wait)?;
        b.check_key(&email.to_owned()).map_err(wait)?;
        Ok(())
    }

    /// `Err(retry_after)` when over the limit.
    pub fn check_login(&self, ip: &str, email: &str) -> Result<(), Duration> {
        self.gc();
        Self::check(&self.login_ip, &self.login_email, ip, email)
    }

    pub fn check_register(&self, ip: &str, email: &str) -> Result<(), Duration> {
        self.gc();
        Self::check(&self.register_ip, &self.register_email, ip, email)
    }
}
