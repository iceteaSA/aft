use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

#[cfg(not(test))]
const DEFAULT_COLD_BUILD_LIMIT: usize = 2;
#[cfg(test)]
const DEFAULT_COLD_BUILD_LIMIT: usize = 1024;

static GLOBAL_COLD_BUILD_LIMITER: LazyLock<Arc<ColdBuildLimiter>> =
    LazyLock::new(|| Arc::new(ColdBuildLimiter::new(DEFAULT_COLD_BUILD_LIMIT)));

pub fn try_acquire() -> Option<ColdBuildPermit> {
    GLOBAL_COLD_BUILD_LIMITER.try_acquire()
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
