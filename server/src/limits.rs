use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Counts open WebSocket connections, in total and per client IP.
pub struct ConnTracker {
    max_total: usize,
    max_per_ip: usize,
    inner: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

impl ConnTracker {
    pub fn new(max_total: usize, max_per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            max_total,
            max_per_ip,
            inner: Mutex::new((0, HashMap::new())),
        })
    }

    /// `None` if the server or this IP is at its limit. The slot is released when the guard drops.
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConnGuard> {
        let mut g = self.inner.lock().unwrap();
        if g.0 >= self.max_total || g.1.get(&ip).copied().unwrap_or(0) >= self.max_per_ip {
            return None;
        }
        g.0 += 1;
        *g.1.entry(ip).or_insert(0) += 1;
        Some(ConnGuard {
            tracker: self.clone(),
            ip,
        })
    }

    pub fn total(&self) -> usize {
        self.inner.lock().unwrap().0
    }
}

pub struct ConnGuard {
    tracker: Arc<ConnTracker>,
    ip: IpAddr,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut g = self.tracker.inner.lock().unwrap();
        g.0 -= 1;
        if let Some(n) = g.1.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                g.1.remove(&self.ip);
            }
        }
    }
}

/// Limits how fast one IP may *open* connections (accepted or not), so a script cannot churn
/// through the connection cap or burn CPU on upgrades. The cap in `ConnTracker` limits how many
/// are open at once; this limits how often a new one may be attempted.
pub struct ConnRate {
    rate: f64,
    burst: f64,
    inner: Mutex<HashMap<IpAddr, TokenBucket>>,
}

/// Above this many tracked IPs, idle entries are dropped so the map cannot grow without bound.
const CONN_RATE_PRUNE_AT: usize = 10_000;

impl ConnRate {
    pub fn new(rate: f64, burst: f64) -> Arc<Self> {
        Arc::new(Self {
            rate,
            burst,
            inner: Mutex::new(HashMap::new()),
        })
    }

    pub fn allow(&self, ip: IpAddr, now: Instant) -> bool {
        let mut map = self.inner.lock().unwrap();
        if map.len() >= CONN_RATE_PRUNE_AT {
            // a bucket idle long enough to be full again carries no information
            let idle =
                std::time::Duration::from_secs_f64((self.burst / self.rate.max(1e-9)).max(1.0));
            map.retain(|_, b| now.saturating_duration_since(b.last) < idle);
        }
        map.entry(ip)
            .or_insert_with(|| TokenBucket::new(self.rate, self.burst, now))
            .allow(now)
    }
}

/// Classic token bucket: `rate` tokens per second up to `burst`.
pub struct TokenBucket {
    tokens: f64,
    burst: f64,
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate: f64, burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            burst,
            rate,
            last: now,
        }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bucket_allows_burst_then_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(5.0, 10.0, t0);
        assert_eq!((0..10).filter(|_| b.allow(t0)).count(), 10);
        assert!(!b.allow(t0));
        // 0.4 s at 5/s is 2 tokens
        let t1 = t0 + Duration::from_millis(400);
        assert!(b.allow(t1));
        assert!(b.allow(t1));
        assert!(!b.allow(t1));
    }

    #[test]
    fn conn_rate_limits_per_ip_and_refills() {
        let t0 = Instant::now();
        let r = ConnRate::new(1.0, 3.0);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!((0..5).filter(|_| r.allow(a, t0)).count(), 3);
        assert!(r.allow(b, t0), "other IPs are unaffected");
        assert!(r.allow(a, t0 + Duration::from_secs(2)), "refills over time");
    }

    #[test]
    fn tracker_caps_per_ip_and_total_and_releases() {
        let t = ConnTracker::new(3, 2);
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        let g1 = t.try_acquire(a).unwrap();
        let _g2 = t.try_acquire(a).unwrap();
        assert!(t.try_acquire(a).is_none(), "per-ip cap");
        let _g3 = t.try_acquire(b).unwrap();
        assert!(t.try_acquire(b).is_none(), "total cap");
        drop(g1);
        assert_eq!(t.total(), 2);
        assert!(t.try_acquire(b).is_some());
    }
}
