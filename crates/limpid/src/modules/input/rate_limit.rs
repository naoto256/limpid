//! Token bucket rate limiter for input modules.
//!
//! Enforces a maximum events-per-second rate. Each event consumes one token.
//! Tokens are replenished at a fixed rate. When the bucket is empty,
//! `acquire()` sleeps until a token is available.
//!
//! Used by input modules to throttle incoming event rates, preventing
//! downstream pipelines from being overwhelmed.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

pub struct RateLimiter {
    inner: Mutex<TokenBucket>,
}

struct TokenBucket {
    /// Maximum tokens (= burst size)
    capacity: f64,
    /// Current available tokens
    tokens: f64,
    /// Tokens added per second
    rate: f64,
    /// Last time tokens were refilled
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// - `events_per_sec`: sustained rate limit
    /// - Burst size is set to `events_per_sec` (allows 1 second of burst).
    pub fn new(events_per_sec: u64) -> Self {
        assert!(events_per_sec > 0, "rate_limit must be greater than 0");
        let rate = events_per_sec as f64;
        Self {
            inner: Mutex::new(TokenBucket {
                capacity: rate,
                tokens: rate, // start full
                rate,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Acquire one token. Sleeps if the bucket is empty.
    /// Returns immediately if tokens are available.
    pub async fn acquire(&self) {
        loop {
            let sleep_duration = {
                let mut bucket = self.inner.lock().await;
                bucket.refill();

                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return;
                }

                // Calculate how long until 1 token is available
                let deficit = 1.0 - bucket.tokens;
                Duration::from_secs_f64(deficit / bucket.rate)
            };

            tokio::time::sleep(sleep_duration).await;
        }
    }
}

impl TokenBucket {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limiter_allows_burst() {
        let limiter = RateLimiter::new(100);

        // Should allow 100 events immediately (full bucket)
        let start = Instant::now();
        for _ in 0..100 {
            limiter.acquire().await;
        }
        let elapsed = start.elapsed();

        // Burst should complete nearly instantly (< 50ms)
        assert!(elapsed < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn test_rate_limiter_throttles() {
        let limiter = RateLimiter::new(100);

        // Drain the bucket
        for _ in 0..100 {
            limiter.acquire().await;
        }

        // Next event should take ~10ms (1/100 sec)
        let start = Instant::now();
        limiter.acquire().await;
        let elapsed = start.elapsed();

        assert!(elapsed >= Duration::from_millis(5));
        assert!(elapsed < Duration::from_millis(50));
    }
}
