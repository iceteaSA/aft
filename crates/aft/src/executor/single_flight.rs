use std::{collections::HashMap, hash::Hash, sync::Arc};

use parking_lot::{Condvar, Mutex};

/// Generation-aware single-flight cache.
///
/// Calls for the same key and generation share one in-flight build. A newer
/// generation supersedes an older in-flight build; the older result is not
/// installed if the entry has moved on by the time it finishes.
pub struct SingleFlight<K, T> {
    inner: Mutex<HashMap<K, FlightEntry<T>>>,
    changed: Condvar,
}

enum FlightEntry<T> {
    Building { generation: u64 },
    Ready { generation: u64, value: Arc<T> },
}

impl<K, T> Default for SingleFlight<K, T>
where
    K: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, T> SingleFlight<K, T>
where
    K: Clone + Eq + Hash,
{
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
        }
    }

    /// Return the cached value for `id` at `generation`, or build it once.
    ///
    /// The build function runs outside the internal lock. Concurrent callers for
    /// the same `(id, generation)` wait for the in-flight build and receive the
    /// installed value. If a newer generation supersedes this call while its
    /// build is running, this call returns the newer ready value instead of
    /// overwriting it with stale work.
    pub fn get_or_build(&self, id: K, generation: u64, build_fn: impl FnOnce() -> T) -> Arc<T> {
        let mut build_fn = Some(build_fn);

        loop {
            let mut guard = self.inner.lock();
            match guard.get(&id) {
                Some(FlightEntry::Ready {
                    generation: ready_generation,
                    value,
                }) if *ready_generation >= generation => return Arc::clone(value),
                Some(FlightEntry::Building {
                    generation: building_generation,
                }) if *building_generation >= generation => {
                    self.changed.wait(&mut guard);
                }
                _ => {
                    guard.insert(id.clone(), FlightEntry::Building { generation });
                    drop(guard);

                    let build = build_fn
                        .take()
                        .expect("single-flight build function used more than once");
                    let built = Arc::new(build());

                    let mut guard = self.inner.lock();
                    match guard.get(&id) {
                        Some(FlightEntry::Building {
                            generation: current_generation,
                        }) if *current_generation == generation => {
                            guard.insert(
                                id.clone(),
                                FlightEntry::Ready {
                                    generation,
                                    value: Arc::clone(&built),
                                },
                            );
                            self.changed.notify_all();
                            return built;
                        }
                        Some(FlightEntry::Ready {
                            generation: current_generation,
                            value,
                        }) if *current_generation >= generation => {
                            let value = Arc::clone(value);
                            self.changed.notify_all();
                            return value;
                        }
                        _ => {
                            self.changed.notify_all();
                        }
                    }
                }
            }
        }
    }
}
