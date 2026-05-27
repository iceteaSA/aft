use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender};
use rayon::prelude::*;

use super::job::{InspectJob, InspectResult, InspectScanSuccess};

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
    Arc::new(run_empty_scan)
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

fn run_empty_scan(job: InspectJob) -> InspectResult {
    let started = Instant::now();
    let files_scanned = job
        .scope_files
        .par_iter()
        .map(|path| {
            let _parser = tree_sitter::Parser::new();
            usize::from(path.is_file())
        })
        .sum::<usize>();

    let aggregate = serde_json::json!({
        "count": 0,
        "items": [],
        "files_scanned": files_scanned,
    });
    let success = InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions: Vec::new(),
        aggregate,
    };
    InspectResult::success(&job, success, started.elapsed())
}

fn default_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .div_ceil(2)
        .clamp(1, 8)
}
