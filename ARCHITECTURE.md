# Architecture

## Pattern Overview

**Overall:** TypeScript plugin + Rust worker process communicating over either a session-scoped NDJSON bridge (standalone mode) or the Subconscious (subc) daemon transport. A unified CLI (`packages/aft-cli/`) serves setup/doctor across all harnesses; shared transport, binary resolution, and ONNX helpers live in `packages/aft-bridge/`.

**Key Characteristics:**
- Use the master configuration switch `enabled` (configured globally or per-project in `aft.jsonc`) to short-circuit plugin loading and disable AFT execution.
- Use `packages/opencode-plugin/src/index.ts` and `packages/pi-plugin/src/index.ts` to register harness tools and map them onto the unified `tool_call` command when enabled.
- Use `packages/aft-bridge/src/transport-factory.ts` to instantiate either `BridgePool` (standalone NDJSON bridge, isolating one `aft` process per project root) or `SubcTransportPool` (daemon-backed transport) satisfying the shared `AftTransportPool` interface.
- Use `packages/aft-cli/src/index.ts` as the unified setup/doctor CLI across all harnesses.
- Use `crates/aft/src/commands/` handlers to keep protocol dispatch thin and command logic modular, with `crates/aft/src/commands/tool_call.rs` acting as the single endpoint for tool invocation routing.
- Use `crates/aft/src/edit.rs`, `crates/aft/src/format.rs`, `crates/aft/src/callgraph.rs`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/inspect/` (including codebase-health scanners and the `oxc_engine/` liveness solver), `crates/aft/src/semantic_index.rs`, `crates/aft/src/search_index.rs`, `crates/aft/src/grep_executor.rs`, `crates/aft/src/memory.rs`, `crates/aft/src/compress/`, `crates/aft/src/patch/`, `crates/aft/src/pty_render.rs`, `crates/aft/src/response_finalize.rs`, `crates/aft/src/lsp/`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, and `crates/aft/src/legacy_partitions.rs` as shared engines behind multiple commands.

## Layers

**OpenCode integration layer:**
- Purpose: Register tools, load config, and attach post-execution metadata.
- Location: `packages/opencode-plugin/src/index.ts`
- Contains: Plugin bootstrap, tool-surface selection, hoisting logic, disabled-tool filtering, session-directory management, RPC server (exposing a live WebSocket endpoint for TUI notification and status invalidation pushes), auto-update checker hook
- Depends on: `packages/opencode-plugin/src/config.ts`, `packages/opencode-plugin/src/tools/*.ts`, `packages/aft-bridge/`
- Used by: OpenCode plugin loading through `@cortexkit/aft-opencode`

**Pi integration layer:**
- Purpose: Register tools, load config, and manage Pi host notifications.
- Location: `packages/pi-plugin/src/index.ts`
- Contains: Plugin bootstrap, tool-surface selection, hoisting logic, LSP auto-install (npm/github/project-relevance probes), `aft-status` command
- Depends on: `packages/pi-plugin/src/config.ts`, `packages/pi-plugin/src/tools/*.ts`, `packages/pi-plugin/src/commands/*.ts`, `packages/aft-bridge/`
- Used by: Pi coding agent through `@cortexkit/aft-pi`

**Shared bridge layer:**
- Purpose: Resolve or download the binary, start worker processes, manage ONNX runtime, format output, select and manage the transport pool, and forward requests. All harness adapters share this layer.
- Location: `packages/aft-bridge/src/bridge.ts`, `packages/aft-bridge/src/pool.ts`, `packages/aft-bridge/src/subc-transport.ts`, `packages/aft-bridge/src/transport.ts`, `packages/aft-bridge/src/transport-factory.ts`, `packages/aft-bridge/src/resolver.ts`, `packages/aft-bridge/src/downloader.ts`, `packages/aft-bridge/src/onnx-runtime.ts`, `packages/aft-bridge/src/migration.ts`, `packages/aft-bridge/src/zoom-format.ts`
- Contains: Transport factory routing selection (via user-tier `subc.connection_file`), subc client connection pooling, route caching per session-identity, background event subscriptions with independent reconnects, session bridge lifecycle, restart handling, version checks, binary discovery, binary download, ONNX runtime detection, storage migration, compact UI formatting, active logger, wait-aware transport budgets propagation (mapping `transportTimeoutMs` to route requests to avoid premature client-side timeouts during long command execution)
- Depends on: Node child-process APIs, GitHub releases, `onnxruntime-node`, `@cortexkit/subc-client`
- Used by: `packages/opencode-plugin/src/index.ts`, `packages/pi-plugin/src/index.ts`

**Unified CLI layer:**
- Purpose: Provide a single `npx @cortexkit/aft` entry point for setup, doctor, and LSP management across all harnesses.
- Location: `packages/aft-cli/src/index.ts`, `packages/aft-cli/src/commands/`
- Contains: `setup`, `doctor`, `doctor lsp`, `doctor --fix`, `doctor --clear`, `doctor --issue`; harness auto-detection (OpenCode/Pi) with `--harness` override; inlines `packages/aft-bridge/` in CLI bundle via literal specifiers to prevent dynamic runtime module resolution errors under `npx`
- Depends on: `packages/aft-bridge/`, harness adapter config paths
- Used by: End users via `npx @cortexkit/aft`

**Tool definition layer (OpenCode):**
- Purpose: Convert OpenCode tool arguments into the unified `tool_call` protocol request and perform permission checks.
- Location: `packages/opencode-plugin/src/tools/`
- Contains: Hoisted tools (edit/write/apply_patch), reading tools, import tools, structure tools, navigation tools, refactoring tools, safety tools, bash tools, conflict tools, AST tools, LSP tools, search tools, semantic tools (governed by isolated `aft_search` host permission checks independent from `grep`), inspect tools, permissions helpers, and the `callToolCall` transport wrapper (`packages/opencode-plugin/src/tools/_shared.ts`)
- Depends on: `packages/aft-bridge/src/pool.ts`, `packages/opencode-plugin/src/shared/`, `packages/opencode-plugin/src/metadata-store.ts`
- Used by: `packages/opencode-plugin/src/index.ts`

**Tool definition layer (Pi):**
- Purpose: Convert Pi tool arguments into the unified `tool_call` protocol request and perform permission checks.
- Location: `packages/pi-plugin/src/tools/`
- Contains: Hoisted tools (read/write/edit/grep) supporting cross-harness compatibility aliases (e.g. accepting `filePath` for `path` or vice versa), reading tools, import tools, structure tools, navigation tools, refactoring tools, safety tools, bash tools, conflict tools, AST tools, inspect tools, semantic tools, render helpers, diff-format helper, and the `callToolCall` transport wrapper (`packages/pi-plugin/src/tools/_shared.ts`)
- Depends on: `packages/aft-bridge/src/pool.ts`, `packages/pi-plugin/src/shared/`
- Used by: `packages/pi-plugin/src/index.ts`

**Protocol and command layer:**
- Purpose: Accept NDJSON requests, route tool calls via the unified `tool_call` command, and dispatch them to focused command handlers.
- Location: `crates/aft/src/main.rs`, `crates/aft/src/protocol.rs`, `crates/aft/src/commands/`, `crates/aft/src/run_tool_call.rs`, `crates/aft/src/subc_translate.rs`, `crates/aft/src/subc_format.rs`
- Contains: Request dispatch, response encoding, a unified `tool_call` routing engine, tool-to-command translation mapping, server-rendered agent-facing text formatting, control channel 0 health check responder, and standalone command handlers for read/write/edit/apply_patch/delete_file/move_file/outline/zoom/bash/bash_orchestrate/bash_status/bash_wait_detach/batch/grep/glob/search/imports/refactor/LSP/inspect/conflicts/checkpoints/state
- Depends on: `crates/aft/src/context.rs`, `crates/aft/src/parser.rs`, `crates/aft/src/callgraph.rs`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/edit.rs`, `crates/aft/src/semantic_index.rs`, `crates/aft/src/search_index.rs`, `crates/aft/src/compress/`
- Used by: `packages/aft-bridge/src/bridge.ts`

**Analysis and mutation engine layer:**
- Purpose: Parse code, compute call graphs, apply edits, format files, manage imports, index code semantically, and search with trigram indexes.
- Location: `crates/aft/src/parser.rs`, `crates/aft/src/callgraph.rs`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/callgraph_store/dead_code_projection.rs`, `crates/aft/src/edit.rs`, `crates/aft/src/format.rs`, `crates/aft/src/imports/`, `crates/aft/src/extract.rs`, `crates/aft/src/inspect/` (including `oxc_engine/` and `scanners/`), `crates/aft/src/semantic_index.rs`, `crates/aft/src/search_index.rs`, `crates/aft/src/grep_executor.rs`, `crates/aft/src/memory.rs`, `crates/aft/src/symbols.rs`, `crates/aft/src/calls.rs`, `crates/aft/src/symbol_cache_disk.rs`, `crates/aft/src/fuzzy_match.rs`, `crates/aft/src/ast_grep_hints.rs`, `crates/aft/src/ast_grep_lang.rs`, `crates/aft/src/query_shape.rs`, `crates/aft/src/pattern_compile.rs`, `crates/aft/src/patch/`, `crates/aft/src/pty_render.rs`
- Contains: Tree-sitter parsing using thread-local parsers (`REUSABLE_PARSERS`) to eliminate lock contention during parallel collection, optimized Rust symbol extraction traversing AST nodes directly without compiled queries, diff generation, formatter detection, type-checker integration, import engines (Java, C#, PHP, Kotlin, Scala, Swift, Ruby, Lua, C/C++, Perl, Solidity, Vue, Groovy), refactor helpers, semantic embedding index (covering Java, Kotlin, Scala, Swift, Ruby, PHP, Lua, Perl, R, Objective-C, Groovy, and other supported languages), disk-backed trigram search index, disk-backed symbol cache, persisted SQLite callgraph store builder, accelerated grep and glob query execution via `GrepExecutor` (with fallback walk limits and budgets when index is unavailable), process-wide memory attribution, allocation pressure relief, AST-grep integration, patch parsing (Add, Delete, and Update hunks) and matching engine, vt100 terminal rendering for PTY screen snapshots, codebase-health scanners, NestJS framework route and decorator spec entry point detection, same-file export liveness propagation, Go interface method liveness matching, and manifest-derived signal tiering for ranking/down-ranking findings (Product, Test, Tooling) and generated-file filtering
- Depends on: tree-sitter grammars, ast-grep, vt100, external formatter and checker processes, ONNX Runtime (optional), fastembed / OpenAI-compatible / Ollama backends (optional)
- Used by: `crates/aft/src/commands/*.rs`

**State and diagnostics layer:**
- Purpose: Hold per-process mutable state for backups, checkpoints, file watching, call graph cache, LSP state, database storage, bash background tasks, cache freshness tracking, file-system locking, and root-keyed writer leases.
- Location: `crates/aft/src/context.rs`, `crates/aft/src/backup.rs`, `crates/aft/src/checkpoint.rs`, `crates/aft/src/lsp/`, `crates/aft/src/db/`, `crates/aft/src/cache_freshness.rs`, `crates/aft/src/fs_lock.rs`, `crates/aft/src/bash_background/`, `crates/aft/src/callgraph_store/mod.rs`, `crates/aft/src/response_finalize.rs`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, `crates/aft/src/legacy_partitions.rs`
- Contains: `AppContext` with symlink path verification checks (recursively following chain hops to reject escaping paths), Windows verbatim path normalization via `canonicalize_normalized` to eliminate path comparison asymmetry, undo history, backup policies and disk-locking handlers, named checkpoints, watcher receiver, LSP manager, diagnostics store (which tracks and masks watcher-stale diagnostics for caching and pull reuse), document store, persistent database tables (backups, bash tasks, compression events, state, callgraph edges and nodes), cache-freshness tracker, file-system lockfile, background task registry, PTY process pool, callgraph store background channels, main-loop pending responses registry, root-keyed writer leases and reader marker file coordination (protecting active reader processes from index removal), and legacy harness coexistence guards (refusing writes into legacy layout partitions and validating migration space thresholds)
- Depends on: `notify`, LSP transport helpers, Rust `RefCell`, SQLite (via `db/` and `callgraph_store/`), `serde`
- Used by: All command handlers through `AppContext`

## Data Flow

**Tool invocation flow:**

1. Register tool definitions and config-driven surface selection -- `packages/opencode-plugin/src/index.ts` or `packages/pi-plugin/src/index.ts`. Before establishing a bridge or transport, the plugin's `bridgeFor` entry point verifies that the target project root directory exists, immediately throwing an error if it has been deleted (such as a reclaimed worktree) to prevent configuring or warming indexes for a dead root.
2. Resolve the active transport pool:
   - For standalone mode (default): send a unified `tool_call` command carrying the bare tool name and arguments over NDJSON -- `packages/aft-bridge/src/pool.ts`, `packages/aft-bridge/src/bridge.ts`
   - For subc mode (when `subc.connection_file` is set): send `{name, arguments}` as a data-plane request over a tool-provider route channel opened and cached per session identity (`BindIdentity`) -- `packages/aft-bridge/src/subc-transport.ts`
3. Dispatch the request to the target command or executor. Under standalone mode, dispatch through the Rust stdin NDJSON loop. Under subc mode, process frames via the TCP loopback client loop. Local `configure` commands are satisfied locally on bind. Native plumbing tools (`bash_drain_completions`, `bash_ack_completions`) bypass the tool manifest check but reinject the BIND session ID to keep sessions isolated. The execution outcome is processed through the server-side text formatter (`crates/aft/src/subc_format.rs`) and a pending response finalizer seam (`crates/aft/src/response_finalize.rs`). Subc relays `structuredContent` carrying the full flat response shape, which is re-lifted into `ToolCallResult` at the transport boundary to maintain parity with standalone mode. Monotonic phase traces (`PhaseTrace` and `ToolCallPhaseDurations`) track the timing/performance of subc tool calls across multiple phases (queuing, translation, execution, formatting, finalization, and egress) for slow-call diagnostics.

**Edit pipeline:**

1. Validate path and verify symlink safety (recursively follow components up to 40 hops to reject escaping paths) -- `crates/aft/src/context.rs`
2. Translate tool arguments to command parameters -- `crates/aft/src/subc_translate.rs`
3. Check edit permissions -- `packages/opencode-plugin/src/tools/permissions.ts` (or Pi equivalents). Under Pi, project-internal mutations apply without confirmation prompts, while external paths are validated by Rust path restrictions to avoid unanswered-prompt hangs.
4. Snapshot, mutate, diff, and validate content -- `crates/aft/src/edit.rs`
5. Auto-format and optionally collect diagnostics after write -- `crates/aft/src/format.rs`, `crates/aft/src/context.rs`. By default, edits return immediately without waiting for LSP diagnostics; pass `diagnostics: true` to enable synchronous wait for diagnostics, or run `aft_inspect` (diagnostics category) to check them asynchronously.

**Call-graph and navigation flow:**

1. Configure project root and initialize file watching -- `crates/aft/src/commands/configure.rs`
2. Query workspace-wide call dependencies via the persisted background-built callgraph store -- `crates/aft/src/callgraph_store/mod.rs`. The active database file is resolved dynamically via generation pointers (`.current` files) to allow atomic swaps and non-blocking reads. Under read-only mode, queries run via `ReadonlyCallGraphStore` directly against the SQLite database without write or rebuild operations. Refresh writes are offloaded to a background worker thread (`aft-callgraph-refresh`) to prevent blocking the watcher and configure loops.
3. Serve navigation commands such as callers, call-tree, impact, trace-to, and trace-data using the callgraph store adapter -- `crates/aft/src/commands/call_tree.rs`, `crates/aft/src/commands/callers.rs`, `crates/aft/src/commands/impact.rs`, `crates/aft/src/commands/trace_data.rs`, `crates/aft/src/commands/trace_to.rs`, `crates/aft/src/commands/trace_to_symbol.rs`, `crates/aft/src/commands/callgraph_store_adapter.rs`. By default, hide test files from results (controlled via the `includeTests` parameter) and collapse unresolved stdlib or external leaf calls in `call_tree` unless `includeUnresolved` is active. Truncate and return a summary (`hub_summary`) when results exceed 20 entries to save token context cost.
4. Serve symbol-level zoom inspection (`aft_zoom`), which fetches a symbol's implementation. If the target is a large container (class, struct, interface, etc., exceeding 150 lines), it renders a member-signature menu instead of the full body. For standard functions, it dedupes outgoing (`calls_out`) and incoming (`called_by`) call sites by name, aggregating duplicate occurrences under `extra_count` to minimize context token cost.

**Search and retrieval flow:**

1. Index project files using a disk-backed, pread-based trigram search index that keeps memory overhead bounded -- `crates/aft/src/search_index.rs`. To prevent redundant disk hashing and index re-verification loops during configure bind/warmup sequences, a verification memo with a 10-minute TTL manages cache freshness checks, utilizing metadata stat checks (`VerifyStrategy::StatFirst`) when possible rather than strict content hashing.
2. Optionally index with dense embeddings (fastembed, OpenAI-compatible, or Ollama) -- `crates/aft/src/semantic_index.rs`. Serialize cold semantic warmups by gating callgraph store building and Tier 2 diagnostics refreshes behind active cold semantic index seeds. Coalesce watcher-driven semantic re-embeds under a 15-second quiet window (`SEMANTIC_REFRESH_QUIET_WINDOW_MS`) to bundle edit bursts into a single collection pass, while masking changed files from search results until indexed to preserve query correctness. In tests, override this quiet window via the `AFT_SEMANTIC_QUIET_WINDOW_MS` environment variable.
3. Classify query shape (prose vs code) using the query shape parser -- `crates/aft/src/query_shape.rs`. Identify "type-concept identifier queries" (TitleCase PascalCase types combined with lowercase concepts) to trigger definition semantic priors.
4. Serve `grep` (trigram, full-text) and `aft_search` (semantic + hybrid) queries, delegating to `GrepExecutor` for accelerated path evaluation and enforcing execution safety limits (like `MAX_FALLBACK_WALK_FILES` and `FALLBACK_WALK_BUDGET`) during fallback walks when indexes are building or unavailable -- `crates/aft/src/grep_executor.rs`, `crates/aft/src/commands/grep.rs`, `crates/aft/src/commands/semantic_search.rs`. Interactive query embeddings are bounded by a dedicated `query_timeout_ms` configuration (clamped to 500..15000ms, defaulting to 3000ms) enforced via a `QueryBudget` to keep interactive requests fast without affecting background build/refresh timeouts, falling back to lexical search if query embedding fails or times out. Downrank generated documentation artifacts (e.g. minified CSS/JS, maps, SVGs) in lexical and hybrid search results.

**File read flow:**

1. Map read arguments and validate boundary permissions -- `packages/opencode-plugin/src/tools/reading.ts`, `packages/pi-plugin/src/tools/reading.ts`. Under project-root path restriction, allow restricted reading of files outside the project root if they are session-owned bash artifact outputs (stdout, stderr, exit code, or pty outputs) registered under the requesting session ID (validated via `AppContext::validate_read_path` using `BgTaskRegistry::is_session_owned_artifact_path`), while strictly rejecting any mutations (which continue to enforce project root boundaries via `AppContext::validate_path`).
2. Sniff content type (text vs binary/PDF/image) and read contents -- `crates/aft/src/commands/read.rs`
3. Process media attachments (resizing, orientation correction, and animation checks) and return them as base64-encoded attachment payloads alongside text content -- `crates/aft/src/commands/read.rs`, `crates/aft/src/subc_format.rs`

**Bash execution flow:**

1. Rewrite high-level commands (cat to read, grep to grep tool) -- `crates/aft/src/bash_rewrite/`
2. Scan for dangerous commands and prompt for permission -- `crates/aft/src/bash_permissions/`
3. Execute foreground, background, PTY, or synchronous wait/foreground modes. Foreground bash executions are orchestrated with a wait window (defaulting to 15s, clamped to config) and deferred to background tasks if they exceed the budget, while wait mode (`wait: true`) blocks to completion using the timeout budget, but detaches to a background task immediately if a `bash_wait_detach` signal is received (such as when a new user message is processed to prevent blocking agent interaction) -- `crates/aft/src/commands/bash_orchestrate.rs`, `crates/aft/src/commands/bash_status.rs`, `crates/aft/src/commands/bash_wait_detach.rs`, `crates/aft/src/pty_render.rs`, `crates/aft/src/bash_background/`
4. Compress output through the tiered compressor -- `crates/aft/src/compress/`

**Background completion wake flow:**

1. Maintain background subscriptions for completions. Under standalone mode, completion notifications push directly over the bridge process stdout channel. Under subc mode, the plugin maintains a persistent `BgSubscription` over a dedicated second route channel -- `packages/aft-bridge/src/subc-transport.ts`.
2. When a background task completes, Rust marks the session's background channel wake-pending using an epoch-based tracking mechanism to prevent race conditions during concurrent tool/maintenance execution (i.e. to avoid suppressing wakes armed after a maintenance snapshot). It emits a coalesced, lossy `{op: "bg_events"}` wake nudge at most once per 250ms tick -- `crates/aft/src/subc.rs`.
3. The plugin receives the nudge via `onBgEventsNudge` and triggers an unconditional forced-drain (`handleSubcBgEventsNudge`) to fetch, deliver, and ack the completions -- `packages/opencode-plugin/src/bg-notifications.ts`, `packages/pi-plugin/src/bg-notifications.ts`. To prevent double-delivery during concurrent tool/forced-drain execution, the plugin maintains two transient per-session task-ID tracking sets: `deliveringTaskIds` (delivery in flight) and `deliveredAwaitingAckTaskIds` (delivered but unacknowledged). Forced drains skip tasks in either set, and automatically re-ack tasks in the awaiting-ack set to terminate subc re-nudge loops. The plugin uses daemon reconciliation (rather than a static time-based TTL) to prune `deliveredAwaitingAckTaskIds`, removing tasks only when they are no longer returned in the daemon's list of outstanding tasks.
4. If a subc background subscription channel drops, `BgSubscription` drives its own independent reconnect loop to resubscribe without waiting for new tool traffic, retrieving any completions queued while disconnected.

**Binary resolution flow:**

1. Check cache, npm platform package, PATH, and cargo install locations -- `packages/aft-bridge/src/resolver.ts`
2. Download and checksum-verify a release asset when local resolution fails -- `packages/aft-bridge/src/downloader.ts`
3. Start bridges against the resolved binary and hot-swap after version mismatch -- `packages/aft-bridge/src/bridge.ts`, `packages/aft-bridge/src/pool.ts`

**Artifact ownership and read-only caching flow:**

1. During `configure`, verify repository scopes, and resolve root-keyed cache directories: `<storage>/callgraph/<artifact_cache_key>` and `<storage>/inspect/<project_scope_key>` -- `crates/aft/src/commands/configure.rs`, `crates/aft/src/artifact_owner.rs`, `crates/aft/src/root_cache.rs`. If the `storage_dir` parameter is not supplied (e.g. daemon connections), default it to the shared CortexKit storage root `~/.local/share/cortexkit/aft/` to prevent RAM-only fallback and index regeneration. Retrieve or record the cache key in `<storage>/cache-keys.json` to memoize the mapping and avoid spawning redundant git process probes. During `configure` (client bind), heavy artifact index loading (deserialization) is deferred until after the bind acknowledgement has been returned to the client, kicking off only during the post-bind maintenance sweep (`drain_deferred_configure_maintenance`).
2. Write an `owner.json` manifest to the cache directory carrying the current checkout's scope key, path, PID, and hostname.
3. If no manifest exists, or if the existing manifest belongs to the same checkout, or if the owning process is dead (stale heartbeat or inactive process ID/hostname), reclaim and write a new lease ("Owner" mode).
4. If an active process on another checkout owns the manifest, claim "ReadOnly" mode.
5. Coordinate concurrent cache writes by acquiring a domain-specific `WriterLease` via `WriterLease::acquire_shared`. In "ReadOnly" mode or when a write lease cannot be acquired, heavy operations like cold callgraph builds, search index generation, and semantic index warming are disabled. Any search or semantic search queries read the cached index files using strict read-only openers (including `ReadonlyCallGraphStore` for callgraph reads) -- `crates/aft/src/readonly_artifacts.rs`, `crates/aft/src/root_cache.rs`, `crates/aft/src/callgraph_store/mod.rs`. Borrow-only roots (e.g., mason worktrees) report search and callgraph indexes as ready since they query shared read-only artifacts. Subc health reports only mark the daemon degraded when dispatch is impaired (e.g., actor snapshot contention), leaving roots warming background indexes or serving read-only work with an Ok health verdict -- `crates/aft/src/subc/health.rs`.
6. Write a `0600` read-marker file under `<domain>/readers/<generation-label>/` to register active reader sessions -- `crates/aft/src/root_cache.rs`. Active readers touch their markers to update heartbeats (at most once every 5 seconds). During sweeps, garbage collection removes old generation SQLite databases unless they have a protected read marker, or if their age exceeds the absolute retention limit of 6 hours (`MARKED_GENERATION_RETENTION_TTL`).
7. Enforce coexistence guards using `legacy_partitions.rs` to refuse write operations into legacy layout partitions and check free space requirements using a 1.5× disk-floor preflight before copying legacy folders to the new root-keyed layout. If a legacy callgraph partition is detected, migrate it using the non-blocking, online SQLite backup API (`rusqlite::Connection::backup`) running page-by-page (128 pages per step) under retry and wall-clock budgets.
8. The active "Owner" session emits periodic heartbeat file-writes to the lease manifest file during its event loop tick -- `crates/aft/src/main.rs`.

**Idle root eviction and memory management flow:**

1. Track the idle time of active project roots based on request activity. When a root has been idle for a configured TTL (`IDLE_ROOT_TTL`), the subc maintenance loop triggers eviction -- `crates/aft/src/subc/mod.rs`, `crates/aft/src/context.rs`.
2. Evict root-scoped artifact handles (callgraph store, search index, semantic index, and inspect caches) from memory via `evict_idle_artifacts`. Subsequent queries (e.g. glob, grep, semantic search) trigger an asynchronous background reload of the evicted index files (using `trigger_search_index_reload_if_evicted` and `trigger_semantic_index_reload_if_evicted` serialized via `artifact_reload_lock` to prevent duplicate concurrent loaders) while serving the immediate request through the fallback walk. Stop the file watcher runtime in the background.
3. Under macOS, after sweeping idle roots, request memory pressure relief from the OS allocator via `relieve_allocator_pressure` to reclaim unused pages -- `crates/aft/src/memory.rs`.
4. Track process-wide and root-scoped memory usage (including SQLite allocator metrics and OS RSS memory) via memory snapshots returned in status reports -- `crates/aft/src/memory.rs`, `crates/aft/src/commands/status.rs`. Key status memory roots by `ProjectRootId` on all platforms to prevent path-casing/verbatim comparison mismatches.

## Key Abstractions

**BinaryBridge:**
- Purpose: Keep one live `aft` subprocess available for request/response traffic.
- Location: `packages/aft-bridge/src/bridge.ts`
- Pattern: Persistent child-process adapter with timeout-triggered restart

**BridgePool:**
- Purpose: Scope bridges per OpenCode/Pi session and preserve isolated undo history.
- Location: `packages/aft-bridge/src/pool.ts`
- Pattern: Session-keyed object pool with LRU eviction

**AftTransportPool / AftProjectTransport / AftTransport:**
- Purpose: Abstract transport details (standalone NDJSON vs daemon-backed subc) behind a unified, session-closed client-facing interface.
- Location: `packages/aft-bridge/src/transport.ts`, `packages/aft-bridge/src/transport-factory.ts`
- Pattern: Factory-created abstraction layer.

**SubcTransportPool:**
- Purpose: Provide route cache and connection management over the authenticated subc client.
- Location: `packages/aft-bridge/src/subc-transport.ts`
- Pattern: Cache per-identity session lifecycle records (`SessionRecord`) containing tool route entries, background event subscriptions (`BgSubscription`), closed states, and in-flight request counts. Force single-flight connection/route opening (preventing duplicate channel leaks) and handle safe session teardown by executing synchronous state mutations before any asynchronous transport cleanup to prevent in-flight request resurrection. Feature a client-level half-open backstop that drops/reconnects the client after consecutive non-transient request failures (e.g. timeouts) to recover from silent connection drops.

**BgSubscription:**
- Purpose: Consume the daemon's held-open `bg_events` wake lane.
- Location: `packages/aft-bridge/src/subc-transport.ts`
- Pattern: Resubscribe itself independently on stream drop or error without waiting for tool traffic, driving unconditional forced-drains.

**BindTrust:**
- Purpose: Enforce caller-identity (principal) trust levels on the subconscious routing daemon connection.
- Location: `crates/aft/src/subc.rs`
- Pattern: Map route binds onto `FirstParty` or `Untrusted` levels by inspecting the caller's principal metadata. `Principal::Direct` and reserved `llm-runner`/`aft` module principals resolve to `FirstParty` trust. Other callers (e.g., facade `subc-mcp` module, unverified principal, or absent principal) map to `Untrusted` trust. `Untrusted` routes deny bash/shell executions, force project-root path restriction check validation even if globally disabled in user config, and block background task observation/wake replay.

**Tool groups (OpenCode):**
- Purpose: Group related OpenCode tool definitions by capability surface.
- Location: `packages/opencode-plugin/src/tools/hoisted.ts`, `packages/opencode-plugin/src/tools/reading.ts`, `packages/opencode-plugin/src/tools/imports.ts`, `packages/opencode-plugin/src/tools/structure.ts`, `packages/opencode-plugin/src/tools/navigation.ts`, `packages/opencode-plugin/src/tools/refactoring.ts`, `packages/opencode-plugin/src/tools/safety.ts`, `packages/opencode-plugin/src/tools/conflicts.ts`, `packages/opencode-plugin/src/tools/lsp.ts`, `packages/opencode-plugin/src/tools/ast.ts`, `packages/opencode-plugin/src/tools/bash.ts`, `packages/opencode-plugin/src/tools/bash_watch.ts`, `packages/opencode-plugin/src/tools/bash_write.ts`, `packages/opencode-plugin/src/tools/inspect.ts`, `packages/opencode-plugin/src/tools/search.ts`, `packages/opencode-plugin/src/tools/semantic.ts`, `packages/opencode-plugin/src/tools/permissions.ts`, `packages/opencode-plugin/src/tools/hoisted-internals.ts`
- Pattern: Thin TypeScript adapters delegating to the unified `tool_call` transport

**Tool groups (Pi):**
- Purpose: Group related Pi tool definitions by capability surface.
- Location: `packages/pi-plugin/src/tools/hoisted.ts`, `packages/pi-plugin/src/tools/reading.ts`, `packages/pi-plugin/src/tools/imports.ts`, `packages/pi-plugin/src/tools/structure.ts`, `packages/pi-plugin/src/tools/navigate.ts`, `packages/pi-plugin/src/tools/refactor.ts`, `packages/pi-plugin/src/tools/safety.ts`, `packages/pi-plugin/src/tools/conflicts.ts`, `packages/pi-plugin/src/tools/ast.ts`, `packages/pi-plugin/src/tools/bash.ts`, `packages/pi-plugin/src/tools/semantic.ts`, `packages/pi-plugin/src/tools/inspect.ts`, `packages/pi-plugin/src/tools/fs.ts`, `packages/pi-plugin/src/tools/diff-format.ts`, `packages/pi-plugin/src/tools/render-helpers.ts`
- Pattern: Thin TypeScript adapters delegating to the unified `tool_call` transport with Pi-specific schema configuration

**ToolCallCommand:**
- Purpose: Route and execute client-facing agent tools via a single request.
- Location: `crates/aft/src/commands/tool_call.rs`, `crates/aft/src/run_tool_call.rs`
- Pattern: Unified request translator and response formatting coordinator
- Contains: `subc_translate` mapping, `subc_format` text rendering, and dispatching to target command handlers

**AppContext:**
- Purpose: Centralize runtime state for commands inside the Rust worker.
- Location: `crates/aft/src/context.rs`
- Pattern: Interior-mutable service container for a single-threaded request loop
- Contains: `CallGraph`, `CallGraphStore`, `SearchIndex`, `SemanticIndex`, `BgTaskRegistry`, `FilterRegistry`, database connections, LSP manager, undo history

**CallGraphStore:**
- Purpose: Persisted SQLite database of project-wide call dependencies.
- Location: `crates/aft/src/callgraph_store/mod.rs`
- Pattern: Background-built SQLite schema containing resolved and name-only call edges, refreshed incrementally on file edits, and queried by navigation commands. Stores are resolved dynamically via generation pointers (`.current` files) to allow safe atomic swaps and non-blocking reads. Under read-only mode, queries read the SQLite file via `ReadonlyCallGraphStore` to prevent write collisions. Returns a `Building` status during cold builds. Cold-build warming is deferred while a cold semantic index seed is actively collecting or embedding.

**Oxc Liveness Engine:**
- Purpose: Perform liveness and dead-code analysis for JavaScript/TypeScript and other supported files.
- Location: `crates/aft/src/inspect/oxc_engine/`
- Pattern: Build a liveness graph starting from resolved entry points. Features include:
  - Framework-decorator roots: Seeding NestJS `@Controller` or `@Injectable` exports as live.
  - Same-file value reference propagation: Keeping exported helpers live when they are referenced locally in the same file, even if the file is not reached from a global entry point.
  - Language-specific dispatch rules: Keeping Go interface methods and receiver/interface dispatches live by matching method names to keep their bodies reachable.

**ProjectRoles / SignalTier:**
- Purpose: Rank inspect findings to prioritize actionable product code over tests, tooling, and generated artifacts.
- Location: `crates/aft/src/inspect/entry_points.rs`, `crates/aft/src/inspect/generated.rs`
- Pattern: Manifest-derived classification mapping files onto `Product`, `Test`, or `Tooling` tiers, combining with generated-file marker detection to sort high-signal findings first in `aft_inspect` summary previews and drill-downs.

**CallGraph:**
- Purpose: Cache per-file local call data and resolve immediate import edges.
- Location: `crates/aft/src/callgraph.rs`
- Pattern: Lazy workspace index with invalidation on watcher events.

**SearchIndex:**
- Purpose: Provide fast trigram-based full-text search across the project.
- Location: `crates/aft/src/search_index.rs`
- Pattern: Disk-backed (pread) postings index written to a single cache file (`cache.bin`) and read on-demand to maintain a bounded RAM footprint, rebuilding in the background on watcher events.

**SemanticIndex:**
- Purpose: Provide dense-embedding semantic search across the project.
- Location: `crates/aft/src/semantic_index.rs`
- Pattern: Optional index backed by fastembed (local), OpenAI-compatible, or Ollama; configurable `max_files` cap

**BgTaskRegistry:**
- Purpose: Manage background bash tasks and PTY sessions.
- Location: `crates/aft/src/bash_background/registry.rs`
- Pattern: Thread-safe registry with a watchdog thread for output compression, completion notification, and task lifecycle cleanup. Generate unique task IDs using 64-bit entropy (represented as a 16-hex character slug `bash-{16hex}`) to prevent ID reuse collisions during subc delivery de-duplication.

**Compressor:**
- Purpose: Reduce hoisted-bash output to relevant tokens.
- Location: `crates/aft/src/compress/` (multiple modules), `crates/aft/src/compress/mod.rs`
- Pattern: Trait-based dispatch with per-command Rust modules, output-shape sniffers, package-manager modules, declarative TOML filters, and a generic fallback

**PendingResponses:**
- Purpose: Hold and poll deferred or orchestrated requests in the main loop.
- Location: `crates/aft/src/response_finalize.rs`, `crates/aft/src/main.rs`
- Pattern: Vector-backed pending queue that polls registered completion steps and runs the finalizer seam before writing responses.

**PatchEngine:**
- Purpose: Parse, match, and apply unified file diffs/patches.
- Location: `crates/aft/src/patch/` (including `mod.rs`, `parser.rs`, `matcher.rs`, `apply.rs`)
- Pattern: AST/line-based parser that maps update/create/delete hunks to target files, matches fuzzy sequences, and executes atomic writes with rollback support.

**PtyRenderer:**
- Purpose: Render raw PTY output bytes into a readable screen.
- Location: `crates/aft/src/pty_render.rs`
- Pattern: vt100 terminal state parser that outputs clean, grid-aligned text for screen snapshots.

**Harness:**
- Purpose: Represent the coding-agent harness (OpenCode or Pi) for config and CLI dispatch.
- Location: `crates/aft/src/harness.rs`
- Pattern: Simple enum with serde round-trip and display/from-str

**ArtifactOwnerLease / ArtifactOwnerClaim:**
- Purpose: Prevent concurrent AFT processes from corrupting shared cache artifacts for the same repository while allowing safe read-only fallbacks.
- Location: `crates/aft/src/artifact_owner.rs`, `crates/aft/src/readonly_artifacts.rs`
- Pattern: File-system lease with active process liveness tracking.
- Contains: Unique process identification (`pid`, `hostname`), heartbeat updates during the event loop to preserve the lease, stale lease reclamation, and read-only index adapters that query the cached search and semantic indexes without trigger-building or modifying files.

**WriterLease / ReadMarker:**
- Purpose: Coordinate safe multi-session access to the root-keyed project cache.
- Location: `crates/aft/src/root_cache.rs`
- Pattern: File-locked writer lease ensuring single-writer exclusivity combined with private read-marker JSON files (created `0600` under `<domain>/readers/`) to track active reader processes.
- Contains: `WriterLease` domain mapping (`RootCacheDomain::Callgraph` or `RootCacheDomain::Inspect`), epoch validation, and process identification metadata for reader heartbeat cleanup. Sweeps clean up dead-PID and expired cross-host markers and delete old SQLite generation files.

**LegacyPartitionGuards:**
- Purpose: Protect legacy harness-scoped folders and manage space limits during layout migration.
- Location: `crates/aft/src/legacy_partitions.rs`
- Pattern: Coexistence write filters and space preflight check.
- Contains: Layout checkers preventing writes to legacy partition structures, disk space size scanning, and 1.5× disk-floor verification before copying files to the new root-keyed cache location.

**GrepExecutor:**
- Purpose: Coordinate accelerated search query execution across project roots and external directories.
- Location: `crates/aft/src/grep_executor.rs`
- Pattern: Query router and execution planner.
- Contains: In-index search routing combined with fallback manual walks capped by size (`MAX_FALLBACK_WALK_FILES`) and time budgets (`FALLBACK_WALK_BUDGET`) when indexes are building or unavailable.

**MemoryEstimate / MemorySnapshot:**
- Purpose: Track, attribute, and report process-wide and subsystem-specific memory usage.
- Location: `crates/aft/src/memory.rs`
- Pattern: Diagnostic structures and OS memory allocators hook.
- Contains: Subsystem memory estimation helpers, SQLite allocator query bindings (`sqlite3_memory_used`), platform-specific resident set size (RSS) query maps, and macOS-specific pressure relief bindings (`malloc_zone_pressure_relief`) to release unused pages during idle sweeps.

## Entry Points

**OpenCode plugin entry point:**
- Location: `packages/opencode-plugin/src/index.ts`
- Triggers: OpenCode loads the `@cortexkit/aft-opencode` plugin
- Responsibilities: Load config, resolve the binary via `@cortexkit/aft-bridge`, create the bridge pool, register tool definitions, manage session lifecycle, run auto-update checker, handle background completion push frames

**Pi plugin entry point:**
- Location: `packages/pi-plugin/src/index.ts`
- Triggers: Pi loads the `@cortexkit/aft-pi` plugin
- Responsibilities: Load config, resolve the binary via `@cortexkit/aft-bridge`, create the bridge pool, register tool definitions, manage LSP auto-install (npm + GitHub), handle background completion push frames

**Unified CLI entry point:**
- Location: `packages/aft-cli/src/index.ts`
- Triggers: `npx @cortexkit/aft` invocation
- Responsibilities: Parse argv, auto-detect harness, dispatch to `setup`, `doctor`, or `doctor lsp` commands

**Shared bridge entry point:**
- Location: `packages/aft-bridge/src/index.ts`
- Triggers: Imported by `@cortexkit/aft-opencode` and `@cortexkit/aft-pi`
- Responsibilities: Export `BinaryBridge`, `BridgePool`, binary resolution (`downloadBinary`, `ensureBinary`, `findBinary`), ONNX runtime detection (`ensureOnnxRuntime`, `isOrtAutoDownloadSupported`), storage migration (`ensureStorageMigrated`), compact formatting helpers

**Rust protocol entry point:**
- Location: `crates/aft/src/main.rs`
- Triggers: `packages/aft-bridge/src/bridge.ts` spawns the `aft` binary
- Responsibilities: Read NDJSON requests from stdin, dispatch handlers, drain watcher and LSP events, compress background task output, and write JSON responses

**Rust subc daemon entry point:**
- Location: `crates/aft/src/main.rs`, `crates/aft/src/subc.rs`
- Triggers: Spawned with the `--subc <connection-file>` argument
- Responsibilities: Connect to the subc daemon over loopback TCP, authenticate using HMAC handshake, and process frames via tokio client loop routed through the per-actor executor

**Rust binary CLI subcommands:**
- Location: `crates/aft/src/cli/`
- Triggers: `aft warmup` or `aft migrate-storage` invocations
- Responsibilities: Pre-warm tree-sitter grammars, migrate storage between legacy and CortexKit paths

**Release automation entry point:**
- Location: `.github/workflows/release.yml`
- Triggers: Git tag pushes matching `v*`
- Responsibilities: Test the workspace, build platform binaries, publish crates and npm packages, and create a GitHub release

## Error Handling

**Strategy:** Return structured Rust `Response::error` payloads from command handlers, convert failed responses into plugin-side exceptions, and restart hung or crashed worker processes in `packages/aft-bridge/src/bridge.ts`. Under subc mode, mutating panics return an `actor_fatal` error code which triggers a fatal teardown and client teardown across the daemon connection.

## Honest Reporting Convention

**Goal:** an agent reading any AFT response must be able to distinguish three states without ambiguity: (1) the work could not be performed, (2) the work was performed and the result is complete, (3) the work was performed but the result is partial.

**Rule (tri-state):**

1. **`success: false` + `code` + `message`** -- the requested work could not be performed. Codes are machine-actionable strings such as `"path_not_found"`, `"no_lsp_server"`, `"project_too_large"`, `"invalid_request"`, `"ambiguous_match"`. The agent must read the message before continuing.

2. **`success: true` + completion signaling** -- the work was performed. Tools that produce results MUST report whether the result is complete and, if not, name the gaps. Conventional fields:
    - `complete: true` -- the agent can trust absence of items in the result
    - `complete: false` + a named gap field -- partial result. Gap fields include `pending_files`, `unchecked_files`, `scope_warnings`, `skipped_files: [{file, reason}]`, `walk_truncated`
    - `removed: bool` (mutations) -- did the file actually change? `false` is a valid success when the requested change was a no-op.
    - `no_files_matched_scope: bool` (search tools) -- distinguishes "the path/glob you gave me resolved to zero files" from "I searched N files and found nothing"

3. **Side-effect skip codes** -- when the main work succeeded but a non-essential side step was skipped (e.g. post-write formatting), use a `<step>_skipped_reason` field so the agent gets specific feedback without treating the whole call as a failure. Approved values:
    - `format_skipped_reason`: `"unsupported_language"` | `"no_formatter_configured"` | `"formatter_not_installed"` | `"formatter_excluded_path"` | `"timeout"` | `"error"`
    - `validate_skipped_reason`: `"unsupported_language"` | `"no_checker_configured"` | `"checker_not_installed"` | `"timeout"` | `"error"`

**Anti-patterns this convention exists to prevent:**

- Returning `success: true` with empty results when the scope (path/glob) didn't resolve to any files -- the agent reads it as "all clear" but really nothing was checked. Return `no_files_matched_scope: true` (when the scope was syntactically valid but matched zero files) or `success: false, code: "path_not_found"` (when a passed path doesn't exist).
- Reusing one skip-reason string for two distinct causes (e.g., `"not_found"` for both "language has no formatter configured" and "configured formatter binary missing"). The agent has different remediations for each -- split them.
- Silently dropping files that fail to parse / open / decode inside a multi-file or directory operation. Always include a `skipped_files: [{file, reason}]` array so the agent knows X out of Y files were actually processed.
- Asserting `success: true` after a partial transaction without a `complete: false` flag and a list of pending work.

**Where this is documented in code:** `crates/aft/src/protocol.rs` `Response` doc comment carries the canonical rule and the approved field set. New tools must follow this convention; existing tools are migrating.

## Bash Output Compression

**Goal:** reduce hoisted-bash output to fewer tokens while keeping the information the agent actually needs (errors, summaries, ref updates) and discarding the noise (progress bars, repeated headers, deep nested directory listings).

**Five-tier dispatch in `crates/aft/src/compress/mod.rs`:**

1. **Specific Rust `Compressor` modules** -- hand-written parsers for high-traffic tools identified by tool tokens (e.g. `git`, `cargo`, `vitest`). Always wins when matched. Each module lives in its own file under `crates/aft/src/compress/` (e.g. `git.rs`, `cargo.rs`, `eslint.rs`) and implements the `Compressor` trait (`fn tokens(&[&str]) -> bool` + `fn compress(&str, &str) -> String`). Modules include `biome`, `bun`, `cargo`, `eslint`, `git`, `go`, `mypy`, `next`, `npm`, `playwright`, `pnpm`, `prettier`, `pytest`, `ruff`, `tsc`, `vitest`.

2. **Output-shape `Compressor` sniffers** -- inner-tool parsers that recognize their own private summaries even when invoked through wrappers such as `npm test`, `make test`, or `./scripts/check.sh`. Tried after specific modules, before package-manager modules.

3. **Package-manager `Compressor` modules** -- broad head-token matchers (`npm`, `pnpm`, `bun`) that compress unclaimed package-manager output.

4. **Declarative TOML filters** -- strip + truncate + cap + shortcircuit rules for the long tail of CLI tools, loaded from three sources at startup with project > user > builtin priority by filename:
    - **Builtin**: shipped via `include_str!()` from `crates/aft/src/compress/builtin_filters/*.toml`, registered in `crates/aft/src/compress/builtin_filters.rs::ALL`. Currently 22 filters: ansible-playbook, aws, curl, deno, df, docker, du, find, gh, gradle, helm, kubectl, ls, make, pip, psql, terraform, tree, uv, wc, wget, xcodebuild.
    - **User**: `<storage_dir>/filters/*.toml` (XDG-aware via the active `storage_dir`)
    - **Project**: `<project_root>/.cortexkit/aft/filters/*.toml` -- gated by `crate::compress::trust`; never loaded for an untrusted project

5. **Generic fallback** -- ANSI strip + consecutive-line dedup + middle-truncate. Always applies when no Rust module or TOML filter matches.

**Pipeline for TOML filters** (in `crates/aft/src/compress/toml_filter.rs::apply_filter`):

1. ANSI strip (when `[ansi].strip` is true; default true)
2. `[strip]` regexes drop matching lines (multiline mode)
3. `[shortcircuit]` checks remaining content; if matched, return `replacement`. Builtin filters never fabricate non-empty output for empty inputs (empty output stays empty).
4. `[truncate]` middle-truncates per line at `line_max` chars
5. `[cap]` enforces `max_lines` with `keep = "head" | "tail" | "middle"`

**Trust model** (`crates/aft/src/compress/trust.rs`): project filters can lie about output (e.g. strip real failures and replace with `tests: ok`). They are off by default. Users opt in via `npx @cortexkit/aft doctor filters trust`, which records the canonicalized project root in `<storage_dir>/trusted-filter-projects.json` (atomic temp-file rename, deserialized fail-closed). The CLI also exposes `untrust`, `trust --list`, `--show <name>`, and the default list view.

**Concurrency:** the filter registry is exposed as `Arc<RwLock<FilterRegistry>>` so the `BgTaskRegistry` watchdog thread can compress completed task output without holding `AppContext`. The compressor is installed as a closure on `BgTaskRegistry` from `crates/aft/src/main.rs` after `AppContext::new` constructs both.

**Configure invalidation:** `crates/aft/src/commands/configure.rs::handle_configure` calls `ctx.sync_bash_compress_flag()` and `ctx.reset_filter_registry()` on every configure so changes to `experimental.bash.compress`, `storage_dir`, `project_root`, or trust state pick up immediately without restart.

**Compression site:** terminal-state output only. Live tail of running tasks (via `bash_status` polling) is shown raw so agents debugging long commands see exactly what the process emitted. Compression fires inside `BgTaskRegistry::maybe_compress_snapshot` (status / list paths) and `enqueue_completion_locked` (completion frame + `bash_drain_completions` cache).

## Cross-Cutting Concerns

**Logging:** Write plugin logs through `packages/opencode-plugin/src/logger.ts` or `packages/pi-plugin/src/logger.ts` and Rust logs through `env_logger` in `crates/aft/src/main.rs`.

**Caching:** Cache resolved binaries in `~/.cache/aft/bin` through `packages/aft-bridge/src/downloader.ts`, cache session bridges in `packages/aft-bridge/src/pool.ts`, cache tool availability in `crates/aft/src/format.rs`, cache call-graph state in `crates/aft/src/callgraph.rs`, cache trigram search indexes on disk via `crates/aft/src/search_index.rs`, cache semantic embeddings on disk via `crates/aft/src/semantic_index.rs`, and cache symbol data on disk via `crates/aft/src/symbol_cache_disk.rs`.

**Storage:** Store undo snapshots in `crates/aft/src/backup.rs` using the append-only v2 layout (indexing files under `<session_hash>/<path_hash>/` with locks to support multi-session project-shared bridges) governed by configured backup policies (`backup.enabled`, `backup.max_depth`, `backup.max_file_size`). Store named checkpoints in `crates/aft/src/checkpoint.rs`, database tables (backups, bash tasks, compression events, state) in `crates/aft/src/db/`, callgraph database in `crates/aft/src/callgraph_store/mod.rs`, inspect diagnostics cache in `crates/aft/src/inspect/cache.rs`, pending UI metadata in `packages/opencode-plugin/src/metadata-store.ts`, and downloaded binaries in the cache directory managed by `packages/aft-bridge/src/downloader.ts`. Storage lives under the CortexKit shared root (`~/.local/share/cortexkit/aft/`), migrated from the legacy path via `crates/aft/src/migrate_storage.rs`.
