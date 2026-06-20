use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use super::*;
use crate::{
    config::Config, parser::TreeSitterProvider, path_identity::ProjectRootId, protocol::Response,
};

fn ok(id: impl Into<String>) -> Response {
    Response::success(id, serde_json::json!({"ok": true}))
}

fn test_ctx() -> Arc<AppContext> {
    Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config::default(),
    ))
}

fn test_root(label: &str) -> (TempDir, ProjectRootId) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("aft-executor-{label}-"))
        .tempdir()
        .expect("create temp actor root");
    let root = ProjectRootId::from_path(dir.path()).expect("canonicalize actor root");
    (dir, root)
}

fn test_executor(
    pool_size: usize,
    read_cap: usize,
    actor_cap: usize,
    heavy_permits: usize,
) -> Executor {
    Executor::with_config(ExecutorConfig {
        pool_size,
        read_cap,
        actor_cap,
        heavy_permits,
        drr_quantum: 1,
    })
}

fn observe_max(max_seen: &AtomicUsize, value: usize) {
    let mut current = max_seen.load(Ordering::Acquire);
    while value > current {
        match max_seen.compare_exchange(current, value, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

#[test]
fn cross_actor_isolation() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir_a, root_a) = test_root("isolation-a");
    let (_dir_b, root_b) = test_root("isolation-b");
    executor.register_actor(root_a.clone(), test_ctx());
    executor.register_actor(root_b.clone(), test_ctx());

    let (a_started_tx, a_started_rx) = crossbeam_channel::bounded(1);
    let (release_a_tx, release_a_rx) = crossbeam_channel::bounded(1);
    let a_done = Arc::new(AtomicUsize::new(0));
    let a_done_job = Arc::clone(&a_done);

    let a_handle = executor.submit(
        root_a,
        Lane::HeavyInit,
        Box::new(move |_| {
            a_started_tx.send(()).expect("signal heavy start");
            release_a_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release heavy actor");
            a_done_job.store(1, Ordering::Release);
            ok("heavy-a")
        }),
    );
    a_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("actor A heavy job starts");

    let (b_done_tx, b_done_rx) = crossbeam_channel::bounded(1);
    let b_handle = executor.submit(
        root_b,
        Lane::PureRead,
        Box::new(move |_| {
            b_done_tx.send(()).expect("signal B read done");
            ok("read-b")
        }),
    );

    b_done_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("actor B read completes while actor A heavy job is still running");
    assert_eq!(a_done.load(Ordering::Acquire), 0);
    b_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("B completion response");

    release_a_tx.send(()).expect("release actor A heavy job");
    a_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("A completion response");
}

#[test]
fn within_actor_read_concurrency() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir, root) = test_root("read-concurrency");
    executor.register_actor(root.clone(), test_ctx());

    let read_count = 6;
    let current_reads = Arc::new(AtomicUsize::new(0));
    let max_reads = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = crossbeam_channel::bounded(read_count);
    let (release_tx, release_rx) = crossbeam_channel::bounded(read_count);
    let mut handles = Vec::new();

    for index in 0..read_count {
        let current_reads = Arc::clone(&current_reads);
        let max_reads = Arc::clone(&max_reads);
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        handles.push(executor.submit(
            root.clone(),
            Lane::PureRead,
            Box::new(move |_| {
                let now = current_reads.fetch_add(1, Ordering::AcqRel) + 1;
                observe_max(&max_reads, now);
                started_tx.send(index).expect("signal read start");
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release read job");
                current_reads.fetch_sub(1, Ordering::AcqRel);
                ok(format!("read-{index}"))
            }),
        ));
    }

    for _ in 0..executor.read_cap() {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("initial read admitted up to cap");
    }
    assert!(started_rx.recv_timeout(Duration::from_millis(75)).is_err());

    for _ in 0..read_count {
        release_tx.send(()).expect("release read token");
    }
    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("read completion response");
    }

    assert_eq!(max_reads.load(Ordering::Acquire), executor.read_cap());
}

#[test]
fn drr_fairness() {
    let executor = test_executor(4, 3, 3, 2);
    let (_dir_a, root_a) = test_root("drr-a");
    let (_dir_b, root_b) = test_root("drr-b");
    executor.register_actor(root_a.clone(), test_ctx());
    executor.register_actor(root_b.clone(), test_ctx());

    let flood_count = 20;
    let (a_started_tx, a_started_rx) = crossbeam_channel::bounded(flood_count);
    let (release_a_tx, release_a_rx) = crossbeam_channel::bounded(flood_count);
    let mut a_handles = Vec::new();

    for index in 0..flood_count {
        let a_started_tx = a_started_tx.clone();
        let release_a_rx = release_a_rx.clone();
        a_handles.push(executor.submit(
            root_a.clone(),
            Lane::PureRead,
            Box::new(move |_| {
                a_started_tx.send(index).expect("signal A flood start");
                release_a_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release A flood job");
                ok(format!("a-{index}"))
            }),
        ));
    }

    for _ in 0..executor.actor_cap() {
        a_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor A fills only its actor cap");
    }

    let (b_started_tx, b_started_rx) = crossbeam_channel::bounded(1);
    let b_handle = executor.submit(
        root_b,
        Lane::PureRead,
        Box::new(move |_| {
            b_started_tx.send(()).expect("signal B start");
            ok("b")
        }),
    );

    b_started_rx
        .recv_timeout(Duration::from_millis(300))
        .expect("actor B is scheduled within a bounded DRR round despite A flood");
    b_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("B completion response");

    for _ in 0..flood_count {
        release_a_tx.send(()).expect("release A flood token");
    }
    for handle in a_handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("A completion response");
    }
}

#[test]
fn heavy_bound() {
    let executor = test_executor(6, 3, 5, 2);
    let job_count = 6;
    let mut roots = Vec::new();
    let mut dirs = Vec::new();
    for index in 0..job_count {
        let (dir, root) = test_root(&format!("heavy-{index}"));
        executor.register_actor(root.clone(), test_ctx());
        dirs.push(dir);
        roots.push(root);
    }

    let current_heavy = Arc::new(AtomicUsize::new(0));
    let max_heavy = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = crossbeam_channel::bounded(job_count);
    let (release_tx, release_rx) = crossbeam_channel::bounded(job_count);
    let mut handles = Vec::new();

    for (index, root) in roots.into_iter().enumerate() {
        let current_heavy = Arc::clone(&current_heavy);
        let max_heavy = Arc::clone(&max_heavy);
        let started_tx = started_tx.clone();
        let release_rx = release_rx.clone();
        handles.push(executor.submit(
            root,
            Lane::HeavyInit,
            Box::new(move |_| {
                let now = current_heavy.fetch_add(1, Ordering::AcqRel) + 1;
                observe_max(&max_heavy, now);
                started_tx.send(index).expect("signal heavy start");
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release heavy job");
                current_heavy.fetch_sub(1, Ordering::AcqRel);
                ok(format!("heavy-{index}"))
            }),
        ));
    }

    for _ in 0..executor.heavy_permits() {
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("heavy job admitted up to semaphore bound");
    }
    assert!(started_rx.recv_timeout(Duration::from_millis(75)).is_err());

    for _ in 0..job_count {
        release_tx.send(()).expect("release heavy token");
    }
    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("heavy completion response");
    }

    assert_eq!(max_heavy.load(Ordering::Acquire), executor.heavy_permits());
    assert_eq!(dirs.len(), job_count);
}

#[test]
fn single_flight() {
    let flight = Arc::new(SingleFlight::<String, usize>::new());
    let build_count = Arc::new(AtomicUsize::new(0));
    let racers = 16;
    let barrier = Arc::new(std::sync::Barrier::new(racers));
    let mut threads = Vec::new();

    for _ in 0..racers {
        let flight = Arc::clone(&flight);
        let build_count = Arc::clone(&build_count);
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            flight.get_or_build("resource".to_string(), 7, || {
                build_count.fetch_add(1, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(50));
                42
            })
        }));
    }

    for thread in threads {
        let value = thread.join().expect("single-flight racer joins");
        assert_eq!(*value, 42);
    }
    assert_eq!(build_count.load(Ordering::Acquire), 1);
}

#[test]
fn mutator_drains_then_exclusive() {
    let executor = test_executor(4, 2, 3, 2);
    let (_dir, root) = test_root("mutator");
    executor.register_actor(root.clone(), test_ctx());

    let current_reads = Arc::new(AtomicUsize::new(0));
    let (read_started_tx, read_started_rx) = crossbeam_channel::bounded(2);
    let (release_reads_tx, release_reads_rx) = crossbeam_channel::bounded(2);
    let mut read_handles = Vec::new();

    for index in 0..2 {
        let current_reads = Arc::clone(&current_reads);
        let read_started_tx = read_started_tx.clone();
        let release_reads_rx = release_reads_rx.clone();
        read_handles.push(executor.submit(
            root.clone(),
            Lane::PureRead,
            Box::new(move |_| {
                current_reads.fetch_add(1, Ordering::AcqRel);
                read_started_tx.send(index).expect("signal read start");
                release_reads_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release read before mutator");
                current_reads.fetch_sub(1, Ordering::AcqRel);
                ok(format!("read-{index}"))
            }),
        ));
    }

    for _ in 0..2 {
        read_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("read starts before mutator");
    }

    let (mutator_started_tx, mutator_started_rx) = crossbeam_channel::bounded(1);
    let (release_mutator_tx, release_mutator_rx) = crossbeam_channel::bounded(1);
    let reads_at_mutator = Arc::clone(&current_reads);
    let mutator_handle = executor.submit(
        root.clone(),
        Lane::Mutating,
        Box::new(move |_| {
            mutator_started_tx
                .send(reads_at_mutator.load(Ordering::Acquire))
                .expect("signal mutator start");
            release_mutator_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release mutator");
            ok("mutator")
        }),
    );

    let (late_read_started_tx, late_read_started_rx) = crossbeam_channel::bounded(1);
    let late_read_handle = executor.submit(
        root,
        Lane::PureRead,
        Box::new(move |_| {
            late_read_started_tx
                .send(())
                .expect("signal late read start");
            ok("late-read")
        }),
    );

    assert!(mutator_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());
    assert!(late_read_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());

    for _ in 0..2 {
        release_reads_tx.send(()).expect("release initial read");
    }
    for handle in read_handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("initial read completion response");
    }

    let observed_reads = mutator_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("mutator starts after reads drain");
    assert_eq!(observed_reads, 0);
    assert!(late_read_started_rx
        .recv_timeout(Duration::from_millis(75))
        .is_err());

    release_mutator_tx.send(()).expect("release mutator");
    mutator_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("mutator completion response");
    late_read_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("late read starts after mutator completes");
    late_read_handle
        .recv_timeout(Duration::from_secs(1))
        .expect("late read completion response");
}

#[test]
fn no_dispatch_of_nonrunnable() {
    let executor = test_executor(5, 2, 2, 2);
    let (_dir_a, root_a) = test_root("random-a");
    let (_dir_b, root_b) = test_root("random-b");
    executor.register_actor(root_a.clone(), test_ctx());
    executor.register_actor(root_b.clone(), test_ctx());

    let total_jobs = 96;
    let (done_tx, done_rx) = crossbeam_channel::bounded(total_jobs);
    let mut handles = Vec::new();
    let mut state = 0x5eed_u64;

    for index in 0..total_jobs {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let root = if state & 1 == 0 {
            root_a.clone()
        } else {
            root_b.clone()
        };
        let lane = match index % 4 {
            0 => Lane::PureRead,
            1 => Lane::SerialLspStatus,
            2 => Lane::HeavyInit,
            _ => Lane::Mutating,
        };
        let done_tx = done_tx.clone();
        let sleep_for = Duration::from_micros(200 + (state % 7) * 100);
        handles.push(executor.submit(
            root,
            lane,
            Box::new(move |_| {
                thread::sleep(sleep_for);
                done_tx.send(index).expect("signal randomized job done");
                ok(format!("random-{index}"))
            }),
        ));
    }

    let started_at = Instant::now();
    for completed in 0..total_jobs {
        assert!(
            started_at.elapsed() < Duration::from_secs(6),
            "randomized scheduler run exceeded wall-clock watchdog"
        );
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| {
                panic!("no global executor progress after {completed} randomized completions")
            });
    }

    for handle in handles {
        handle
            .recv_timeout(Duration::from_secs(1))
            .expect("randomized completion response");
    }

    assert_eq!(executor.nonrunnable_dispatch_count(), 0);
}
