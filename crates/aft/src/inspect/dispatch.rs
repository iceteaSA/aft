use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender};

use super::job::{InspectCategory, InspectJob, InspectResult};

pub type InspectWorker = Arc<dyn Fn(InspectJob) -> InspectResult + Send + Sync + 'static>;

#[derive(Clone)]
pub struct DispatchHandles {
    pub request_tx: Sender<InspectJob>,
    pub result_rx: Receiver<InspectResult>,
    pub pool: Arc<rayon::ThreadPool>,
}

pub fn start_dispatch_loop(worker: InspectWorker) -> DispatchHandles {
    let (request_tx, request_rx) = unbounded::<InspectJob>();
    let (result_tx, result_rx) = unbounded::<InspectResult>();
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(default_pool_size())
            .thread_name(|index| format!("aft-inspect-{index}"))
            .build()
            .expect("inspect worker pool must build"),
    );

    let loop_pool = Arc::clone(&pool);
    thread::spawn(move || dispatch_loop(request_rx, result_tx, loop_pool, worker));

    DispatchHandles {
        request_tx,
        result_rx,
        pool,
    }
}

pub fn default_worker() -> InspectWorker {
    Arc::new(dispatch_category)
}

fn dispatch_loop(
    request_rx: Receiver<InspectJob>,
    result_tx: Sender<InspectResult>,
    pool: Arc<rayon::ThreadPool>,
    worker: InspectWorker,
) {
    while let Ok(job) = request_rx.recv() {
        let tx = result_tx.clone();
        let worker = Arc::clone(&worker);
        pool.spawn(move || {
            let result = worker(job);
            let _ = tx.send(result);
        });
    }
}

fn dispatch_category(job: InspectJob) -> InspectResult {
    use crate::inspect::scanners;

    match job.category {
        InspectCategory::Todos => scanners::todos::run_todos_scan(&job),
        InspectCategory::Metrics => scanners::metrics::run_metrics_scan(&job),
        InspectCategory::DeadCode => scanners::dead_code::run_dead_code_scan(&job),
        InspectCategory::UnusedExports => scanners::unused_exports::run_unused_exports_scan(&job),
        InspectCategory::Duplicates => scanners::duplicates::run_duplicates_scan(&job),
        InspectCategory::Diagnostics => {
            let started = Instant::now();
            InspectResult::failed(
                &job,
                "diagnostics: implementation pending (v0.34)",
                started.elapsed(),
            )
        }
        other => {
            let started = Instant::now();
            InspectResult::failed(
                &job,
                format!("inspect category '{other}' is not active in v0.33"),
                started.elapsed(),
            )
        }
    }
}

fn default_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .div_ceil(2)
        .clamp(1, 8)
}
