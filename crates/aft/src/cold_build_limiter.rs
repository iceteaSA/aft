use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

#[cfg(not(test))]
const DEFAULT_COLD_BUILD_LIMIT: usize = 2;
#[cfg(test)]
const DEFAULT_COLD_BUILD_LIMIT: usize = 1024;

static GLOBAL_COLD_BUILD_LIMITER: LazyLock<Arc<ColdBuildLimiter>> =
    LazyLock::new(|| Arc::new(ColdBuildLimiter::new(DEFAULT_COLD_BUILD_LIMIT)));

pub fn try_acquire() -> Option<ColdBuildPermit> {
    GLOBAL_COLD_BUILD_LIMITER.try_acquire()
}

/// Block until a build slot is free, then take it.
///
/// For build sites with no reschedule path (search-index builds spawn once per
/// configure): skipping would strand the index, so past-cap work waits instead.
/// Production captures showed concurrent per-root builds starving dispatch
/// while CPU sat idle; waiting serializes that pressure at the source. Only
/// call from dedicated background threads, never the dispatch thread or an
/// executor worker.
pub fn acquire_blocking(kind: &str) -> ColdBuildPermit {
    acquire_blocking_while(kind, || true).expect("unconditional cold-build admission")
}

/// Wait for a build slot while `admitted` remains true. The predicate is checked
/// before every attempt, so a root that becomes unbound does not consume a slot
/// after spending time queued behind the process-wide cap.
pub fn acquire_blocking_while(kind: &str, admitted: impl Fn() -> bool) -> Option<ColdBuildPermit> {
    acquire_blocking_while_with_limiter(&GLOBAL_COLD_BUILD_LIMITER, kind, admitted)
}

fn acquire_blocking_while_with_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    admitted: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    let started = Instant::now();
    let mut logged = false;
    loop {
        if !admitted() {
            return None;
        }
        if let Some(permit) = limiter.try_acquire() {
            // The root can become unbound after the pre-attempt check but
            // before the permit CAS succeeds. Recheck while owning the slot;
            // dropping the permit here returns it before any build starts.
            if !admitted() {
                drop(permit);
                return None;
            }
            if logged {
                crate::slog_info!(
                    "maintenance build slot acquired after {}ms wait: {}",
                    started.elapsed().as_millis(),
                    kind
                );
            }
            return Some(permit);
        }
        if !logged {
            crate::slog_info!(
                "maintenance build queued behind concurrency cap ({}): {}",
                limiter.limit(),
                kind
            );
            logged = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn limit() -> usize {
    GLOBAL_COLD_BUILD_LIMITER.limit()
}

#[cfg(test)]
pub(crate) fn test_limiter(limit: usize) -> Arc<ColdBuildLimiter> {
    Arc::new(ColdBuildLimiter::new(limit))
}

#[cfg(test)]
pub(crate) fn acquire_blocking_while_with_test_limiter(
    limiter: &Arc<ColdBuildLimiter>,
    kind: &str,
    admitted: impl Fn() -> bool,
) -> Option<ColdBuildPermit> {
    acquire_blocking_while_with_limiter(limiter, kind, admitted)
}

#[derive(Debug)]
pub(crate) struct ColdBuildLimiter {
    available: AtomicUsize,
    limit: usize,
}

impl ColdBuildLimiter {
    fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            available: AtomicUsize::new(limit),
            limit,
        }
    }

    fn limit(&self) -> usize {
        self.limit
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ColdBuildPermit> {
        loop {
            let available = self.available.load(Ordering::Acquire);
            if available == 0 {
                return None;
            }
            if self
                .available
                .compare_exchange(
                    available,
                    available - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(ColdBuildPermit {
                    limiter: Arc::clone(self),
                });
            }
        }
    }
}

#[derive(Debug)]
pub struct ColdBuildPermit {
    limiter: Arc<ColdBuildLimiter>,
}

impl Drop for ColdBuildPermit {
    fn drop(&mut self) {
        let previous = self.limiter.available.fetch_add(1, Ordering::Release);
        debug_assert!(previous < self.limiter.limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate the process-global limiter; run them one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn permits_release_on_drop() {
        let _serial = serial();
        let before = GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire);
        {
            let _a = acquire_blocking("test-a");
            let _b = acquire_blocking("test-b");
            assert_eq!(
                GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire),
                before - 2
            );
        }
        assert_eq!(
            GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire),
            before
        );
    }

    #[test]
    fn acquire_blocking_waits_until_release() {
        let _serial = serial();
        // Drain every slot, then prove a waiter blocks until one holder drops.
        let mut held: Vec<ColdBuildPermit> = Vec::new();
        while let Some(permit) = try_acquire() {
            held.push(permit);
        }
        let waiter = std::thread::spawn(|| {
            let _p = acquire_blocking("waiter");
        });
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!waiter.is_finished(), "waiter must block while cap is full");
        drop(held.pop());
        waiter.join().expect("waiter finishes after release");
        drop(held);
    }

    #[test]
    fn admission_revoked_between_check_and_permit_drops_the_slot() {
        let _serial = serial();
        let before = GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire);
        let checks = AtomicUsize::new(0);

        let permit = acquire_blocking_while("revoked-after-cas", || {
            checks.fetch_add(1, Ordering::SeqCst) == 0
        });

        assert!(permit.is_none());
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert_eq!(
            GLOBAL_COLD_BUILD_LIMITER.available.load(Ordering::Acquire),
            before,
            "revoked admission must return the just-acquired slot"
        );
    }

    #[test]
    fn conditional_waiter_cancels_without_consuming_a_released_slot() {
        let _serial = serial();
        let mut held = Vec::new();
        while let Some(permit) = try_acquire() {
            held.push(permit);
        }
        let admitted = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let waiter_admitted = Arc::clone(&admitted);
        let waiter = std::thread::spawn(move || {
            acquire_blocking_while("conditional waiter", || {
                waiter_admitted.load(Ordering::SeqCst)
            })
        });
        std::thread::sleep(Duration::from_millis(150));
        admitted.store(false, Ordering::SeqCst);
        assert!(
            waiter.join().expect("conditional waiter joins").is_none(),
            "revoked work must leave the cold-build queue without taking a permit"
        );
        drop(held);
    }
}
