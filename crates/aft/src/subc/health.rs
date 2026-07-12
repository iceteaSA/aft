//! Dispatch-path metrics and health-report helpers for the subc transport loop.

use super::{
    json, Arc, AtomicU64, AtomicUsize, Duration, Executor, HashMap, HealthReport, HealthStatus,
    Instant, Ordering, PendingBind, RootHealthSnapshot, RouteChannel, Value,
    DISPATCH_PATH_BIND_WARN_AFTER, WRITER_QUEUE_CAPACITY,
};
use crate::context::RootHealthState;
use crate::executor::BindBlockerSnapshot;

pub(super) struct DispatchPathMetrics {
    pub(super) origin: Instant,
    pub(super) frame_loop_last_tick_ms: AtomicU64,
    pub(super) writer_queued: AtomicUsize,
    pub(super) writer_saturation_count: AtomicU64,
    pub(super) control_completion_queued: AtomicUsize,
    pub(super) maintenance_queued: AtomicUsize,
    pub(super) bash_deferred_queued: AtomicUsize,
    pub(super) bash_poll_touch_queued: AtomicUsize,
    pub(super) reliable_push_budget_deferrals: AtomicU64,
    pub(super) maintenance_budget_deferrals: AtomicU64,
    pub(super) response_tasks_live: AtomicUsize,
}

impl DispatchPathMetrics {
    pub(super) fn new() -> Self {
        Self {
            origin: Instant::now(),
            frame_loop_last_tick_ms: AtomicU64::new(0),
            writer_queued: AtomicUsize::new(0),
            writer_saturation_count: AtomicU64::new(0),
            control_completion_queued: AtomicUsize::new(0),
            maintenance_queued: AtomicUsize::new(0),
            bash_deferred_queued: AtomicUsize::new(0),
            bash_poll_touch_queued: AtomicUsize::new(0),
            reliable_push_budget_deferrals: AtomicU64::new(0),
            maintenance_budget_deferrals: AtomicU64::new(0),
            response_tasks_live: AtomicUsize::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        duration_millis_u64(self.origin.elapsed())
    }

    pub(super) fn mark_frame_loop_tick(&self) {
        self.frame_loop_last_tick_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    fn snapshot(
        &self,
        pending_binds: &HashMap<RouteChannel, PendingBind>,
        executor: &Executor,
    ) -> Value {
        let now = Instant::now();
        let oldest_pending_age_ms = pending_binds
            .values()
            .map(|bind| duration_millis_u64(now.saturating_duration_since(bind.started_at)))
            .max();
        let last_tick_ms = self.frame_loop_last_tick_ms.load(Ordering::Relaxed);
        json!({
            "frame_loop": {
                "last_tick_age_ms": self.now_ms().saturating_sub(last_tick_ms),
            },
            "pending_binds": {
                "count": pending_binds.len(),
                "oldest_age_ms": oldest_pending_age_ms,
            },
            "completion_channels": {
                "control": self.control_completion_queued.load(Ordering::Relaxed),
                "maintenance": self.maintenance_queued.load(Ordering::Relaxed),
                "bash_deferred": self.bash_deferred_queued.load(Ordering::Relaxed),
                "bash_poll_touch": self.bash_poll_touch_queued.load(Ordering::Relaxed),
            },
            "budget_deferrals": {
                "reliable_push": self.reliable_push_budget_deferrals.load(Ordering::Relaxed),
                "maintenance": self.maintenance_budget_deferrals.load(Ordering::Relaxed),
            },
            "writer": {
                "queued": self.writer_queued.load(Ordering::Relaxed),
                "capacity": WRITER_QUEUE_CAPACITY,
                "saturation_count": self.writer_saturation_count.load(Ordering::Relaxed),
            },
            "response_tasks": {
                "live": self.response_tasks_live.load(Ordering::Relaxed),
            },
            "mutating_lanes": mutating_lanes_metrics(executor),
        })
    }
}

pub(super) struct ResponseTaskGuard {
    metrics: Arc<DispatchPathMetrics>,
}

impl ResponseTaskGuard {
    pub(super) fn new(metrics: &Arc<DispatchPathMetrics>) -> Self {
        metrics.response_tasks_live.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics: Arc::clone(metrics),
        }
    }
}

impl Drop for ResponseTaskGuard {
    fn drop(&mut self) {
        self.metrics
            .response_tasks_live
            .fetch_sub(1, Ordering::Relaxed);
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

pub(super) fn warn_slow_pending_binds(
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    executor: &Executor,
) {
    let now = Instant::now();
    for (route, pending) in pending_binds.iter_mut() {
        if pending.warned_half_deadline {
            continue;
        }
        let age = now.saturating_duration_since(pending.started_at);
        if age < DISPATCH_PATH_BIND_WARN_AFTER {
            continue;
        }
        pending.warned_half_deadline = true;
        let snapshot = executor
            .try_bind_blocker_snapshot(&pending.bind_root_id, &pending.configure_request_id)
            .unwrap_or_else(|| BindBlockerSnapshot {
                configure_state: "scheduler_busy",
                configure_phase_timings: None,
                blockers: vec!["scheduler_busy".to_string()],
            });
        crate::slog_warn!(
            "{}",
            pending_bind_breadcrumb(
                *route,
                &pending.bind_root_id,
                age,
                &pending.configure_request_id,
                &snapshot,
            )
        );
    }
}

fn pending_bind_breadcrumb(
    route: RouteChannel,
    root_id: &crate::path_identity::ProjectRootId,
    age: Duration,
    configure_request_id: &str,
    snapshot: &BindBlockerSnapshot,
) -> String {
    let blockers = if snapshot.blockers.is_empty() {
        "none".to_string()
    } else {
        snapshot.blockers.join(", ")
    };
    let phase_timings = snapshot
        .configure_phase_timings
        .as_deref()
        .unwrap_or("unavailable");
    format!(
        "subc attach: pending RouteBind route {route} for root {} crossed {}ms (configure_request_id={}, configure_state={}, configure_phase_timings=[{}], blockers=[{}])",
        root_id.as_path().display(),
        duration_millis_u64(age),
        configure_request_id,
        snapshot.configure_state,
        phase_timings,
        blockers,
    )
}

fn mutating_lanes_metrics(executor: &Executor) -> Value {
    match executor.try_mutating_lane_snapshots() {
        Some(snapshots) => Value::Array(
            snapshots
                .into_iter()
                .map(|snapshot| {
                    json!({
                        "root": snapshot.root_id.as_path().to_string_lossy(),
                        "request_id": snapshot.request_id,
                        "job": snapshot.command,
                        "started_age_ms": snapshot.started_age_ms,
                    })
                })
                .collect(),
        ),
        None => json!({ "scheduler_busy": true }),
    }
}

fn dispatch_liveness_metrics(executor: &Executor) -> Value {
    match executor.try_dispatch_liveness_snapshot() {
        Some(snapshot) => json!({
            "interactive": {
                "queued": snapshot.interactive.queued,
                "oldest_age_ms": snapshot.interactive.oldest_age_ms,
            },
            "maintenance": {
                "queued": snapshot.maintenance.queued,
                "oldest_age_ms": snapshot.maintenance.oldest_age_ms,
            },
            "running": {
                "interactive": snapshot.running.interactive,
                "maintenance": snapshot.running.maintenance,
            },
            "interactive_reserve": snapshot.interactive_reserve,
            "maintenance_cap": snapshot.maintenance_cap,
        }),
        None => json!({ "scheduler_busy": true }),
    }
}

pub(super) fn build_health_report(
    executor: &Executor,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    dispatch_path_metrics: &DispatchPathMetrics,
) -> HealthReport {
    // Health replies must stay cheap under any load (subc-health rule: a
    // probe that queues behind busy state lies about liveness and gets the
    // module health-killed). Every read below is try-lock-only: a contended
    // scheduler lock degrades to an empty root list instead of waiting.
    let mut roots: Vec<RootHealthSnapshot> = executor
        .try_actor_entries()
        .unwrap_or_default()
        .into_iter()
        .map(|(root_id, ctx)| ctx.try_health_snapshot(root_id.as_path()))
        .collect();
    roots.sort_by(|left, right| left.project_root.cmp(&right.project_root));

    // Health-verdict rule: DEGRADED means dispatch is impaired (an actor we
    // could not even snapshot without contention), never "a background index
    // is still warming". A serving root with search/callgraph mid-build is
    // healthy — component build states are informational detail, otherwise a
    // module with any active mason worktree reads permanently degraded and
    // the daemon's on-failing policies treat routine warmup as wreckage.
    let busy_roots = roots
        .iter()
        .filter(|root| matches!(root.state, RootHealthState::Busy))
        .count();
    let warming_roots = roots
        .iter()
        .filter(|root| !matches!(root.state, RootHealthState::Busy) && !root.is_fully_ready())
        .count();
    let detail = if busy_roots > 0 {
        Some(format!(
            "{busy_roots} root actor(s) could not be snapshotted without contention"
        ))
    } else if warming_roots > 0 {
        Some(format!(
            "{warming_roots} root(s) warming background indexes (serving normally)"
        ))
    } else {
        None
    };

    HealthReport {
        status: if busy_roots > 0 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Ok
        },
        detail,
        metrics: Some(json!({
            "actor_count": roots.iter().map(|root| root.actor_count).sum::<usize>(),
            "root_count": roots.len(),
            "roots": roots,
            "dispatch_liveness": dispatch_liveness_metrics(executor),
            "dispatch_path": dispatch_path_metrics.snapshot(pending_binds, executor),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{test_ctx, test_root};
    use super::super::{Lane, Response};
    use super::*;
    use serde_json::json;

    #[test]
    fn pending_bind_breadcrumb_names_every_blocker_class() {
        let (_dir, root) = test_root("breadcrumb-blockers");
        let cases = [
            "queued_behind_configure(2)",
            "queued_behind_maintenance(job=subc-maintenance-drain-watcher lane=Mutating root=/tmp/a age_ms=1)",
            "waiting_on_readers",
            "idle_workers==0(job=subc-bind-other lane=Mutating root=/tmp/b age_ms=2)",
        ];

        for blocker in cases {
            let breadcrumb = pending_bind_breadcrumb(
                7,
                &root,
                Duration::from_secs(6),
                "subc-bind-7",
                &BindBlockerSnapshot {
                    configure_state: "queued",
                    configure_phase_timings: Some("artifact_owner_claim=12ms".to_string()),
                    blockers: vec![blocker.to_string()],
                },
            );
            assert!(
                breadcrumb.contains(blocker),
                "breadcrumb omitted blocker class: {breadcrumb}"
            );
            assert!(
                breadcrumb.contains("configure_phase_timings=[artifact_owner_claim=12ms]"),
                "breadcrumb omitted configure phase timings: {breadcrumb}"
            );
        }
    }

    #[test]
    fn health_report_includes_nonblocking_dispatch_liveness_for_queued_interactive() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 2,
            drr_quantum: 1,
        });
        let (_dir_a, root_a) = test_root("health-liveness-a");
        let (_dir_b, root_b) = test_root("health-liveness-b");
        let (_dir_c, root_c) = test_root("health-liveness-c");
        executor.register_actor(root_a.clone(), test_ctx());
        executor.register_actor(root_b.clone(), test_ctx());
        executor.register_actor(root_c.clone(), test_ctx());

        let (started_tx, started_rx) = crossbeam_channel::bounded(2);
        let (release_tx, release_rx) = crossbeam_channel::bounded(2);
        let mut blockers = Vec::new();
        for (index, root) in [root_a, root_b].into_iter().enumerate() {
            let started_tx = started_tx.clone();
            let release_rx = release_rx.clone();
            blockers.push(executor.submit(
                root,
                Lane::PureRead,
                format!("health-blocker-{index}"),
                Box::new(move |_| {
                    started_tx.send(index).expect("signal blocker start");
                    release_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("release blocker");
                    Response::success(format!("blocker-{index}"), json!({ "ok": true }))
                }),
            ));
        }
        for _ in 0..2 {
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocker starts");
        }

        let queued = executor.submit(
            root_c,
            Lane::PureRead,
            "queued-interactive".to_string(),
            Box::new(|_| Response::success("queued-interactive", json!({ "ok": true }))),
        );
        std::thread::sleep(Duration::from_millis(75));

        let metrics = DispatchPathMetrics::new();
        let pending_binds = HashMap::new();
        let report = build_health_report(&executor, &pending_binds, &metrics);
        let dispatch = report
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.get("dispatch_liveness"))
            .expect("dispatch_liveness metric");
        assert_eq!(dispatch.get("scheduler_busy"), None);
        assert_eq!(dispatch["interactive"]["queued"].as_u64(), Some(1));
        assert!(dispatch["interactive"]["oldest_age_ms"].as_u64().is_some());

        for _ in 0..2 {
            release_tx.send(()).expect("release blocker");
        }
        for blocker in blockers {
            blocker
                .recv_timeout(Duration::from_secs(1))
                .expect("blocker completion response");
        }
        queued
            .recv_timeout(Duration::from_secs(1))
            .expect("queued completion response");
    }

    #[test]
    fn health_snapshot_fast_fails_while_mutating_job_holds_component_lock() {
        let executor = Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        });
        let (_dir, root) = test_root("health-mutating-lock");
        let ctx = test_ctx();
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let blocker = executor.submit(
            root,
            Lane::Mutating,
            "health-lock-blocker".to_string(),
            Box::new(move |ctx| {
                let _index = ctx
                    .search_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                started_tx.send(()).expect("signal held health lock");
                std::thread::sleep(Duration::from_secs(2));
                Response::success("health-lock-blocker", json!({ "ok": true }))
            }),
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("mutating lock holder starts");

        let started = Instant::now();
        let report = build_health_report(&executor, &HashMap::new(), &DispatchPathMetrics::new());
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(10),
            "health snapshot waited behind a mutating lock for {elapsed:?}"
        );
        assert_eq!(report.status, HealthStatus::Degraded);

        blocker
            .recv_timeout(Duration::from_secs(3))
            .expect("mutating lock holder completes");
    }
}
