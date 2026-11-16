use std::{
    net::IpAddr,
    sync::{
        atomic::{AtomicU32, AtomicU64},
        Arc,
    },
    time::Duration,
};

use moka::sync::Cache;

use crate::{config::Config, wiki::Wiki};

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub rate_limited: AtomicU64,
    pub edits: AtomicU64,
    pub searches: AtomicU64,
}

/// Fixed-window per-IP limiter: each IP gets a counter that expires `window` after creation.
pub struct RateLimiter {
    counters: Cache<IpAddr, Arc<AtomicU32>>,
    limit: u32,
}

impl RateLimiter {
    pub fn new(limit: u32, window: Duration) -> Self {
        Self { counters: Cache::builder().max_capacity(100_000).time_to_live(window).build(), limit }
    }

    pub fn allow(&self, ip: IpAddr) -> bool {
        if self.limit == 0 {
            return true;
        }
        let counter = self.counters.get_with(ip, || Arc::new(AtomicU32::new(0)));
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < self.limit
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub wiki: Wiki,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_blocks_after_limit() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(rl.allow(ip));
        assert!(rl.allow(ip));
        assert!(!rl.allow(ip));
        assert!(rl.allow("127.0.0.2".parse().unwrap()));
    }

    #[test]
    fn limiter_zero_disables() {
        let rl = RateLimiter::new(0, Duration::from_secs(60));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..1000 {
            assert!(rl.allow(ip));
        }
    }
}
