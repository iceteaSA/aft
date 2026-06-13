mod cli;
use aft::bash_background::BgTaskRegistry;
use aft::config::Config;
use aft::context::{
    AppContext, SemanticIndexEvent, SemanticIndexStatus, SemanticRefreshEvent,
    SemanticRefreshRequest, StatusBarCounts,
};
use aft::log_ctx;
use aft::lsp::client::LspEvent;
use aft::parser::TreeSitterProvider;
use aft::protocol::{EchoParams, PushFrame, RawRequest, Response};
use aft::watcher_filter::{watcher_path_is_infra_skip, WatcherDispatchEvent};
use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    // Handle --version flag before anything else
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("aft {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if std::env::args().nth(1).as_deref() == Some("migrate-storage") {
        let args = std::env::args_os().skip(2).collect::<Vec<_>>();
        match aft::migrate_storage::parse_cli_args(args) {
            Ok(args) => {
                let status = aft::migrate_storage::run_with_options(
                    args,
                    aft::migrate_storage::Options::default(),
                );
                std::process::exit(i32::from(status.code()));
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(2);
            }
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let prefix = if record.target().starts_with("aft::lsp")
                || record.target().starts_with("aft_lsp")
            {
                "[aft-lsp]"
            } else {
                "[aft]"
            };
            writeln!(buf, "{} {}", prefix, record.args())
        })
        .init();

    if std::env::args().nth(1).as_deref() == Some("warmup") {
        let args = std::env::args_os().skip(2).collect::<Vec<_>>();
        match cli::warmup::run(args) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(error.exit_code());
            }
        }
    }

    aft::slog_info!("started, pid {}", std::process::id());

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    install_signal_handler(ctx.bash_background().clone(), ctx.lsp_child_registry());

    // Install bash output-compression closure on the BgTaskRegistry. The
    // closure captures the shared filter-registry handle and the shared
    // compress-flag (atomic) so the watchdog thread can compress without
    // touching the rest of AppContext. The flag is updated from `configure`
    // when `experimental.bash.compress` changes; the filter registry is
    // updated when `reset_filter_registry` is called.
    {
        let filter_registry_handle = ctx.shared_filter_registry();
        let compress_flag = ctx.bash_compress_flag();
        ctx.bash_background().set_compressor_with_exit_code(
            move |command: &str, output: String, exit_code: Option<i32>| {
                if !compress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return aft::compress::CompressionResult::new(output);
                }
                let registry_guard = match filter_registry_handle.read() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                aft::compress::compress_with_registry_exit_code(
                    command,
                    &output,
                    exit_code,
                    &registry_guard,
                )
            },
        );
    }

    let stdout_writer = ctx.stdout_writer();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let shutdown_from_push = Arc::clone(&shutdown_requested);
    ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame: PushFrame| {
        let Ok(mut writer) = stdout_writer.lock() else {
            aft::slog_error!("stdout push frame lock poisoned; shutting down bridge");
            shutdown_from_push.store(true, Ordering::SeqCst);
            return;
        };
        write_push_frame_or_request_shutdown(&mut *writer, &frame, &shutdown_from_push);
    }))));

    // Stdin is read by a dedicated thread that forwards lines through a
    // channel. The main thread does recv_timeout so it wakes periodically
    // even when no agent traffic is arriving — that periodic wake runs
    // the drain_* functions so background-build channel events (e.g.
    // SemanticIndexEvent::Ready) get processed and their status_changed
    // push frames emitted. Without the wake, the sidebar can stay stuck
    // on "loading" indefinitely until the next request happens to arrive.
    const DRAIN_INTERVAL: Duration = Duration::from_millis(250);
    let (line_tx, line_rx) = mpsc::channel::<io::Result<String>>();
    thread::spawn(move || {
        let stdin = io::stdin();
        let reader = stdin.lock();
        for line_result in reader.lines() {
            if line_tx.send(line_result).is_err() {
                break;
            }
        }
    });

    loop {
        if shutdown_requested.load(Ordering::SeqCst) {
            break;
        }

        let line_result = match line_rx.recv_timeout(DRAIN_INTERVAL) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic drain so push frames flow even without requests.
                // Cheap on the idle path: each drain just checks try_recv
                // on a channel and bails if empty.
                drain_configure_warning_events(&ctx);
                drain_search_index_events(&ctx);
                drain_callgraph_store_events(&ctx);
                drain_semantic_index_events(&ctx);
                drain_semantic_refresh_events(&ctx);
                drain_inspect_events(&ctx);
                drain_watcher_events(&ctx);
                drain_lsp_events(&ctx);
                if shutdown_requested.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                aft::slog_error!("stdin read error: {}", e);
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut shutdown_after_response = false;
        let response = match serde_json::from_str::<RawRequest>(trimmed) {
            Ok(req) => {
                // Drain search index FIRST so watcher events apply to the latest index.
                // If reversed, watcher updates applied to the old index would be lost
                // when the background-built index replaces it.
                drain_configure_warning_events(&ctx);
                drain_search_index_events(&ctx);
                drain_callgraph_store_events(&ctx);
                drain_semantic_index_events(&ctx);
                drain_semantic_refresh_events(&ctx);
                drain_inspect_events(&ctx);
                drain_watcher_events(&ctx);
                drain_lsp_events(&ctx);
                let request_id = req.id.clone();
                let session_id = req.session().to_string();
                let command = req.command.clone();
                let session_id_for_log = req.session_id.clone();
                let dispatch_result = catch_unwind(AssertUnwindSafe(|| {
                    log_ctx::with_session(session_id_for_log, || dispatch(req, &ctx))
                }));
                match dispatch_result {
                    Ok(mut response) => {
                        attach_bg_completions(&mut response, &ctx, &session_id, &command);
                        attach_status_bar(&mut response, &ctx, &command);
                        response
                    }
                    Err(payload) => {
                        shutdown_after_response = true;
                        dispatch_panic_response(request_id, &command, payload.as_ref())
                    }
                }
            }
            Err(e) => {
                aft::slog_error!("parse error: {} — input: {}", e, trimmed);
                Response::error(
                    "_parse_error",
                    "parse_error",
                    format!("failed to parse request: {}", e),
                )
            }
        };

        if let Err(e) = write_response(&ctx, &response) {
            aft::slog_error!("stdout write error: {}", e);
            break;
        }
        drain_configure_warning_events(&ctx);
        if shutdown_after_response || shutdown_requested.load(Ordering::SeqCst) {
            break;
        }
    }

    ctx.lsp().shutdown_all();
    ctx.bash_background().detach();
    aft::slog_info!("stdin closed, shutting down");
}

#[cfg(unix)]
fn install_signal_handler(
    bg_registry: BgTaskRegistry,
    lsp_children: aft::lsp::child_registry::LspChildRegistry,
) {
    let signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ]);
    let Ok(mut signals) = signals else {
        if let Err(error) = signals {
            aft::slog_error!("failed to install signal handlers: {error}");
        }
        return;
    };

    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            // Plugin restarts can SIGTERM the bridge while background bash jobs
            // are still running. Detach first so child handles are not killed by
            // Rust drop glue and can be rehydrated from disk.
            bg_registry.detach();
            // Kill LSP children synchronously before exit. Without this, LSP
            // child processes (typescript-language-server, biome lsp-proxy,
            // etc.) get orphaned to PID 1 because process::exit bypasses the
            // graceful shutdown path that LspManager::shutdown_all uses on
            // the natural stdin-closed exit. Graceful shutdown takes up to
            // 5s per server (shutdown request + exit notification + poll),
            // which is too slow for a signal handler — we SIGKILL instead.
            let killed = lsp_children.kill_all();
            if killed > 0 {
                aft::slog_info!("signal {}: killed {} LSP child process(es)", signal, killed);
            }
            std::process::exit(128 + signal);
        }
    });
}

#[cfg(not(unix))]
static WINDOWS_SIGNAL_REGISTRIES: std::sync::OnceLock<(
    BgTaskRegistry,
    aft::lsp::child_registry::LspChildRegistry,
)> = std::sync::OnceLock::new();

#[cfg(windows)]
unsafe extern "system" fn windows_console_handler(ctrl_type: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    if matches!(
        ctrl_type,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        if let Some((bg_registry, lsp_children)) = WINDOWS_SIGNAL_REGISTRIES.get() {
            bg_registry.detach();
            let killed = lsp_children.kill_all();
            if killed > 0 {
                aft::slog_info!(
                    "windows console event {ctrl_type}: killed {killed} LSP child process(es)"
                );
            }
        }
        1
    } else {
        0
    }
}

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(not(unix))]
fn install_signal_handler(
    bg_registry: BgTaskRegistry,
    lsp_children: aft::lsp::child_registry::LspChildRegistry,
) {
    #[cfg(windows)]
    {
        let _ = WINDOWS_SIGNAL_REGISTRIES.set((bg_registry, lsp_children));
        // SAFETY: registers a process-global console-control callback. The
        // callback only uses cloneable registries stored in OnceLock.
        let ok = unsafe { SetConsoleCtrlHandler(Some(windows_console_handler), 1) };
        if ok == 0 {
            aft::slog_error!("failed to install Windows console control handler");
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (bg_registry, lsp_children);
    }
}

fn write_push_frame_or_request_shutdown(
    writer: &mut impl Write,
    frame: &PushFrame,
    shutdown_requested: &AtomicBool,
) {
    if let Err(error) = write_push_frame(writer, frame) {
        aft::slog_error!(
            "stdout push frame write error: {}; shutting down bridge",
            error
        );
        shutdown_requested.store(true, Ordering::SeqCst);
    }
}

fn dispatch_panic_response(
    request_id: impl Into<String>,
    command: &str,
    payload: &(dyn std::any::Any + Send),
) -> Response {
    let panic_message = panic_payload_message(payload);
    aft::slog_error!(
        "command '{}' panicked: {}; shutting down bridge",
        command,
        panic_message
    );
    Response::error(
        request_id,
        "internal_error",
        format!("command '{command}' panicked: {panic_message}"),
    )
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn drain_configure_warning_events(ctx: &AppContext) {
    for (generation, frame) in ctx.drain_configure_warnings() {
        if ctx.configure_generation() != generation {
            aft::slog_info!(
                "dropping stale configure_warnings for generation {} (current {})",
                generation,
                ctx.configure_generation()
            );
            continue;
        }

        if let Some(sender) = ctx.progress_sender_handle() {
            sender(PushFrame::ConfigureWarnings(frame));
        }
    }
}

fn drain_inspect_events(ctx: &AppContext) {
    let drained = ctx.inspect_manager().drain_completions();
    // Watcher-driven Tier-2 scans complete via the reuse path, which bypasses
    // `result_rx`/`drain_completions`. Poll the manager's reuse counter so a
    // background scan still refreshes the bar (#3) — otherwise the counts and
    // `~` marker would only update on a manual `aft_inspect`.
    let reuse_completed = ctx.take_new_reuse_completions();
    // A completed background Tier-2 scan refreshes the agent status-bar counts
    // to the freshly-persisted aggregate, and clears the stale marker — so the
    // bar reflects the new numbers on the next tool result without waiting for
    // an explicit aft_inspect call.
    if drained > 0 || reuse_completed {
        if let Some(project_root) = ctx.config().project_root.clone() {
            let (dead_code, unused_exports, duplicates) = ctx
                .inspect_manager()
                .latest_tier2_counts(ctx.inspect_dir(), project_root);
            // Don't clear the `~` stale marker until the whole serial Tier-2
            // cycle has drained — while any category is still in flight the
            // already-persisted categories may predate the latest edit, so
            // claiming fresh would be premature (#20). `None` counts preserve
            // the last-known value rather than fabricating a `0` (#1).
            let stale = ctx.inspect_manager().tier2_any_in_flight();
            ctx.update_status_bar_tier2(dead_code, unused_exports, duplicates, None, stale);
            // Push the refreshed snapshot so the sidebar reflects the new Tier-2
            // counts immediately. `update_status_bar_tier2` only mutates the
            // in-memory counts (which the agent status bar reads live on each
            // tool result); the push-driven sidebar would otherwise keep showing
            // the pre-population snapshot — where `status_bar` was null and the
            // Code Health section stayed hidden — until some unrelated event
            // happened to emit a status frame.
            ctx.status_emitter().signal(ctx.build_status_snapshot());
        }
    }
}

fn attach_bg_completions(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    command: &str,
) {
    if matches!(
        command,
        "configure"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    if !ctx
        .bash_background()
        .has_completions_for_session(Some(session_id))
    {
        return;
    }
    let completions = ctx
        .bash_background()
        .drain_completions_for_session(Some(session_id));
    if completions.is_empty() {
        return;
    }
    let value = serde_json::json!(completions);
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("bg_completions".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "bg_completions": value });
        }
    }
}

/// Attach the agent status-bar counts to the response envelope so the plugin
/// after-hook can surface the IDE-style status bar (emit-on-change). Skips
/// internal/transport commands that don't represent agent tool calls (their
/// responses never reach the agent, and bash-lifecycle commands fire rapidly).
/// `errors`/`warnings` are read live from the LSP store here; Tier-2/todos are
/// last-known. Omitted entirely until the Tier-2 cache is populated once.
fn status_bar_last_emitted() -> &'static Mutex<Option<StatusBarCounts>> {
    static LAST_EMITTED_STATUS_BAR: OnceLock<Mutex<Option<StatusBarCounts>>> = OnceLock::new();
    LAST_EMITTED_STATUS_BAR.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn reset_status_bar_emission_for_test() {
    if let Ok(mut last) = status_bar_last_emitted().lock() {
        *last = None;
    }
}

fn attach_status_bar(response: &mut Response, ctx: &AppContext, command: &str) {
    if matches!(
        command,
        "configure"
            | "ping"
            | "version"
            | "status"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    let Some(counts) = ctx.status_bar_counts() else {
        return;
    };
    match status_bar_last_emitted().lock() {
        Ok(mut last) => {
            if last.as_ref() == Some(&counts) {
                return;
            }
            *last = Some(counts.clone());
        }
        Err(_) => {
            // If the fingerprint lock is poisoned, prefer the previous behavior
            // (emit) over accidentally suppressing a changed status bar.
        }
    }
    let value = serde_json::json!({
        "errors": counts.errors,
        "warnings": counts.warnings,
        "dead_code": counts.dead_code,
        "unused_exports": counts.unused_exports,
        "duplicates": counts.duplicates,
        "todos": counts.todos,
        "tier2_stale": counts.tier2_stale,
    });
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("status_bar".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "status_bar": value });
        }
    }
}

fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    match req.command.as_str() {
        "ping" => Response::success(&req.id, serde_json::json!({ "command": "pong" })),
        "version" => Response::success(
            &req.id,
            serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
        ),
        "echo" => handle_echo(&req),
        "bash" => aft::commands::bash::handle(&req, ctx),
        "bash_drain_completions" => aft::commands::bash_drain_completions::handle(&req, ctx),
        "bash_ack_completions" => aft::commands::bash_drain_completions::handle_ack(&req, ctx),
        "bash_status" => aft::commands::bash_status::handle(&req, ctx),
        "bash_notify" => aft::commands::bash_notify::handle(&req, ctx),
        "bash_unnotify" => aft::commands::bash_notify::handle_unnotify(&req, ctx),
        "bash_promote" => aft::commands::bash_promote::handle(&req, ctx),
        "bash_kill" => aft::commands::bash_kill::handle(&req, ctx),
        "bash_write" => aft::commands::bash_write::handle(&req, ctx),
        "db_get_state" => aft::commands::state::handle_db_get_state(&req, ctx),
        "db_set_state" => aft::commands::state::handle_db_set_state(&req, ctx),
        "db_get_host_state" => aft::commands::state::handle_db_get_host_state(&req, ctx),
        "db_set_host_state" => aft::commands::state::handle_db_set_host_state(&req, ctx),
        "outline" => aft::commands::outline::handle_outline(&req, ctx),
        "zoom" => aft::commands::zoom::handle_zoom(&req, ctx),
        "read" => aft::commands::read::handle_read(&req, ctx),
        "undo" => aft::commands::undo::handle_undo(&req, ctx),
        "undo_preview" => aft::commands::undo::handle_undo_preview(&req, ctx),
        "edit_history" => aft::commands::edit_history::handle_edit_history(&req, ctx),
        "checkpoint" => aft::commands::checkpoint::handle_checkpoint(&req, ctx),
        "checkpoint_paths" => aft::commands::checkpoint::handle_checkpoint_paths(&req, ctx),
        "restore_checkpoint" => {
            aft::commands::restore_checkpoint::handle_restore_checkpoint(&req, ctx)
        }
        "list_checkpoints" => aft::commands::list_checkpoints::handle_list_checkpoints(&req, ctx),
        "write" => aft::commands::write::handle_write(&req, ctx),
        "delete_file" => aft::commands::delete_file::handle_delete_file(&req, ctx),
        "move_file" => aft::commands::move_file::handle_move_file(&req, ctx),
        "edit_symbol" => aft::commands::edit_symbol::handle_edit_symbol(&req, ctx),
        "edit_match" => aft::commands::edit_match::handle_edit_match(&req, ctx),
        "batch" => aft::commands::batch::handle_batch(&req, ctx),
        "add_import" => aft::commands::add_import::handle_add_import(&req, ctx),
        "remove_import" => aft::commands::remove_import::handle_remove_import(&req, ctx),
        "organize_imports" => aft::commands::organize_imports::handle_organize_imports(&req, ctx),
        "configure" => aft::commands::configure::handle_configure(&req, ctx),
        "glob" => aft::commands::glob::handle_glob(&req, ctx),
        "grep" => aft::commands::grep::handle_grep(&req, ctx),
        "semantic_search" => {
            if let Some(response) = wait_for_semantic_index_before_search(&req, ctx) {
                response
            } else {
                aft::commands::semantic_search::handle_semantic_search(&req, ctx)
            }
        }
        "status" => aft::commands::status::handle_status(&req, ctx),
        "list_filters" => aft::commands::list_filters::handle_list_filters(&req, ctx),
        "trust_filter_project" => {
            aft::commands::trust_filter_project::handle_trust_filter_project(&req, ctx)
        }
        "untrust_filter_project" => {
            aft::commands::untrust_filter_project::handle_untrust_filter_project(&req, ctx)
        }
        "call_tree" => aft::commands::call_tree::handle_call_tree(&req, ctx),
        "callers" => aft::commands::callers::handle_callers(&req, ctx),
        "trace_to" => aft::commands::trace_to::handle_trace_to(&req, ctx),
        "trace_to_symbol" => aft::commands::trace_to_symbol::handle_trace_to_symbol(&req, ctx),
        "impact" => aft::commands::impact::handle_impact(&req, ctx),
        "trace_data" => aft::commands::trace_data::handle_trace_data(&req, ctx),
        "move_symbol" => aft::commands::move_symbol::handle_move_symbol(&req, ctx),
        "extract_function" => aft::commands::extract_function::handle_extract_function(&req, ctx),
        "inline_symbol" => aft::commands::inline_symbol::handle_inline_symbol(&req, ctx),
        "inspect" => aft::commands::inspect::handle_inspect(&req, ctx),
        "inspect_tier2_run" => aft::commands::inspect::handle_inspect_tier2_run(&req, ctx),
        "git_conflicts" => aft::commands::conflicts::handle_git_conflicts(ctx, &req),
        "ast_search" => aft::commands::ast_search::handle_ast_search(&req, ctx),
        "ast_replace" => aft::commands::ast_replace::handle_ast_replace(&req, ctx),
        "lsp_diagnostics" => aft::commands::lsp_diagnostics::handle_lsp_diagnostics(&req, ctx),
        "lsp_inspect" => aft::commands::lsp_inspect::handle_lsp_inspect(&req, ctx),
        "lsp_hover" => aft::commands::lsp_hover::handle_lsp_hover(&req, ctx),
        "lsp_goto_definition" => {
            aft::commands::lsp_goto_definition::handle_lsp_goto_definition(&req, ctx)
        }
        "lsp_find_references" => {
            aft::commands::lsp_find_references::handle_lsp_find_references(&req, ctx)
        }
        "lsp_prepare_rename" => {
            aft::commands::lsp_prepare_rename::handle_lsp_prepare_rename(&req, ctx)
        }
        "lsp_rename" => aft::commands::lsp_rename::handle_lsp_rename(&req, ctx),
        // NOTE: "snapshot" must remain in the production binary because integration tests in
        // crates/aft/tests/integration/ spawn the compiled binary as a subprocess and send
        // "snapshot" commands through the stdin/stdout protocol. A #[cfg(test)] gate would
        // only affect unit-test compilation and would not exclude this arm from the binary
        // that integration tests execute. See: crates/aft/tests/integration/safety_test.rs
        "snapshot" => handle_snapshot(&req, ctx),
        _ => {
            aft::slog_warn!("unknown command: {}", req.command);
            Response::error(
                &req.id,
                "unknown_command",
                format!("unknown command: {}", req.command),
            )
        }
    }
}

fn handle_echo(req: &RawRequest) -> Response {
    match serde_json::from_value::<EchoParams>(req.params.clone()) {
        Ok(params) => Response::success(&req.id, serde_json::json!({ "message": params.message })),
        Err(e) => Response::error(
            &req.id,
            "invalid_request",
            format!("echo: invalid params: {}", e),
        ),
    }
}

/// Test-only command: snapshot a file into the backup store.
///
/// Params: `file` (string, required) — path to snapshot.
/// Returns: `{ backup_id }`.
fn wait_for_semantic_index_before_search(req: &RawRequest, ctx: &AppContext) -> Option<Response> {
    if std::env::var_os("AFT_WAIT_FOR_SEMANTIC_READY").is_none() || !ctx.config().semantic_search {
        return None;
    }

    let timeout_ms = std::env::var("AFT_WAIT_FOR_SEMANTIC_READY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(600_000);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        drain_search_index_events(ctx);
        drain_semantic_index_events(ctx);

        match ctx.semantic_index_status().borrow().clone() {
            SemanticIndexStatus::Ready { .. }
            | SemanticIndexStatus::Disabled
            | SemanticIndexStatus::Failed(_) => return None,
            SemanticIndexStatus::Building { stage, .. } => {
                if Instant::now() >= deadline {
                    return Some(Response::error(
                        &req.id,
                        "semantic_index_timeout",
                        format!(
                            "semantic index did not become ready before semantic_search within {timeout_ms}ms (stage: {stage})"
                        ),
                    ));
                }
            }
        }

        thread::sleep(Duration::from_millis(250));
    }
}

fn handle_snapshot(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "snapshot: missing required param 'file'",
            );
        }
    };

    let path = match ctx.validate_path(&req.id, std::path::Path::new(file)) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let path = path.as_path();
    let mut backup = ctx.backup().borrow_mut();

    match backup.snapshot(req.session(), path, "manual snapshot") {
        Ok(id) => Response::success(&req.id, serde_json::json!({ "backup_id": id })),
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn write_response(ctx: &AppContext, response: &Response) -> io::Result<()> {
    let stdout_writer = ctx.stdout_writer();
    let mut writer = stdout_writer
        .lock()
        .map_err(|_| io::Error::other("stdout writer lock poisoned"))?;
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_push_frame(writer: &mut impl Write, frame: &PushFrame) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, frame)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Source file extensions that the call graph supports.
const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi", "rs", "go",
];

/// A `tsconfig.json` / `jsconfig.json` (including variant names like
/// `tsconfig.base.json`). A change to any of these can shift TypeScript build
/// membership (which files `tsc` checks), so the status-bar membership cache
/// must be invalidated. Deliberately broad on the variant suffix and ignorant
/// of `extends` graphs: the cache is cleared wholesale on a match, and base
/// configs almost always follow the `tsconfig*.json` naming. Non-standard base
/// names are covered on the next `tsconfig.json` change or `configure`.
fn watcher_path_is_tsconfig(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n == "tsconfig.json"
                || n == "jsconfig.json"
                || ((n.starts_with("tsconfig.") || n.starts_with("jsconfig."))
                    && n.ends_with(".json"))
        })
        .unwrap_or(false)
}

fn watcher_path_is_source(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// A file the callgraph STORE would have indexed at cold-build time. The store
/// indexes every file `walk_project_files` yields (i.e. any detected language),
/// not just the trigram `SOURCE_EXTENSIONS` set. Gating the store's watcher
/// refresh on the narrower trigram set left edits to Java/C/C++/C#/Kotlin/Ruby/
/// PHP/… (all of which the store extracts calls for) serving stale results until
/// a full rebuild. Mirror cold-build exactly so refresh coverage == index
/// coverage.
fn watcher_path_is_callgraph_indexed(path: &std::path::Path) -> bool {
    aft::parser::detect_language(path).is_some()
}

fn watcher_path_is_ignored_by_current_matcher(ctx: &AppContext, path: &std::path::Path) -> bool {
    if watcher_path_is_infra_skip(path) {
        return true;
    }

    if let Some(matcher) = ctx.gitignore() {
        if path.starts_with(matcher.path()) {
            let is_dir = path.is_dir();
            return matcher
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore();
        }
    }

    false
}

fn replay_search_index_pending_updates(
    ctx: &AppContext,
    index: &mut aft::search_index::SearchIndex,
    pending_paths: Vec<std::path::PathBuf>,
) {
    for path in pending_paths {
        if path.exists() {
            if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                index.remove_file(&path);
            } else {
                index.update_file(&path);
            }
        } else {
            index.remove_file(&path);
        }
    }
}

fn semantic_corpus_refresh_in_progress(ctx: &AppContext) -> bool {
    matches!(
        &*ctx.semantic_index_status().borrow(),
        SemanticIndexStatus::Building { stage, .. } if stage == "refreshing_corpus"
    )
}

fn watcher_path_is_semantic_source(path: &std::path::Path) -> bool {
    aft::semantic_index::is_semantic_indexed_extension(path)
}

const MAX_RETRY_ATTEMPTS: usize = 6;
const BREAKER_TRIP_THRESHOLD: usize = 3;

static SEMANTIC_REFRESH_CONSECUTIVE_TRANSIENT_FAILURES: AtomicUsize = AtomicUsize::new(0);
static SEMANTIC_REFRESH_CIRCUIT_OPEN: AtomicBool = AtomicBool::new(false);
static SEMANTIC_REFRESH_PROBE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static SEMANTIC_REFRESH_PROBE_READY: AtomicBool = AtomicBool::new(false);

fn semantic_refresh_retry_attempts() -> &'static Mutex<BTreeMap<std::path::PathBuf, usize>> {
    static ATTEMPTS: OnceLock<Mutex<BTreeMap<std::path::PathBuf, usize>>> = OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Backoff for live semantic refresh retries after a transient embedding backend
/// failure. Mirrors the cold-build retry cadence (15s -> 30s -> 60s capped) so
/// a down backend cannot spin the watcher/refresh loop hot while still
/// self-healing once the backend returns.
fn semantic_refresh_retry_backoff(attempt: usize) -> Duration {
    // Test seam, intentionally matching the build-level retry override.
    if let Ok(raw) = std::env::var("AFT_SEMANTIC_RETRY_BACKOFF_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return Duration::from_millis(ms);
        }
    }
    const SCHEDULE_SECS: [u64; 3] = [15, 30, 60];
    let secs = SCHEDULE_SECS
        .get(attempt)
        .copied()
        .unwrap_or(*SCHEDULE_SECS.last().unwrap());
    Duration::from_secs(secs)
}

struct SemanticRefreshRetryPlan {
    retry_paths: Vec<std::path::PathBuf>,
    capped_paths: Vec<std::path::PathBuf>,
    delay: Option<Duration>,
}

fn next_semantic_refresh_retry_plan(paths: Vec<std::path::PathBuf>) -> SemanticRefreshRetryPlan {
    let mut retry_paths = Vec::new();
    let mut capped_paths = Vec::new();
    let mut max_attempt = 0usize;

    let Ok(mut attempts) = semantic_refresh_retry_attempts().lock() else {
        return SemanticRefreshRetryPlan {
            retry_paths: paths,
            capped_paths,
            delay: Some(semantic_refresh_retry_backoff(0)),
        };
    };

    for path in paths {
        let attempt = attempts.get(&path).copied().unwrap_or(0);
        if attempt >= MAX_RETRY_ATTEMPTS {
            capped_paths.push(path);
            continue;
        }
        max_attempt = max_attempt.max(attempt);
        attempts.insert(path.clone(), attempt.saturating_add(1));
        retry_paths.push(path);
    }

    let delay = if retry_paths.is_empty() {
        None
    } else {
        Some(semantic_refresh_retry_backoff(max_attempt))
    };

    SemanticRefreshRetryPlan {
        retry_paths,
        capped_paths,
        delay,
    }
}

fn clear_semantic_refresh_retry_attempts(paths: &[std::path::PathBuf]) {
    if let Ok(mut attempts) = semantic_refresh_retry_attempts().lock() {
        for path in paths {
            attempts.remove(path);
        }
    }
}

fn clear_all_semantic_refresh_retry_attempts() {
    if let Ok(mut attempts) = semantic_refresh_retry_attempts().lock() {
        attempts.clear();
    }
}

fn clear_completed_pending_semantic_index_paths(
    ctx: &AppContext,
    completed_paths: &[std::path::PathBuf],
) {
    if completed_paths.is_empty() {
        return;
    }

    let completed = completed_paths.iter().cloned().collect::<HashSet<_>>();
    let remaining = ctx
        .take_pending_semantic_index_paths()
        .into_iter()
        .filter(|path| !completed.contains(path))
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        ctx.add_pending_semantic_index_paths(remaining);
    }
}

fn semantic_refresh_probe_delay() -> Duration {
    semantic_refresh_retry_backoff(usize::MAX)
}

fn semantic_refresh_circuit_is_open() -> bool {
    SEMANTIC_REFRESH_CIRCUIT_OPEN.load(Ordering::SeqCst)
}

fn record_semantic_refresh_transient_failure() -> bool {
    let failures = SEMANTIC_REFRESH_CONSECUTIVE_TRANSIENT_FAILURES
        .fetch_add(1, Ordering::SeqCst)
        .saturating_add(1);
    if failures >= BREAKER_TRIP_THRESHOLD
        && !SEMANTIC_REFRESH_CIRCUIT_OPEN.swap(true, Ordering::SeqCst)
    {
        aft::slog_warn!(
            "embedding backend appears down; suspending active retries, will resume on next change or successful probe"
        );
    }
    semantic_refresh_circuit_is_open()
}

fn reset_semantic_refresh_transient_failure_count() {
    SEMANTIC_REFRESH_CONSECUTIVE_TRANSIENT_FAILURES.store(0, Ordering::SeqCst);
}

fn reset_semantic_refresh_circuit_after_success() {
    reset_semantic_refresh_transient_failure_count();
    SEMANTIC_REFRESH_PROBE_READY.store(false, Ordering::SeqCst);
    if SEMANTIC_REFRESH_CIRCUIT_OPEN.swap(false, Ordering::SeqCst) {
        aft::slog_info!("embedding backend recovered; resuming normal refresh retries");
    }
}

fn mark_semantic_refresh_success(ctx: &AppContext, completed_paths: &[std::path::PathBuf]) {
    clear_semantic_refresh_retry_attempts(completed_paths);
    clear_completed_pending_semantic_index_paths(ctx, completed_paths);
    reset_semantic_refresh_circuit_after_success();
}

fn mark_semantic_corpus_refresh_success() {
    clear_all_semantic_refresh_retry_attempts();
    reset_semantic_refresh_circuit_after_success();
}

#[cfg(test)]
fn reset_semantic_refresh_retry_state_for_test() {
    clear_all_semantic_refresh_retry_attempts();
    SEMANTIC_REFRESH_CONSECUTIVE_TRANSIENT_FAILURES.store(0, Ordering::SeqCst);
    SEMANTIC_REFRESH_CIRCUIT_OPEN.store(false, Ordering::SeqCst);
    SEMANTIC_REFRESH_PROBE_IN_FLIGHT.store(false, Ordering::SeqCst);
    SEMANTIC_REFRESH_PROBE_READY.store(false, Ordering::SeqCst);
}

#[cfg(test)]
fn semantic_refresh_transient_failure_count_for_test() -> usize {
    SEMANTIC_REFRESH_CONSECUTIVE_TRANSIENT_FAILURES.load(Ordering::SeqCst)
}

#[cfg(test)]
fn semantic_refresh_probe_is_scheduled_for_test() -> bool {
    SEMANTIC_REFRESH_PROBE_IN_FLIGHT.load(Ordering::SeqCst)
        || SEMANTIC_REFRESH_PROBE_READY.load(Ordering::SeqCst)
}

fn ensure_semantic_refresh_probe_scheduled() {
    if SEMANTIC_REFRESH_PROBE_READY.load(Ordering::SeqCst) {
        return;
    }
    if SEMANTIC_REFRESH_PROBE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    if SEMANTIC_REFRESH_PROBE_READY.load(Ordering::SeqCst) {
        SEMANTIC_REFRESH_PROBE_IN_FLIGHT.store(false, Ordering::SeqCst);
        return;
    }

    let delay = semantic_refresh_probe_delay();
    let session_id = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            thread::sleep(delay);
            SEMANTIC_REFRESH_PROBE_READY.store(true, Ordering::SeqCst);
            SEMANTIC_REFRESH_PROBE_IN_FLIGHT.store(false, Ordering::SeqCst);
        });
    });
}

fn maybe_fire_semantic_refresh_probe(ctx: &AppContext) {
    if !SEMANTIC_REFRESH_PROBE_READY.swap(false, Ordering::SeqCst) {
        return;
    }
    if !semantic_refresh_circuit_is_open() {
        return;
    }

    let pending_paths = ctx.take_pending_semantic_index_paths();
    if pending_paths.is_empty() {
        return;
    }

    let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
        sender
            .send(SemanticRefreshRequest::Files {
                paths: pending_paths.clone(),
            })
            .is_ok()
    });
    if !sent {
        ctx.add_pending_semantic_index_paths(pending_paths);
    }
}

fn schedule_semantic_refresh_retry(
    ctx: &AppContext,
    paths: Vec<std::path::PathBuf>,
    error: &str,
) -> bool {
    if paths.is_empty() {
        return false;
    }
    let Some(sender) = ctx.semantic_refresh_sender() else {
        return false;
    };

    let SemanticRefreshRetryPlan {
        retry_paths,
        capped_paths,
        delay,
    } = next_semantic_refresh_retry_plan(paths);

    if !capped_paths.is_empty() {
        aft::slog_warn!(
            "semantic refresh retry limit reached for {} file(s); preserving for next watcher/configure refresh",
            capped_paths.len(),
        );
        ctx.add_pending_semantic_index_paths(capped_paths);
    }

    let Some(delay) = delay else {
        return true;
    };

    let clean = aft::semantic_index::strip_transient_embedding_marker(error);
    aft::slog_warn!(
        "semantic refresh hit a transient backend error ({}); retrying {} file(s) in {}ms",
        clean,
        retry_paths.len(),
        delay.as_millis(),
    );

    let session_id = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            thread::sleep(delay);
            let _ = sender.send(SemanticRefreshRequest::Files { paths: retry_paths });
        });
    });
    true
}

#[cfg(debug_assertions)]
fn delay_search_rebuild_publish_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

#[cfg(not(debug_assertions))]
fn delay_search_rebuild_publish_for_debug() {}

fn spawn_search_corpus_refresh(
    ctx: &AppContext,
    root: std::path::PathBuf,
    config: aft::config::Config,
) {
    if let Some(index) = ctx.search_index().borrow_mut().as_mut() {
        index.ready = false;
    }

    let (tx, rx): (
        crossbeam_channel::Sender<aft::search_index::SearchIndex>,
        crossbeam_channel::Receiver<aft::search_index::SearchIndex>,
    ) = crossbeam_channel::unbounded();
    *ctx.search_index_rx().borrow_mut() = Some(rx);
    ctx.reset_symbol_cache();

    let is_worktree_bridge = ctx.is_worktree_bridge();
    let session_id = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            let cache_dir =
                aft::search_index::resolve_cache_dir(&root, config.storage_dir.as_deref());
            let _cache_lock = if is_worktree_bridge {
                None
            } else {
                match aft::search_index::CacheLock::acquire(&cache_dir) {
                    Ok(lock) => Some(lock),
                    Err(error) => {
                        aft::slog_warn!(
                            "failed to acquire search cache lock for ignore refresh: {}",
                            error
                        );
                        None
                    }
                }
            };
            let index = aft::search_index::SearchIndex::build_with_limit(
                &root,
                config.search_index_max_file_size,
            );
            delay_search_rebuild_publish_for_debug();
            if !is_worktree_bridge {
                index.write_to_disk(&cache_dir, index.stored_git_head());
            }
            let _ = tx.send(index);
        });
    });
}

fn refresh_corpus_after_ignore_change(ctx: &AppContext) -> bool {
    let Some(root) = ctx.canonical_cache_root_opt() else {
        return false;
    };
    let config = ctx.config().clone();
    let mut status_changed = false;

    if let Some(graph) = ctx.callgraph().borrow_mut().as_mut() {
        graph.invalidate_file(&root.join(".gitignore"));
        graph.invalidate_file(&root.join(".aftignore"));
    }

    if !ctx.is_worktree_bridge() {
        if let Some(store) = ctx.callgraph_store().borrow_mut().as_mut() {
            let current_files = aft::callgraph::walk_project_files(&root).collect::<Vec<_>>();
            match store.refresh_corpus(&current_files) {
                Ok(stats) => {
                    aft::slog_info!(
                        "callgraph store corpus refresh after ignore-rule change: {} files, {} edges",
                        stats.files,
                        stats.edges
                    );
                    status_changed = true;
                }
                Err(error) => {
                    aft::slog_warn!(
                        "callgraph store corpus refresh after ignore-rule change failed: {}",
                        error
                    );
                    if let Err(mark_error) = store.mark_files_stale(&current_files) {
                        aft::slog_warn!(
                            "failed to mark callgraph store corpus stale after refresh failure: {}",
                            mark_error
                        );
                    }
                }
            }
        }
    }

    if config.search_index {
        spawn_search_corpus_refresh(ctx, root.clone(), config.clone());
        status_changed = true;
        aft::slog_info!("started search index refresh after ignore-rule change");
    }

    if config.semantic_search {
        if let Some(sender) = ctx.semantic_refresh_sender() {
            *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                stage: "refreshing_corpus".to_string(),
                files: None,
                entries_done: None,
                entries_total: None,
            };
            match sender.send(SemanticRefreshRequest::Corpus) {
                Ok(()) => {
                    status_changed = true;
                }
                Err(error) => {
                    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(
                        format!("semantic corpus refresh worker unavailable: {error}"),
                    );
                    status_changed = true;
                }
            }
        } else if ctx.semantic_index_rx().borrow().is_some() {
            ctx.mark_pending_semantic_corpus_refresh();
        }
    }

    status_changed
}

fn refresh_callgraph_store_for_watcher(ctx: &AppContext, changed: &HashSet<std::path::PathBuf>) {
    if ctx.is_worktree_bridge() {
        return;
    }
    let source_paths = changed
        .iter()
        .filter(|path| watcher_path_is_callgraph_indexed(path))
        .cloned()
        .collect::<Vec<_>>();
    if source_paths.is_empty() {
        return;
    }
    // Converge to the current generation before writing: if another process
    // published a newer one, drop our stale store so the changed paths get
    // recorded as pending and replayed against the fresh store (rather than
    // incrementally written into a superseded generation).
    ctx.revalidate_callgraph_store_generation();
    let mut store_ref = ctx.callgraph_store().borrow_mut();
    let Some(store) = store_ref.as_mut() else {
        // Store not resident yet. If a cold build is in flight, record the
        // changed paths so they're replayed once the freshly-built store lands
        // (otherwise mid-build edits would be silently lost). If no build is
        // running, there's nothing to refresh.
        if ctx.callgraph_store_rx().borrow().is_some() {
            ctx.add_pending_callgraph_store_paths(source_paths);
        }
        return;
    };
    if let Err(error) = store.refresh_files(&source_paths) {
        aft::slog_warn!("callgraph store refresh failed: {}", error);
        match store.mark_files_stale(&source_paths) {
            Ok(marked) => aft::slog_warn!(
                "marked {} callgraph store file(s) stale after refresh failure",
                marked.len()
            ),
            Err(mark_error) => aft::slog_warn!(
                "failed to mark callgraph store files stale after refresh failure: {}",
                mark_error
            ),
        }
    }
}

/// Drain pre-filtered watcher events and apply cache invalidations on the
/// dispatch thread. The watcher filter thread owns notify receive/decode,
/// metadata filtering, ignore matching, root-deleted detection, and path
/// coalescing; this drain only reacts to compact control events and surviving
/// paths because the cache/index state below is not Send.
fn drain_watcher_events(ctx: &AppContext) {
    let mut changed: HashSet<std::path::PathBuf> = HashSet::new();
    let mut ignore_file_changed = false;
    let mut watcher_failed = None;
    let mut root_deleted = false;

    {
        let rx_ref = ctx.watcher_rx().borrow();
        let rx = match rx_ref.as_ref() {
            Some(rx) => rx,
            None => {
                ctx.tick_tier2_refresh_scheduler(0);
                return; // No watcher configured
            }
        };

        loop {
            match rx.try_recv() {
                Ok(WatcherDispatchEvent::Paths(paths)) => {
                    changed.extend(paths);
                }
                Ok(WatcherDispatchEvent::IgnoreRulesChanged { path }) => {
                    ignore_file_changed = true;
                    log::debug!(
                        "watcher: ignore rules changed at {}, rebuilding matcher",
                        path.display()
                    );
                    ctx.rebuild_gitignore();
                }
                Ok(WatcherDispatchEvent::RootDeleted) => {
                    root_deleted = true;
                    break;
                }
                Ok(WatcherDispatchEvent::Error(error)) => {
                    watcher_failed = Some(error);
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    watcher_failed = Some("watcher channel disconnected".to_string());
                    break;
                }
            }
        }
    }

    let mut watcher_status_changed = false;
    if root_deleted {
        ctx.stop_watcher_runtime();
        let _ = ctx.add_degraded_reason("project_root_deleted".to_string());
        aft::slog_warn!(
            "project root deleted; dropping watcher to avoid delete-storm: {:?}",
            ctx.canonical_cache_root_opt()
        );
        watcher_status_changed = true;
        changed.clear();
    } else if let Some(error) = watcher_failed {
        ctx.stop_watcher_runtime();
        let _ = ctx.add_degraded_reason("watcher_unavailable".to_string());
        aft::slog_warn!("watcher unavailable: {}", error);
        watcher_status_changed = true;
    }

    let mut status_changed = watcher_status_changed;
    if ignore_file_changed {
        status_changed |= refresh_corpus_after_ignore_change(ctx);
    }

    let scheduler_changed_path_count = if ignore_file_changed {
        changed.len().max(1)
    } else {
        changed.len()
    };
    if changed.is_empty() {
        if status_changed {
            ctx.status_emitter().signal(ctx.build_status_snapshot());
        }
        ctx.tick_tier2_refresh_scheduler(scheduler_changed_path_count);
        return;
    }

    // A real source change makes the last-known Tier-2 counts stale until the
    // next background scan reconciles them — surface that in the status bar
    // immediately (the `~` marker) so the agent never reads them as live.
    if ctx.mark_status_bar_tier2_stale() {
        status_changed = true;
    }

    // A tsconfig change can shift which files `tsc` checks, which is the policy
    // the status-bar E/W count filters on. Clear the membership cache wholesale
    // so the next bar count re-resolves from disk (handles new nested configs,
    // edited `extends` parents, and deletions without per-key bookkeeping).
    if changed.iter().any(|path| watcher_path_is_tsconfig(path)) {
        ctx.clear_tsconfig_membership_cache();
        status_changed = true;
    }

    if ctx.search_index_rx().borrow().is_some() {
        ctx.add_pending_search_index_paths(changed.iter().cloned());
    }
    let semantic_source_paths = changed
        .iter()
        .filter(|path| watcher_path_is_semantic_source(path))
        .cloned()
        .collect::<Vec<_>>();
    let semantic_build_in_progress = ctx.semantic_index_rx().borrow().is_some();
    let semantic_corpus_refresh_in_progress = semantic_corpus_refresh_in_progress(ctx);
    if (semantic_build_in_progress || semantic_corpus_refresh_in_progress)
        && !semantic_source_paths.is_empty()
    {
        ctx.add_pending_semantic_index_paths(semantic_source_paths.clone());
    }

    if let Ok(mut symbol_cache) = ctx.symbol_cache().write() {
        for path in &changed {
            symbol_cache.invalidate(path);
        }
    }

    // Phase 2: invalidate each changed file in the call graph
    let mut graph_ref = ctx.callgraph().borrow_mut();
    if let Some(graph) = graph_ref.as_mut() {
        for path in &changed {
            if watcher_path_is_source(path) {
                graph.invalidate_file(path);
            }
        }
    }
    drop(graph_ref);
    refresh_callgraph_store_for_watcher(ctx, &changed);

    let mut index_ref = ctx.search_index().borrow_mut();
    if let Some(index) = index_ref.as_mut() {
        for path in &changed {
            if path.exists() {
                index.update_file(path);
            } else {
                index.remove_file(path);
            }
        }
    }

    let mut semantic_index_ref = ctx.semantic_index().borrow_mut();
    let mut semantic_refresh_paths = Vec::new();
    if let Some(index) = semantic_index_ref.as_mut() {
        let mut stale_paths = Vec::new();
        for path in &semantic_source_paths {
            index.invalidate_file(path);
            stale_paths.push(path.clone());
        }
        if !stale_paths.is_empty() {
            let mut status = ctx.semantic_index_status().borrow_mut();
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &stale_paths {
                    status.add_refreshing_file(path.clone());
                }
                semantic_refresh_paths = stale_paths;
                status_changed = true;
            }
        }
    }

    drop(semantic_index_ref);
    drop(index_ref);

    // A vanished file's LSP diagnostics would otherwise linger in the warm set
    // forever (no server republishes for a path that no longer exists),
    // inflating the error/warning counts in the status bar and `aft_inspect`.
    // Clear them here so every deletion source is covered (AFT delete, `rm`,
    // `git checkout`, branch switch) — not just the delete command. The agent
    // status bar reads E/W live from the warm set on each response, so clearing
    // the store is sufficient; the next tool call's bar reflects the new count.
    //
    // Not gated on the trigram `SOURCE_EXTENSIONS` set: any registered LSP
    // server (Bash, YAML, Solidity, Vue, C/C++, custom servers, …) can publish
    // diagnostics for files outside that set, and gating on it left their
    // diagnostics stranded after deletion. `clear_for_file` is a cheap no-op
    // when the store holds nothing for the path, so clearing unconditionally
    // for every vanished path is safe.
    for path in &changed {
        if !path.exists() && ctx.lsp_clear_diagnostics_for_file(path) {
            status_changed = true;
        }
    }

    if !semantic_refresh_paths.is_empty() {
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: semantic_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} refreshing file(s)",
                semantic_refresh_paths.len()
            );
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &semantic_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    aft::slog_info!("invalidated {} files", changed.len());
    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
    ctx.tick_tier2_refresh_scheduler(scheduler_changed_path_count);
}

fn drain_search_index_events(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.search_index_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(index) => latest = Some(index),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    let mut status_changed = false;
    let mut installed_index = false;
    if let Some(mut index) = latest {
        let pending_paths = ctx.take_pending_search_index_paths();
        if !pending_paths.is_empty() {
            replay_search_index_pending_updates(ctx, &mut index, pending_paths);
        }
        *ctx.search_index().borrow_mut() = Some(index);
        installed_index = true;
        status_changed = true;
    }

    if disconnected || installed_index {
        *ctx.search_index_rx().borrow_mut() = None;
        if disconnected && !installed_index {
            let _ = ctx.take_pending_search_index_paths();
        }
        status_changed = true;
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

/// Install a background-built callgraph store once its cold build completes.
/// Mirrors `drain_search_index_events`: drains the receiver, installs the
/// freshest store, replays paths that changed during the build, and clears the
/// receiver. On build failure (channel disconnected with nothing installed) the
/// receiver is cleared so a later op can retry the cold build.
fn drain_callgraph_store_events(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.callgraph_store_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(store) => latest = Some(store),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    let mut status_changed = false;
    let mut installed = false;
    if let Some(store) = latest {
        // Replay source files that changed while the cold build was running so
        // the freshly-installed store reflects mid-build edits.
        let pending = ctx.take_pending_callgraph_store_paths();
        if !pending.is_empty() {
            if let Err(error) = store.refresh_files(&pending) {
                aft::slog_warn!(
                    "callgraph store post-build pending refresh failed: {}",
                    error
                );
                if let Err(mark_error) = store.mark_files_stale(&pending) {
                    aft::slog_warn!(
                        "failed to mark callgraph store files stale after post-build refresh failure: {}",
                        mark_error
                    );
                }
            }
        }
        *ctx.callgraph_store().borrow_mut() = Some(store);
        installed = true;
        status_changed = true;
    }

    if disconnected || installed {
        *ctx.callgraph_store_rx().borrow_mut() = None;
        if disconnected && !installed {
            // Build failed: discard pending paths (no store to apply them to);
            // a later op restarts the build and re-walks the project.
            let _ = ctx.take_pending_callgraph_store_paths();
        }
        status_changed = true;
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn drain_semantic_index_events(ctx: &AppContext) {
    let (events, disconnected) = {
        let rx_ref = ctx.semantic_index_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (events, disconnected)
    };

    if events.is_empty() && !disconnected {
        return;
    }

    let mut keep_receiver = true;
    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    let mut replay_corpus_refresh = false;
    for event in events {
        match event {
            SemanticIndexEvent::Progress {
                stage,
                files,
                entries_done,
                entries_total,
            } => {
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                    stage,
                    files,
                    entries_done,
                    entries_total,
                };
                // Push progress to the sidebar. Without this, a long rebuild
                // (e.g. a slow local embedding backend re-indexing after a prior
                // failure) leaves the sidebar showing the stale prior state —
                // "failed" with an old error — for the entire build, even though
                // it is actively embedding. Progress transitions are exactly
                // when the user needs to see "building".
                status_changed = true;
            }
            SemanticIndexEvent::Ready(mut index) => {
                mark_semantic_corpus_refresh_success();
                let pending_paths = ctx.take_pending_semantic_index_paths();
                for path in pending_paths {
                    if watcher_path_is_semantic_source(&path) {
                        index.invalidate_file(&path);
                        replay_refresh_paths.push(path);
                    }
                }
                replay_corpus_refresh = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index().borrow_mut() = Some(index);
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
                keep_receiver = false;
                status_changed = true;
            }
            SemanticIndexEvent::Failed(error) => {
                let _ = ctx.take_pending_semantic_index_paths();
                let _ = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index().borrow_mut() = None;
                ctx.clear_semantic_refresh_worker();
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(error);
                keep_receiver = false;
                status_changed = true;
            }
        }
    }

    if disconnected && keep_receiver {
        let _ = ctx.take_pending_semantic_index_paths();
        let _ = ctx.take_pending_semantic_corpus_refresh();
        *ctx.semantic_index().borrow_mut() = None;
        ctx.clear_semantic_refresh_worker();
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(
            "semantic index build worker disconnected before reporting completion".to_string(),
        );
        keep_receiver = false;
        status_changed = true;
    }

    if !keep_receiver {
        *ctx.semantic_index_rx().borrow_mut() = None;
    }

    if replay_corpus_refresh {
        if ctx.canonical_cache_root_opt().is_some() {
            *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                stage: "refreshing_corpus".to_string(),
                files: None,
                entries_done: None,
                entries_total: None,
            };
            let sent = ctx
                .semantic_refresh_sender()
                .is_some_and(|sender| sender.send(SemanticRefreshRequest::Corpus).is_ok());
            if !sent {
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(
                    "semantic corpus refresh worker unavailable".to_string(),
                );
            }
            status_changed = true;
        }
    } else if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx.semantic_index_status().borrow_mut();
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn drain_semantic_refresh_events(ctx: &AppContext) {
    let (events, disconnected) = {
        let rx_ref = ctx.semantic_refresh_event_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (events, disconnected)
    };

    if events.is_empty() && !disconnected {
        maybe_fire_semantic_refresh_probe(ctx);
        return;
    }

    let had_events = !events.is_empty();
    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    for event in events {
        match event {
            SemanticRefreshEvent::Started { paths } => {
                let mut status = ctx.semantic_index_status().borrow_mut();
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in paths {
                        status.start_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusStarted { files } => {
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                    stage: "refreshing_corpus".to_string(),
                    files: Some(files),
                    entries_done: None,
                    entries_total: None,
                };
                status_changed = true;
            }
            SemanticRefreshEvent::Completed {
                added_entries,
                updated_metadata,
                completed_paths,
            } => {
                if let Some(index) = ctx.semantic_index().borrow_mut().as_mut() {
                    index.apply_refresh_update(added_entries, updated_metadata, &completed_paths);
                }
                mark_semantic_refresh_success(ctx, &completed_paths);
                let mut status = ctx.semantic_index_status().borrow_mut();
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in &completed_paths {
                        status.complete_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusCompleted {
                mut index,
                changed,
                added,
                deleted,
                total_processed,
            } => {
                mark_semantic_corpus_refresh_success();
                if changed > 0 || added > 0 || deleted > 0 {
                    aft::slog_info!(
                        "semantic corpus refresh completed: {} changed, {} new, {} deleted, {} total processed",
                        changed,
                        added,
                        deleted,
                        total_processed
                    );
                }
                let pending_paths = ctx.take_pending_semantic_index_paths();
                for path in pending_paths {
                    if !watcher_path_is_semantic_source(&path) {
                        continue;
                    }
                    index.invalidate_file(&path);
                    if !watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                        replay_refresh_paths.push(path);
                    }
                }
                *ctx.semantic_index().borrow_mut() = Some(index);
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
                status_changed = true;
            }
            SemanticRefreshEvent::Failed { paths, error } => {
                if aft::semantic_index::embedding_failure_is_transient(&error) {
                    if record_semantic_refresh_transient_failure() {
                        ctx.add_pending_semantic_index_paths(paths);
                        ensure_semantic_refresh_probe_scheduled();
                    } else if !schedule_semantic_refresh_retry(ctx, paths.clone(), &error) {
                        aft::slog_warn!(
                            "semantic refresh worker unavailable; preserving {} transiently failed file(s) for retry",
                            paths.len(),
                        );
                        ctx.add_pending_semantic_index_paths(paths);
                    }
                } else {
                    aft::slog_warn!("semantic refresh failed: {}", error);
                    reset_semantic_refresh_transient_failure_count();
                    clear_semantic_refresh_retry_attempts(&paths);
                    let mut status = ctx.semantic_index_status().borrow_mut();
                    if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                        for path in &paths {
                            status.complete_refreshing_file(path);
                        }
                        status_changed = true;
                    }
                }
            }
            SemanticRefreshEvent::CorpusFailed { error } => {
                // A transient backend blip during a corpus refresh must NOT
                // destroy the working index — the prior index is still valid and
                // serving. Keep it Ready and let the next watcher/ignore change
                // re-trigger the refresh, rather than nuking everything to
                // `Failed` over a connection hiccup (the same park-forever trap
                // the initial build now rides out). Permanent errors (dimension
                // mismatch, too-many-files) still drop the index and surface the
                // real failure.
                if aft::semantic_index::embedding_failure_is_transient(&error) {
                    let clean = aft::semantic_index::strip_transient_embedding_marker(&error);
                    let has_index = ctx.semantic_index().borrow().is_some();
                    if has_index {
                        aft::slog_warn!(
                            "semantic corpus refresh hit a transient backend error ({}); keeping the existing index",
                            clean,
                        );
                        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
                    } else {
                        // No index to fall back on — surface the clean message.
                        aft::slog_warn!("semantic corpus refresh failed: {}", clean);
                        *ctx.semantic_index_status().borrow_mut() =
                            SemanticIndexStatus::Failed(clean);
                    }
                    status_changed = true;
                } else {
                    aft::slog_warn!("semantic corpus refresh failed: {}", error);
                    let _ = ctx.take_pending_semantic_index_paths();
                    *ctx.semantic_index().borrow_mut() = None;
                    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(error);
                    status_changed = true;
                }
            }
        }
    }

    if disconnected {
        ctx.clear_semantic_refresh_worker();
        let refreshing_paths = {
            let status = ctx.semantic_index_status().borrow();
            match &*status {
                SemanticIndexStatus::Ready { refreshing } => refreshing.clone(),
                _ => Vec::new(),
            }
        };
        if !refreshing_paths.is_empty() {
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &refreshing_paths {
                status.cancel_refreshing_file(path);
            }
        }
        if !refreshing_paths.is_empty() || had_events {
            status_changed = true;
        }
    }

    if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx.semantic_index_status().borrow_mut();
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed corpus file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    maybe_fire_semantic_refresh_probe(ctx);

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn drain_lsp_events(ctx: &AppContext) {
    let drained = {
        let mut lsp = ctx.lsp();
        lsp.drain_events()
    };
    let mut status_changed = drained.diagnostics_changed;
    for event in drained.events {
        match event {
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] notification {:?} {} {} {}",
                    server_kind,
                    root.display(),
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerRequest {
                server_kind,
                root,
                id,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] request {:?} {} {:?} {} {}",
                    server_kind,
                    root.display(),
                    id,
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerExited { server_kind, root } => {
                aft::slog_info!("exited {:?} {}", server_kind, root.display());
                status_changed = true;
            }
        }
    }
    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

#[cfg(test)]
mod watcher_filter_tests {
    use super::{
        attach_status_bar, dispatch_panic_response, drain_configure_warning_events,
        drain_semantic_index_events, drain_semantic_refresh_events, drain_watcher_events,
        reset_semantic_refresh_retry_state_for_test, reset_status_bar_emission_for_test,
        schedule_semantic_refresh_retry, semantic_refresh_circuit_is_open,
        semantic_refresh_probe_is_scheduled_for_test,
        semantic_refresh_transient_failure_count_for_test, watcher_path_is_callgraph_indexed,
        write_push_frame_or_request_shutdown, BREAKER_TRIP_THRESHOLD, MAX_RETRY_ATTEMPTS,
    };
    use aft::config::Config;
    use aft::context::{
        AppContext, SemanticIndexEvent, SemanticIndexStatus, SemanticRefreshEvent,
        SemanticRefreshRequest, SemanticRefreshWorkerSlot,
    };
    use aft::harness::Harness;
    use aft::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
    use aft::lsp::registry::ServerKind;
    use aft::lsp::roots::ServerKey;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::{ConfigureWarningsFrame, PushFrame, Response};
    use aft::semantic_index::SemanticIndex;
    use aft::watcher_filter::{
        watcher_event_invalidates, FilteredWatcherPaths, WatcherDispatchEvent, WatcherFilterConfig,
    };
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify::EventKind;
    use tempfile::TempDir;

    /// Wait budget for an async dispatch (semantic-refresh request / status
    /// frame) the worker produces on a freshly-spawned thread. The dispatch
    /// thread does `spawn -> sleep(backoff) -> send`, so under CI load the
    /// spawn-plus-wakeup latency can briefly exceed a tight budget. The wait
    /// returns the instant the value arrives, so this is zero-cost on the happy
    /// path. This is only headroom for thread scheduling; the actual cross-test
    /// flake (the request never arriving at all) was a process-global
    /// breaker-state race, fixed by serializing breaker tests via
    /// `semantic_breaker_test_lock`. Negative waits (asserting absence) must
    /// stay short and are NOT this const.
    const RECV_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    fn make_ctx_with_root(root: &std::path::Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    fn install_watcher_rx(ctx: &AppContext) -> crossbeam_channel::Sender<WatcherDispatchEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().borrow_mut() = Some(rx);
        tx
    }

    fn watcher_paths_event(path: std::path::PathBuf) -> WatcherDispatchEvent {
        WatcherDispatchEvent::Paths(vec![path])
    }

    fn filter_watcher_raw_paths<I>(ctx: &AppContext, raw_paths: I) -> FilteredWatcherPaths
    where
        I: IntoIterator<Item = std::path::PathBuf>,
    {
        let root = ctx
            .canonical_cache_root_opt()
            .or_else(|| ctx.config().project_root.clone())
            .expect("test context has project root");
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let config = WatcherFilterConfig::new(root, ctx.git_common_dir());
        aft::watcher_filter::filter_watcher_raw_paths_for_test(
            &config,
            &ctx.shared_gitignore(),
            raw_paths,
        )
    }

    // The callgraph store indexes every detected language (not just the trigram
    // SOURCE_EXTENSIONS set), so its watcher refresh must too — otherwise edits
    // to Java/C/C++/C#/Kotlin/Ruby/… serve stale call results until full rebuild.
    #[test]
    fn callgraph_watcher_gate_covers_all_indexed_languages() {
        use std::path::Path;
        for ok in [
            "Foo.java", "x.cpp", "y.c", "Svc.cs", "m.kt", "a.rb", "z.php", "s.scala", "C.sol",
            "app.ts", "main.rs", "h.go", "p.py",
        ] {
            assert!(
                watcher_path_is_callgraph_indexed(Path::new(ok)),
                "{ok} should be callgraph-indexed"
            );
        }
        // Genuinely-undetected extensions (detect_language → None). Note md/json/
        // yaml ARE detected (the store walks them at cold-build), so matching
        // cold-build means they refresh too — refresh coverage == index coverage.
        for skip in [
            "notes.txt",
            "image.png",
            "Cargo.lock",
            "data.csv",
            "config.toml",
        ] {
            assert!(
                !watcher_path_is_callgraph_indexed(Path::new(skip)),
                "{skip} should not be callgraph-indexed"
            );
        }
    }

    fn install_semantic_refresh_channels(
        ctx: &AppContext,
    ) -> (
        crossbeam_channel::Receiver<SemanticRefreshRequest>,
        crossbeam_channel::Sender<SemanticRefreshEvent>,
    ) {
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker_slot: SemanticRefreshWorkerSlot =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        ctx.install_semantic_refresh_worker(request_tx, event_rx, worker_slot);
        (request_rx, event_tx)
    }

    fn transient_embedding_error() -> String {
        format!(
            "{}backend unavailable",
            aft::semantic_index::TRANSIENT_EMBEDDING_MARKER
        )
    }

    fn recv_files_request(
        request_rx: &crossbeam_channel::Receiver<SemanticRefreshRequest>,
    ) -> Vec<std::path::PathBuf> {
        match request_rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("semantic refresh request")
        {
            SemanticRefreshRequest::Files { paths } => paths,
            SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
        }
    }

    fn status_frame_rx(ctx: &AppContext) -> std::sync::mpsc::Receiver<PushFrame> {
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        rx
    }

    fn recv_status_changed(rx: &std::sync::mpsc::Receiver<PushFrame>) -> serde_json::Value {
        match rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("status_changed frame")
        {
            PushFrame::StatusChanged(frame) => frame.snapshot,
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    fn err_diag(file: &std::path::Path) -> StoredDiagnostic {
        StoredDiagnostic {
            file: file.to_path_buf(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "boom".into(),
            code: None,
            source: None,
        }
    }

    /// Shared serialization lock for every test that touches the process-global
    /// semantic-refresh breaker state (`SEMANTIC_REFRESH_CIRCUIT_OPEN` /
    /// `PROBE_READY` / `PROBE_IN_FLIGHT` / `CONSECUTIVE_TRANSIENT_FAILURES`).
    ///
    /// The breaker is intentionally one-per-process in production (a single
    /// embedding backend per process), so the globals are correct there. But
    /// the test harness runs these tests in parallel against that shared state,
    /// and they corrupt each other: e.g. a concurrent breaker test that leaves
    /// `PROBE_READY + CIRCUIT_OPEN` true makes another test's
    /// `drain_semantic_refresh_events -> maybe_fire_semantic_refresh_probe`
    /// spuriously fire and consume that test's per-ctx pending paths. EVERY
    /// breaker-touching test MUST hold this lock (via `with_semantic_retry_backoff_ms`
    /// or `with_semantic_breaker_isolation`) so only one runs at a time.
    fn semantic_breaker_test_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Reset the breaker globals and run `f` serialized against every other
    /// breaker-touching test. For tests that touch the breaker but do not need
    /// the retry-backoff override.
    fn with_semantic_breaker_isolation<R>(f: impl FnOnce() -> R) -> R {
        let _guard = semantic_breaker_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_semantic_refresh_retry_state_for_test();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        reset_semantic_refresh_retry_state_for_test();
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn with_semantic_retry_backoff_ms<R>(ms: u64, f: impl FnOnce() -> R) -> R {
        let _guard = semantic_breaker_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_semantic_refresh_retry_state_for_test();
        let previous = std::env::var_os("AFT_SEMANTIC_RETRY_BACKOFF_MS");
        unsafe {
            std::env::set_var("AFT_SEMANTIC_RETRY_BACKOFF_MS", ms.to_string());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("AFT_SEMANTIC_RETRY_BACKOFF_MS", value),
                None => std::env::remove_var("AFT_SEMANTIC_RETRY_BACKOFF_MS"),
            }
        }
        reset_semantic_refresh_retry_state_for_test();
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Run `f` with global git-ignore discovery neutralized.
    ///
    /// `rebuild_gitignore` loads git's global excludes (the `ignore` crate
    /// resolves `$XDG_CONFIG_HOME/git/ignore`, falling back to
    /// `$HOME/.config/git/ignore`). A developer machine commonly has that file,
    /// so any "no project ignore → None" baseline is only deterministic when
    /// global discovery is pointed at an empty directory. Pointing
    /// `XDG_CONFIG_HOME` at a fresh tempdir does that without touching `HOME`.
    /// Serialized by a process-local mutex; env is restored before use.
    fn with_neutralized_global_gitignore<R>(f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized by LOCK above; restored immediately after `f`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn watcher_drain_refreshes_open_callgraph_store() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let source = root.join("main.ts");
        std::fs::write(
            &source,
            "export function entry() { oldLeaf(); }\nfunction oldLeaf() {}\nfunction newLeaf() {}\n",
        )
        .unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(root.join("storage")),
                callgraph_store: true,
                ..Config::default()
            },
        );
        ctx.set_harness(Harness::Opencode);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx.set_cache_role(false, None);
        ctx.rebuild_gitignore();
        let tx = install_watcher_rx(&ctx);
        {
            let store = ctx
                .ensure_callgraph_store()
                .unwrap()
                .expect("store should build on demand");
            let tree = store
                .call_tree(std::path::Path::new("main.ts"), "entry", 1)
                .unwrap();
            assert_eq!(tree.children[0].name, "oldLeaf");
        }

        std::fs::write(
            &source,
            "export function entry() { newLeaf(); }\nfunction oldLeaf() {}\nfunction newLeaf() {}\n",
        )
        .unwrap();
        tx.send(watcher_paths_event(source)).unwrap();
        drain_watcher_events(&ctx);

        let store_ref = ctx.callgraph_store().borrow();
        let store = store_ref.as_ref().expect("store remains open");
        let tree = store
            .call_tree(std::path::Path::new("main.ts"), "entry", 1)
            .unwrap();
        assert_eq!(tree.children[0].name, "newLeaf");
    }

    #[test]
    fn create_and_remove_invalidate() {
        assert!(watcher_event_invalidates(&EventKind::Create(
            CreateKind::File
        )));
        assert!(watcher_event_invalidates(&EventKind::Remove(
            RemoveKind::File
        )));
    }

    #[test]
    fn modify_data_invalidates() {
        // The "actual file write" case — must invalidate.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Data(DataChange::Content)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Data(DataChange::Any)
        )));
    }

    #[test]
    fn modify_name_rename_invalidates() {
        // Renames should invalidate the old path's cached state.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Name(RenameMode::To)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Name(RenameMode::From)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Name(RenameMode::Both)
        )));
    }

    #[test]
    fn modify_metadata_writetime_invalidates() {
        // mtime change implies real content edit on every supported platform.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
    }

    #[test]
    fn modify_metadata_any_or_other_invalidates() {
        // Catch-all "we can't tell what changed" — better to over-invalidate.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Any)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Other)
        )));
    }

    /// Regression: biome-lint reading every TS file under Linux atime triggers
    /// notify `Modify(Metadata(AccessTime))` events. Treating those as
    /// invalidations re-parses the entire symbol cache, search index, and
    /// semantic index for every read-only lint pass — wasted work.
    #[test]
    fn modify_metadata_access_time_does_not_invalidate() {
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::AccessTime)
        )));
    }

    #[test]
    fn modify_metadata_permissions_ownership_extended_do_not_invalidate() {
        // chmod / chown / xattrs don't change content.
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Permissions)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Ownership)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Extended)
        )));
    }

    #[test]
    fn access_events_do_not_invalidate() {
        // Read syscalls cause an atime feedback loop on Linux when the watcher
        // is watching a directory we read into.
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Open(AccessMode::Read)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Read
        )));
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Close(AccessMode::Read)
        )));
    }

    #[test]
    fn other_event_kinds_do_not_invalidate() {
        // `Other`, `Any` — we explicitly opt out of unknown event categories
        // since the existing `Modify(_)` and `Modify(Metadata(Any))` arms
        // already handle the meaningful catch-all cases.
        assert!(!watcher_event_invalidates(&EventKind::Other));
        assert!(!watcher_event_invalidates(&EventKind::Any));
    }

    #[test]
    fn dispatch_panic_response_is_clear_internal_error() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");

        let response = dispatch_panic_response("panic-id", "db_get_state", payload.as_ref());

        assert!(!response.success);
        assert_eq!(response.data["code"], "internal_error");
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("command 'db_get_state' panicked: boom"));
    }

    #[test]
    fn push_frame_write_error_requests_shutdown() {
        struct BrokenWriter;

        impl std::io::Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stdout closed",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let shutdown = std::sync::atomic::AtomicBool::new(false);
        let frame = PushFrame::ConfigureWarnings(ConfigureWarningsFrame::new(
            "/repo",
            0,
            false,
            5_000,
            Vec::new(),
        ));

        write_push_frame_or_request_shutdown(&mut BrokenWriter, &frame, &shutdown);

        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn status_bar_attach_skips_unchanged_fingerprint() {
        reset_status_bar_emission_for_test();
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);

        let mut first = Response::success("one", serde_json::json!({}));
        attach_status_bar(&mut first, &ctx, "read");
        assert_eq!(first.data["status_bar"]["dead_code"], 1);
        assert_eq!(first.data["status_bar"]["unused_exports"], 2);
        assert_eq!(first.data["status_bar"]["duplicates"], 3);
        assert_eq!(first.data["status_bar"]["todos"], 4);

        let mut unchanged = Response::success("two", serde_json::json!({}));
        attach_status_bar(&mut unchanged, &ctx, "read");
        assert!(unchanged.data.get("status_bar").is_none());

        ctx.update_status_bar_tier2(Some(5), Some(2), Some(3), Some(4), false);
        let mut changed = Response::success("three", serde_json::json!({}));
        attach_status_bar(&mut changed, &ctx, "read");
        assert_eq!(changed.data["status_bar"]["dead_code"], 5);
    }

    #[test]
    fn configure_warning_drain_drops_stale_generation() {
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        let (frame_tx, frame_rx) = std::sync::mpsc::channel();
        ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
            let _ = frame_tx.send(frame);
        }))));

        let warnings_tx = ctx.configure_warnings_sender();
        let current_generation = ctx.advance_configure_generation();
        warnings_tx
            .send((
                current_generation - 1,
                ConfigureWarningsFrame::new("/stale", 1, false, 5_000, Vec::new()),
            ))
            .unwrap();
        warnings_tx
            .send((
                current_generation,
                ConfigureWarningsFrame::new("/current", 2, false, 5_000, Vec::new()),
            ))
            .unwrap();

        drain_configure_warning_events(&ctx);

        let frame = frame_rx.try_recv().expect("current warning frame");
        match frame {
            PushFrame::ConfigureWarnings(frame) => {
                assert_eq!(frame.project_root, "/current");
                assert_eq!(frame.source_file_count, 2);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(frame_rx.try_recv().is_err());
    }

    #[test]
    fn gitignore_write_rebuilds_before_filtering_same_batch_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let gitignore = root.join(".gitignore");
        let ignored = root.join("foo.txt");
        let kept = root.join("bar.txt");
        std::fs::write(&ignored, "ignored").unwrap();
        std::fs::write(&kept, "kept").unwrap();

        let ctx = make_ctx_with_root(root);
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        assert!(ctx.gitignore().is_none());

        std::fs::write(&gitignore, "foo.txt\n").unwrap();
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        let changed =
            filter_watcher_raw_paths(&ctx, vec![gitignore.clone(), ignored.clone(), kept.clone()]);

        let gitignore = std::fs::canonicalize(gitignore).unwrap();
        let ignored = std::fs::canonicalize(ignored).unwrap();
        let kept = std::fs::canonicalize(kept).unwrap();
        assert!(changed.ignore_file_changed);
        assert!(changed.changed.contains(&gitignore));
        assert!(!changed.changed.contains(&ignored));
        assert!(changed.changed.contains(&kept));
    }

    #[test]
    fn infra_ignore_file_does_not_request_corpus_refresh_but_project_aftignore_does() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let infra_gitignore = root.join("node_modules").join("pkg").join(".gitignore");
        std::fs::create_dir_all(infra_gitignore.parent().unwrap()).unwrap();
        std::fs::write(&infra_gitignore, "dist/\n").unwrap();

        let ctx = make_ctx_with_root(root);
        let changed = filter_watcher_raw_paths(&ctx, vec![infra_gitignore]);

        assert!(!changed.ignore_file_changed);
        assert!(changed.changed.is_empty());

        let aftignore = root.join(".aftignore");
        std::fs::write(&aftignore, "ignored/\n").unwrap();
        let changed = filter_watcher_raw_paths(&ctx, vec![aftignore.clone()]);

        let aftignore = std::fs::canonicalize(aftignore).unwrap();
        assert!(changed.ignore_file_changed);
        assert!(changed.changed.contains(&aftignore));
    }

    #[test]
    fn project_git_info_exclude_requests_corpus_refresh_without_indexing_git_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let git_info = root.join(".git").join("info");
        std::fs::create_dir_all(&git_info).unwrap();
        let exclude = git_info.join("exclude");
        std::fs::write(&exclude, "ignored/\n").unwrap();

        let ctx = make_ctx_with_root(root);
        let changed = filter_watcher_raw_paths(&ctx, vec![exclude]);

        assert!(changed.ignore_file_changed);
        assert!(changed.changed.is_empty());
    }

    #[test]
    fn shared_git_info_exclude_requests_corpus_refresh_without_indexing_external_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let common = TempDir::new().unwrap();
        let git_info = common.path().join("info");
        std::fs::create_dir_all(&git_info).unwrap();
        let exclude = git_info.join("exclude");
        std::fs::write(&exclude, "ignored/\n").unwrap();

        let ctx = make_ctx_with_root(root);
        ctx.set_cache_role(false, Some(common.path().to_path_buf()));
        let changed = filter_watcher_raw_paths(&ctx, vec![exclude]);

        assert!(changed.ignore_file_changed);
        assert!(changed.changed.is_empty());
    }

    #[test]
    fn semantic_index_disconnect_without_terminal_event_marks_failed() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = make_ctx_with_root(&root);
        let (tx, rx) = crossbeam_channel::unbounded::<SemanticIndexEvent>();
        *ctx.semantic_index_rx().borrow_mut() = Some(rx);
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
            stage: "embedding".into(),
            files: Some(1),
            entries_done: Some(0),
            entries_total: Some(1),
        };
        drop(tx);

        drain_semantic_index_events(&ctx);

        assert!(ctx.semantic_index_rx().borrow().is_none());
        assert!(matches!(
            &*ctx.semantic_index_status().borrow(),
            SemanticIndexStatus::Failed(message)
                if message.contains("disconnected before reporting completion")
        ));
    }

    #[test]
    fn semantic_refresh_disconnect_after_started_cancels_refreshing_paths() {
        // Drains refresh events (-> maybe_fire probe touches global breaker), so
        // serialize against the other breaker tests.
        with_semantic_breaker_isolation(|| {
            let tmp = TempDir::new().unwrap();
            let root = std::fs::canonicalize(tmp.path()).unwrap();
            let file = root.join("lib.rs");
            std::fs::write(&file, "fn main() {}\n").unwrap();

            let ctx = make_ctx_with_root(&root);
            *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
            let (_request_rx, event_tx) = install_semantic_refresh_channels(&ctx);
            event_tx
                .send(SemanticRefreshEvent::Started {
                    paths: vec![file.clone()],
                })
                .unwrap();
            drop(event_tx);

            drain_semantic_refresh_events(&ctx);

            assert!(ctx.semantic_refresh_event_rx().borrow().is_none());
            assert_eq!(ctx.semantic_index_status().borrow().refreshing_count(), 0);
        });
    }

    #[test]
    fn watcher_error_clears_receiver_and_marks_degraded() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = make_ctx_with_root(&root);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx
            .send(WatcherDispatchEvent::Error(
                "watcher init failed".to_string(),
            ))
            .unwrap();

        drain_watcher_events(&ctx);

        assert!(ctx.watcher_rx().borrow().is_none());
        assert!(ctx
            .degraded_reasons()
            .contains(&"watcher_unavailable".to_string()));
    }

    #[test]
    fn project_root_deleted_control_drops_watcher_rx_and_marks_degraded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let ctx = make_ctx_with_root(&root);
        ctx.set_canonical_cache_root(root);

        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(WatcherDispatchEvent::RootDeleted).unwrap();

        drain_watcher_events(&ctx);

        assert!(ctx.watcher_rx().borrow().is_none());
        assert!(ctx
            .degraded_reasons()
            .contains(&"project_root_deleted".to_string()));
    }

    #[test]
    fn watcher_semantic_refresh_includes_vue_extension() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("App.vue");
        std::fs::write(&file, "<script setup>const n = 1;</script>").unwrap();

        let ctx = make_ctx_with_root(&root);
        *ctx.semantic_index().borrow_mut() = Some(SemanticIndex::new(root.clone(), 3));
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
        let (request_rx, _event_tx) = install_semantic_refresh_channels(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file.clone())).unwrap();

        drain_watcher_events(&ctx);

        match request_rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("semantic refresh request")
        {
            SemanticRefreshRequest::Files { paths } => assert_eq!(paths, vec![file]),
            SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
        }
    }

    #[test]
    fn transient_file_refresh_failure_requeues_retry_without_completing() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);
            let mut status = SemanticIndexStatus::ready();
            status.add_refreshing_file(file.clone());
            status.start_refreshing_file(file.clone());
            *ctx.semantic_index_status().borrow_mut() = status;

            event_tx
                .send(SemanticRefreshEvent::Failed {
                    paths: vec![file.clone()],
                    error: format!(
                        "{}backend unavailable",
                        aft::semantic_index::TRANSIENT_EMBEDDING_MARKER
                    ),
                })
                .unwrap();

            drain_semantic_refresh_events(&ctx);

            match request_rx
                .recv_timeout(RECV_DISPATCH_TIMEOUT)
                .expect("retry request")
            {
                SemanticRefreshRequest::Files { paths } => assert_eq!(paths, vec![file.clone()]),
                SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
            }
            assert_eq!(ctx.semantic_index_status().borrow().refreshing_count(), 1);
        });
    }

    #[test]
    fn semantic_refresh_breaker_coalesces_open_retries_into_single_probe() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let files = (0..(BREAKER_TRIP_THRESHOLD + 2))
            .map(|index| root.join(format!("file{index}.rs")))
            .collect::<Vec<_>>();
        for file in &files {
            std::fs::write(file, "fn main() {}\n").unwrap();
        }

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);

            for file in files.iter().take(BREAKER_TRIP_THRESHOLD) {
                event_tx
                    .send(SemanticRefreshEvent::Failed {
                        paths: vec![file.clone()],
                        error: transient_embedding_error(),
                    })
                    .unwrap();
            }
            drain_semantic_refresh_events(&ctx);

            assert!(semantic_refresh_circuit_is_open());
            assert!(semantic_refresh_probe_is_scheduled_for_test());

            for file in files.iter().skip(BREAKER_TRIP_THRESHOLD) {
                event_tx
                    .send(SemanticRefreshEvent::Failed {
                        paths: vec![file.clone()],
                        error: transient_embedding_error(),
                    })
                    .unwrap();
            }
            drain_semantic_refresh_events(&ctx);

            assert!(semantic_refresh_probe_is_scheduled_for_test());
            let expected_probe_paths = files[(BREAKER_TRIP_THRESHOLD - 1)..].to_vec();
            let pending = ctx.take_pending_semantic_index_paths();
            assert_eq!(pending, expected_probe_paths);
            ctx.add_pending_semantic_index_paths(pending);

            std::thread::sleep(std::time::Duration::from_millis(20));
            drain_semantic_refresh_events(&ctx);

            let mut batches = Vec::new();
            for _ in 0..BREAKER_TRIP_THRESHOLD {
                batches.push(recv_files_request(&request_rx));
            }
            for file in files.iter().take(BREAKER_TRIP_THRESHOLD - 1) {
                assert!(batches.contains(&vec![file.clone()]));
            }
            assert!(batches.contains(&expected_probe_paths));
            assert!(request_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err());
        });
    }

    #[test]
    fn semantic_refresh_success_resets_breaker_and_next_failure_starts_fresh() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let files = (0..(BREAKER_TRIP_THRESHOLD + 1))
            .map(|index| root.join(format!("reset{index}.rs")))
            .collect::<Vec<_>>();
        for file in &files {
            std::fs::write(file, "fn main() {}\n").unwrap();
        }

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);

            for file in files.iter().take(BREAKER_TRIP_THRESHOLD) {
                event_tx
                    .send(SemanticRefreshEvent::Failed {
                        paths: vec![file.clone()],
                        error: transient_embedding_error(),
                    })
                    .unwrap();
            }
            drain_semantic_refresh_events(&ctx);
            assert!(semantic_refresh_circuit_is_open());

            // The pre-breaker failures each schedule a retry on an independent
            // thread with the same backoff, so their arrival order at the channel
            // is nondeterministic. Collect them and assert the set, not the order
            // (matches the sibling breaker tests).
            let mut retry_batches = Vec::new();
            for _ in 0..(BREAKER_TRIP_THRESHOLD - 1) {
                retry_batches.push(recv_files_request(&request_rx));
            }
            for file in files.iter().take(BREAKER_TRIP_THRESHOLD - 1) {
                assert!(
                    retry_batches.contains(&vec![file.clone()]),
                    "missing retry for {file:?}; got {retry_batches:?}"
                );
            }

            event_tx
                .send(SemanticRefreshEvent::Completed {
                    added_entries: Vec::new(),
                    updated_metadata: Vec::new(),
                    completed_paths: vec![files[BREAKER_TRIP_THRESHOLD - 1].clone()],
                })
                .unwrap();
            drain_semantic_refresh_events(&ctx);

            assert!(!semantic_refresh_circuit_is_open());
            assert_eq!(semantic_refresh_transient_failure_count_for_test(), 0);

            let fresh_file = files[BREAKER_TRIP_THRESHOLD].clone();
            event_tx
                .send(SemanticRefreshEvent::Failed {
                    paths: vec![fresh_file.clone()],
                    error: transient_embedding_error(),
                })
                .unwrap();
            drain_semantic_refresh_events(&ctx);

            assert!(!semantic_refresh_circuit_is_open());
            assert_eq!(semantic_refresh_transient_failure_count_for_test(), 1);
            assert_eq!(recv_files_request(&request_rx), vec![fresh_file]);
        });
    }

    #[test]
    fn semantic_refresh_retry_attempt_cap_stashes_path_without_hot_timer() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("cap.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, _event_tx) = install_semantic_refresh_channels(&ctx);
            let error = transient_embedding_error();

            for _ in 0..MAX_RETRY_ATTEMPTS {
                assert!(schedule_semantic_refresh_retry(
                    &ctx,
                    vec![file.clone()],
                    &error
                ));
                assert_eq!(recv_files_request(&request_rx), vec![file.clone()]);
            }

            assert!(schedule_semantic_refresh_retry(
                &ctx,
                vec![file.clone()],
                &error
            ));
            assert!(request_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err());
            assert_eq!(ctx.take_pending_semantic_index_paths(), vec![file]);
        });
    }

    #[test]
    fn transient_corpus_failure_preserves_pending_semantic_paths() {
        // Touches the process-global breaker via drain -> maybe_fire probe, so
        // it must serialize against the other breaker tests (otherwise a
        // concurrent test's leaked PROBE_READY+CIRCUIT_OPEN makes our drain
        // spuriously fire the probe and consume our pending paths).
        with_semantic_breaker_isolation(|| {
            let tmp = TempDir::new().unwrap();
            let root = std::fs::canonicalize(tmp.path()).unwrap();
            let file = root.join("pending.vue");
            std::fs::write(&file, "<template />").unwrap();

            let ctx = make_ctx_with_root(&root);
            *ctx.semantic_index().borrow_mut() = Some(SemanticIndex::new(root.clone(), 3));
            *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                stage: "refreshing_corpus".into(),
                files: Some(1),
                entries_done: None,
                entries_total: None,
            };
            ctx.add_pending_semantic_index_paths(vec![file.clone()]);
            let (_request_rx, event_tx) = install_semantic_refresh_channels(&ctx);

            event_tx
                .send(SemanticRefreshEvent::CorpusFailed {
                    error: format!(
                        "{}backend unavailable",
                        aft::semantic_index::TRANSIENT_EMBEDDING_MARKER
                    ),
                })
                .unwrap();

            drain_semantic_refresh_events(&ctx);

            let pending = ctx.take_pending_semantic_index_paths();
            assert_eq!(pending, vec![file]);
            assert!(matches!(
                &*ctx.semantic_index_status().borrow(),
                SemanticIndexStatus::Ready { .. }
            ));
        });
    }

    #[test]
    fn watcher_stale_mark_emits_status_without_semantic_change() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("notes.txt");
        std::fs::write(&file, "changed").unwrap();

        let ctx = make_ctx_with_root(&root);
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);
        let rx = status_frame_rx(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file)).unwrap();

        drain_watcher_events(&ctx);

        let snapshot = recv_status_changed(&rx);
        assert_eq!(
            snapshot["status_bar"]["tier2_stale"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn watcher_diagnostics_clear_emits_status_without_semantic_change() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("gone.txt");
        std::fs::write(&file, "deleted").unwrap();

        let ctx = make_ctx_with_root(&root);
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), true);
        {
            let key = ServerKey {
                kind: ServerKind::TypeScript,
                root: root.clone(),
            };
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish(key, file.clone(), vec![err_diag(&file)]);
        }
        assert_eq!(ctx.status_bar_counts().unwrap().errors, 1);

        std::fs::remove_file(&file).unwrap();
        let rx = status_frame_rx(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file)).unwrap();

        drain_watcher_events(&ctx);

        let snapshot = recv_status_changed(&rx);
        assert_eq!(snapshot["status_bar"]["errors"], serde_json::Value::from(0));
        assert_eq!(ctx.status_bar_counts().unwrap().errors, 0);
    }
}
