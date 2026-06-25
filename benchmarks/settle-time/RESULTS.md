# AFT settle-time benchmark results

Date: 2026-06-04. Host: macOS arm64 + OrbStack Docker, runtime forced to `linux/amd64` to match the shipped linux-x64 fastembed/ONNX path. AFT binary was built inside Docker via `tests/docker/Dockerfile.build-linux`.

Configuration measured: `search_index=true`, `semantic_search=true`, `semantic.backend="fastembed"`, Tier-2 inspect enabled. Tier-2 was **not** forced by an `inspect` request; all runs use the watcher-driven `ConfigureWarm` scan, which fired around +90s after configure.

Settle definition: all known phases terminal (search index, semantic index, dead_code, unused_exports, duplicates) and process-tree CPU below 5% of one core for 30s. CPU/RSS are for the AFT process tree.

## Shipped defaults vs measured scenario

Source defaults do **not** enable semantic search:

- `crates/aft/src/config.rs`: `search_index=false`, `semantic_search=false`; semantic backend default is `fastembed` with `max_files=20_000` if semantic is enabled.
- `packages/opencode-plugin/src/config.ts`: schema comments also say `search_index` default false and `semantic_search` default false.
- Plugin inspect is enabled by default (`inspect.enabled` defaults true), so default users can pay Tier-2 cost even when search/semantic are off, depending on plugin configuration/session path.

The measurements below intentionally enable semantic because that is the v0.35.x regression scenario requested for this benchmark.

## Headline table

| Repo | Commit | Tracked files | AFT source files | Search-index files | t_search | t_semantic | t_Tier2 | t_settle | Peak CPU (% of one core) | Peak RSS | Result |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| honojs/hono | `c78932d745cd` | 493 | 429 | 486 | 1.3s | 37.4s | 120.0s | 153.1s | 889.7% @ semantic embed | 1.29 GiB | settled |
| vuejs/core | `9d92dbded200` | 703 | 621 | 703 | 1.3s | 46.4s | 205.3s | 289.4s | 910.5% @ semantic embed | 2.77 GiB | settled |
| nestjs/nest | `c47518f611e6` | 2,125 | 2,020 | 2,125 | 1.4s | 89.1s | 152.4s | 192.3s | 975.0% @ semantic embed | 2.52 GiB | settled |
| microsoft/vscode (bounded rerun) | `4efb1f746ea8` | 15,566 | 12,860 | 15,201 | 13.3s | not terminal by 30m | not terminal by 30m | did not settle within 30m | 1821.3% @ search+semantic extraction | 11.89 GiB @ Tier-2 callgraph snapshot | **timeout** |

`microsoft/vscode` is the bounded representative mid-size run requested after re-scope: 12,860 AFT source files, i.e. inside the requested ~5k-15k source-file band. At the 30-minute cutoff it was still in `tier2:callgraph_snapshot`, using ~935.8% CPU and ~6.02 GiB RSS in the last sample.

A separate 30k-60k source-file run was not attempted after the 30-minute VSCode run because the mid-size run already failed the cap and was still burning ~9-10 cores in the suspected Tier-2 phase. The harness is parameterized for that next run, but this report stops at bounded, actionable data rather than launching another likely non-settling 30-minute container.

## Tier-2 breakdown

| Repo | Tier-2 reason | dead_code | callgraph snapshot inside dead_code | unused_exports | duplicates |
|---|---|---:|---:|---:|---:|
| honojs/hono | `configure_warm` | +90.154s → +119.180s (29.0s) | +90.259s → +101.488s (11.2s; 429 files) | 0.242s | 0.560s |
| vuejs/core | `configure_warm` | +90.244s → +204.327s (114.1s) | +90.337s → +140.615s (50.3s; 621 files) | 0.274s | 0.652s |
| nestjs/nest | `configure_warm` | +90.174s → +151.304s (61.1s) | +90.281s → +108.964s (18.7s; 2,020 files) | 0.468s | 0.621s |
| microsoft/vscode | `configure_warm` | started +90.489s; not complete by +1801.239s | started +90.883s; still running at cutoff (12,860 files) | not reached | not reached |

For all settled repos, `unused_exports` and `duplicates` were sub-second. The expensive Tier-2 category was `dead_code`; the expensive subphase inside `dead_code` was the callgraph snapshot build.

## Phase timelines

### honojs/hono (`c78932d745cd`)

- +1.3s: search index ready.
- +5.0s → +5.2s: semantic collect, 2,248 chunks / 376 files.
- +5.2s → +36.5s: semantic embed, 2,248 chunks, 36 batches, 31.3s, 71 chunks/s.
- +37.4s: semantic ready.
- +90.2s: Tier-2 `configure_warm` scheduled; `dead_code` starts.
- +90.3s → +101.5s: Tier-2 callgraph snapshot, 429 files.
- +119.2s: `dead_code` complete.
- +119.4s: `unused_exports` complete.
- +120.0s: `duplicates` complete.
- +153.1s: CPU idle window satisfied; settled.

### vuejs/core (`9d92dbded200`)

- +1.3s: search index ready.
- +0.7s → +1.1s: semantic collect, 3,340 chunks / 543 files.
- +1.1s → +45.1s: semantic embed, 3,340 chunks, 53 batches, 44.0s, 75 chunks/s.
- +46.4s: semantic ready.
- +90.2s: Tier-2 `configure_warm` scheduled; `dead_code` starts.
- +90.3s → +140.6s: Tier-2 callgraph snapshot, 621 files.
- +204.3s: `dead_code` complete.
- +204.6s: `unused_exports` complete.
- +205.3s: `duplicates` complete.
- +289.4s: CPU idle window satisfied; settled.

### nestjs/nest (`c47518f611e6`)

- +1.4s: search index ready.
- +0.7s → +0.9s: semantic collect, 6,972 chunks / 1,704 files.
- +0.9s → +88.5s: semantic embed, 6,972 chunks, 109 batches, 87.5s, 79 chunks/s.
- +89.1s: semantic ready.
- +90.2s: Tier-2 `configure_warm` scheduled; `dead_code` starts.
- +90.3s → +109.0s: Tier-2 callgraph snapshot, 2,020 files.
- +151.3s: `dead_code` complete.
- +151.8s: `unused_exports` complete.
- +152.4s: `duplicates` complete.
- +192.3s: CPU idle window satisfied; settled.

### microsoft/vscode bounded rerun (`4efb1f746ea8`)

- +13.3s: search index ready.
- +1.0s → +50.2s: semantic collect, 150,509 chunks / 11,322 files.
- +90.4s: Tier-2 `configure_warm` scheduled while semantic was still loading.
- +90.5s: `dead_code` starts for 12,860 files.
- +90.9s: Tier-2 callgraph snapshot starts.
- +313s, +637s, +1020s, +1467s, +1786s progress samples: still `tier2:callgraph_snapshot`, CPU roughly 790-1003%, RSS roughly 5.7-6.0 GiB.
- +1801.2s cutoff: timed out; still `tier2:callgraph_snapshot`, CPU 935.8%, RSS 6.02 GiB. Semantic had not emitted a terminal `ready` marker by the 30-minute cutoff.

## Partial data from the aborted overlong VSCode run

Before the re-scope, the same VSCode target (`4efb1f746ea8`, 12,860 AFT source files) was run with a 3-hour cap and blocked too long. The partial data captured before stopping was still useful:

- Search ready: +9.5s.
- Semantic ready: +2707.6s (~45.1m), 150,509 entries. Embed phase: 150,509 chunks, 2,352 batches, 2,659.8s, ~56 chunks/s.
- Tier-2 callgraph snapshot: +90.7s → +3345.7s, 12,860 files, 3,255.1s (~54.3m), 46,621 exported symbols, 1,322,941 outbound calls.
- The run still did not settle before the 3-hour cap; peak RSS observed was ~16.0 GiB while Tier-2 remained active.

That overlong run is why the benchmark harness/reporting was re-scoped to 30-minute bounded runs with incremental progress output.

## Verdict

The prime suspect is confirmed for the measured regression path.

At the time of this run, `crates/aft/src/inspect/manager.rs::build_tier2_callgraph_snapshot` built a legacy callgraph snapshot over every project file for Tier-2 `dead_code`. It bypassed the old interactive callgraph cap. The instrumentation showed this phase started at the Tier-2 warm scan and dominated CPU:

- On small repos it is already visible (11-50s), while `unused_exports` and `duplicates` stay below 1s.
- On VSCode-sized repos it continues past the 30-minute cap, pegging ~8-10 cores and reaching ~12 GiB RSS in the bounded run.
- In the aborted overlong run, the snapshot alone took ~54 minutes for 12,860 source files, and the overall Tier-2 scan still did not settle before 3 hours.

Semantic fastembed is also CPU-heavy during embedding (roughly 9-18 cores at peak in these Docker samples), but on the settled repos it terminates in 37-89s. The non-settling behavior and long post-open CPU burn are driven by Tier-2 `dead_code`, specifically the uncapped callgraph snapshot/reparse path.

No 10-minute ceiling re-scan was observed in the settled runs; they settled before 10 minutes. VSCode never completed the first Tier-2 scan, so the ceiling re-scan was not reached.
