//! Resource limits: per-client rate limiting and connection accounting

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Token bucket refilled at `rate` tokens per second, holding at most one
/// second's worth
#[derive(Debug)]
pub struct TokenBucket {
    rate: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate: u32) -> Self {
        Self {
            rate: f64::from(rate),
            tokens: f64::from(rate),
            last: Instant::now(),
        }
    }

    /// Take `n` tokens if available
    #[cfg_attr(not(feature = "nat"), allow(dead_code))]
    pub fn take(&mut self, n: u32) -> bool {
        self.take_at(n, Instant::now())
    }

    fn take_at(&mut self, n: u32, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = elapsed.mul_add(self.rate, self.tokens).min(self.rate);
        if self.tokens >= f64::from(n) {
            self.tokens -= f64::from(n);
            true
        } else {
            false
        }
    }
}

/// Per-client traffic limit: frames and bytes per second
#[derive(Debug)]
pub struct RateLimit {
    frames: TokenBucket,
    bytes: TokenBucket,
}

impl RateLimit {
    pub fn new(frames_per_sec: u32, bytes_per_sec: u32) -> Self {
        Self {
            frames: TokenBucket::new(frames_per_sec),
            bytes: TokenBucket::new(bytes_per_sec),
        }
    }

    /// Whether a frame of `len` bytes may pass
    pub fn allow(&mut self, len: usize) -> bool {
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        // Check both before taking, so a refused frame costs nothing
        let now = Instant::now();
        let ok = self.frames.take_at(0, now) && self.bytes.take_at(0, now);
        ok && self.frames.take_at(1, now) && self.bytes.take_at(len, now)
    }
}

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// Counts connected clients, in total and per address
#[derive(Debug, Clone)]
pub struct ConnectionLimiter {
    counts: Arc<Mutex<Counts>>,
    max_total: usize,
    max_per_ip: usize,
}

/// Why a connection was refused
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    TooManyClients,
    TooManyFromAddress,
}

/// A connection slot; released when dropped
#[derive(Debug)]
pub struct ConnectionSlot {
    counts: Arc<Mutex<Counts>>,
    ip: IpAddr,
}

impl ConnectionLimiter {
    pub fn new(max_total: usize, max_per_ip: usize) -> Self {
        Self {
            counts: Arc::default(),
            max_total,
            max_per_ip,
        }
    }

    // The lock is held for the whole check-and-increment on purpose
    #[allow(clippy::significant_drop_tightening)]
    pub fn acquire(&self, ip: IpAddr) -> Result<ConnectionSlot, Refusal> {
        let mut c = self.counts.lock().unwrap();
        if c.total >= self.max_total {
            return Err(Refusal::TooManyClients);
        }
        let n = c.per_ip.entry(ip).or_default();
        if *n >= self.max_per_ip {
            return Err(Refusal::TooManyFromAddress);
        }
        *n += 1;
        c.total += 1;
        Ok(ConnectionSlot {
            counts: Arc::clone(&self.counts),
            ip,
        })
    }

    pub fn total(&self) -> usize {
        self.counts.lock().unwrap().total
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut c = self.counts.lock().unwrap();
        c.total -= 1;
        if let Some(n) = c.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                c.per_ip.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn token_bucket_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10);
        b.last = t0;
        for _ in 0..10 {
            assert!(b.take_at(1, t0));
        }
        assert!(!b.take_at(1, t0), "empty");
        assert!(
            b.take_at(5, t0 + Duration::from_millis(500)),
            "half a second refills 5"
        );
        assert!(!b.take_at(1, t0 + Duration::from_millis(500)));
        assert!(
            !b.take_at(11, t0 + Duration::from_secs(60)),
            "capacity is one second"
        );
    }

    #[test]
    fn rate_limit_checks_frames_and_bytes() {
        let mut r = RateLimit::new(100, 1000);
        assert!(r.allow(600));
        assert!(!r.allow(600), "byte budget exhausted");
        assert!(r.allow(300), "a refused frame cost nothing");
        let mut r = RateLimit::new(2, 1_000_000);
        assert!(r.allow(1) && r.allow(1));
        assert!(!r.allow(1), "frame budget exhausted");
    }

    #[test]
    fn connection_limits() {
        let l = ConnectionLimiter::new(3, 2);
        let a: IpAddr = "192.0.2.1".parse().unwrap();
        let b: IpAddr = "192.0.2.2".parse().unwrap();
        let s1 = l.acquire(a).unwrap();
        let _s2 = l.acquire(a).unwrap();
        assert_eq!(l.acquire(a).unwrap_err(), Refusal::TooManyFromAddress);
        let _s3 = l.acquire(b).unwrap();
        assert_eq!(l.acquire(b).unwrap_err(), Refusal::TooManyClients);
        drop(s1);
        assert_eq!(l.total(), 2);
        assert!(l.acquire(a).is_ok(), "slot released on drop");
    }
}
