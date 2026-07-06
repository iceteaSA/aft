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
    let started = Instant::now();
    let mut logged = false;
    loop {
        if let Some(permit) = GLOBAL_COLD_BUILD_LIMITER.try_acquire() {
            if logged {
                crate::slog_info!(
                    "maintenance build slot acquired after {}ms wait: {}",
                    started.elapsed().as_millis(),
                    kind
                );
            }
            return permit;
        }
        if !logged {
            crate::slog_info!(
                "maintenance build queued behind concurrency cap ({}): {}",
                GLOBAL_COLD_BUILD_LIMITER.limit(),
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

#[derive(Debug)]
struct ColdBuildLimiter {
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

    // Both tests mutate the process-global limiter; run them one at a time.
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
}
