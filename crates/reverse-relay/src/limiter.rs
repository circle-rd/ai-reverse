//! Bounds repeated authentication failures per client address.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Tuning of the failure limiter.
#[derive(Debug, Clone, Copy)]
pub struct LimiterConfig {
    /// Failures within the window that trigger a lockout.
    pub threshold: u32,
    /// Sliding window that counts failures.
    pub window: Duration,
    /// Lockout applied at the threshold; doubles for each extra failure, up to eight times.
    pub lockout: Duration,
}

/// What a check says about the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The caller may proceed.
    Allow,
    /// The caller is locked out for the given duration.
    Blocked(Duration),
}

const MAX_ENTRIES: usize = 10_000;
const MAX_LOCKOUT_FACTOR: u32 = 3;

struct Entry {
    failures: u32,
    window_start: Instant,
    last_seen: Instant,
    blocked_until: Option<Instant>,
}

/// Bounded per-address failure tracker.
///
/// The state is pruned on every decision, so an attacker cannot grow it; when too many
/// addresses fail at once the oldest entries are dropped, which can only weaken the
/// limiter, never the service.
pub struct FailureLimiter {
    config: LimiterConfig,
    entries: Mutex<BTreeMap<IpAddr, Entry>>,
}

impl FailureLimiter {
    /// Builds an empty limiter.
    #[must_use]
    pub fn new(config: LimiterConfig) -> Self {
        Self {
            config,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether the address may attempt an authentication now.
    #[must_use]
    pub fn check(&self, address: IpAddr, now: Instant) -> Decision {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(&address) else {
            return Decision::Allow;
        };
        if let Some(until) = entry.blocked_until {
            if until > now {
                return Decision::Blocked(until.saturating_duration_since(now));
            }
            entry.blocked_until = None;
            entry.failures = 0;
            entry.window_start = now;
        } else if now.saturating_duration_since(entry.window_start) > self.config.window {
            entry.failures = 0;
            entry.window_start = now;
        }
        entry.last_seen = now;
        Decision::Allow
    }

    /// Records one failed authentication.
    pub fn failure(&self, address: IpAddr, now: Instant) {
        let mut entries = self.lock();
        let entry = entries.entry(address).or_insert_with(|| Entry {
            failures: 0,
            window_start: now,
            last_seen: now,
            blocked_until: None,
        });
        if now.saturating_duration_since(entry.window_start) > self.config.window {
            entry.failures = 0;
            entry.window_start = now;
        }
        entry.failures = entry.failures.saturating_add(1);
        entry.last_seen = now;
        if entry.failures >= self.config.threshold {
            let excess = entry.failures - self.config.threshold;
            let factor = 1_u32 << excess.min(MAX_LOCKOUT_FACTOR);
            entry.blocked_until = Some(now + self.config.lockout.saturating_mul(factor));
        }
        if entries.len() > MAX_ENTRIES {
            let window = self.config.window.max(Duration::from_secs(60));
            entries.retain(|_, entry| now.saturating_duration_since(entry.last_seen) <= window);
            while entries.len() > MAX_ENTRIES {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_seen)
                    .map(|(address, _)| *address);
                match oldest {
                    Some(address) => {
                        entries.remove(&address);
                    }
                    None => break,
                }
            }
        }
    }

    /// Clears the failures of an address that authenticated successfully.
    pub fn success(&self, address: IpAddr) {
        self.lock().remove(&address);
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<IpAddr, Entry>> {
        // Poisoning can only follow a panic; release builds abort on panic.
        self.entries.lock().expect("limiter lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: LimiterConfig = LimiterConfig {
        threshold: 3,
        window: Duration::from_secs(60),
        lockout: Duration::from_secs(30),
    };

    fn ip() -> IpAddr {
        "203.0.113.7".parse().unwrap()
    }

    #[test]
    fn repeated_failures_block_then_expire() {
        let limiter = FailureLimiter::new(CONFIG);
        let start = Instant::now();
        for _ in 0..2 {
            assert_eq!(limiter.check(ip(), start), Decision::Allow);
            limiter.failure(ip(), start);
        }
        assert_eq!(limiter.check(ip(), start), Decision::Allow);
        limiter.failure(ip(), start);
        assert!(matches!(limiter.check(ip(), start), Decision::Blocked(_)));
        assert_eq!(
            limiter.check(ip(), start + Duration::from_secs(31)),
            Decision::Allow
        );
    }

    #[test]
    fn a_success_clears_the_record_and_the_window_ages_out() {
        let limiter = FailureLimiter::new(CONFIG);
        let start = Instant::now();
        limiter.failure(ip(), start);
        limiter.failure(ip(), start);
        limiter.success(ip());
        limiter.failure(ip(), start);
        assert_eq!(limiter.check(ip(), start), Decision::Allow, "counter reset");

        limiter.failure(ip(), start);
        limiter.failure(ip(), start);
        let later = start + Duration::from_secs(61);
        assert_eq!(
            limiter.check(ip(), later),
            Decision::Allow,
            "window aged out"
        );
    }

    #[test]
    fn lockouts_double_with_repeated_offences_and_are_bounded() {
        let limiter = FailureLimiter::new(CONFIG);
        let start = Instant::now();
        for _ in 0..3 {
            limiter.failure(ip(), start);
        }
        let first = match limiter.check(ip(), start) {
            Decision::Blocked(wait) => wait,
            Decision::Allow => panic!("expected a lockout"),
        };
        for _ in 0..10 {
            limiter.failure(ip(), start);
        }
        let capped = match limiter.check(ip(), start) {
            Decision::Blocked(wait) => wait,
            Decision::Allow => panic!("expected a longer lockout"),
        };
        assert!(capped >= first);
        assert!(capped <= CONFIG.lockout * 8);
    }

    #[test]
    fn the_state_stays_bounded_under_many_addresses() {
        let limiter = FailureLimiter::new(CONFIG);
        let start = Instant::now();
        for last in 1..=MAX_ENTRIES + 100 {
            let address = IpAddr::from([10, 0, (last / 256) as u8, (last % 256) as u8]);
            limiter.failure(address, start);
        }
        assert!(limiter.lock().len() <= MAX_ENTRIES);
    }
}
