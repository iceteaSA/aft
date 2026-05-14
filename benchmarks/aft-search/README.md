# AFT search eval harness

Manual retrieval baseline for the AFT search overhaul PR series. This is **not**
part of `release.yml` and is intentionally run by hand before retrieval-changing
work lands. PR 2 freezes the current semantic-only behavior in `baseline.json`;
PR 3 runs this same harness against hybrid retrieval and compares the result.

The harness is implemented in Python, matching the existing `benchmarks/`
tooling and the council decision (`bg_7ea22ecf`) that selected Python for this
manual eval. The canonical implementation spec is
`.alfonso/plans/aft-search-overhaul-v6.md` §4.3.

## Metrics

- **Per-shape P@5**: for each query shape, the average of a binary per-fixture
  win/loss: `1` when any returned top-5 file is in `expected_top_files`, else
  `0`.
- **Per-shape MRR**: reciprocal rank of the first expected file in the top 5,
  averaged by query shape.
- **p50 / p95 query latency**: wall-clock time from sending `semantic_search` to
  receiving the response, summarized per shape and overall.
- **Embedding-cache hit rate**: currently reported as unavailable (`null`). The
  PR 2 baseline binary has no query-embedding cache counter to scrape; PR 3 can
  populate this once the cache/counter exists in the Rust search path.

## Running

From this directory:

```bash
python3 run.py --binary ../../target/release/aft --project-root ../..
```

Or from the repository root:

```bash
python3 benchmarks/aft-search/run.py \
  --binary target/release/aft \
  --project-root . \
  --out benchmarks/aft-search/baseline.json
```

The runner starts the `aft` binary in stdin/stdout protocol mode, sends
`configure` with `semantic_search=true`, waits for the semantic index to report
`ready`, then evaluates every fixture with `semantic_search(top_k=5)`.

Compare a candidate binary against the committed baseline:

```bash
python3 run.py \
  --mode compare baseline.json \
  --binary ../../target/release/aft \
  --project-root ../..
```

## Updating `baseline.json`

Only update the baseline when intentionally freezing a new release line or a new
retrieval design point:

1. Build the release binary to measure, for example
   `cargo build --release -p agent-file-tools`.
2. Run the harness with `--out baseline.json` from `benchmarks/aft-search/`.
3. Review the stdout report and the JSON metadata (`aft` version, binary path,
   project git rev, fixture count).
4. Re-run to a temporary output and diff it against `baseline.json` to verify
   deterministic retrieval results:

   ```bash
   python3 run.py --binary ../../target/release/aft --project-root ../.. --out /tmp/test-baseline.json
   diff /tmp/test-baseline.json baseline.json
   ```

Latency is measured during every run and printed in the report. To keep the
committed baseline reproducible with plain `diff`, a run that writes to a
different output path reuses the committed baseline's volatile latency and run
metadata fields when the fixture/query results match.

## Fixture scope

`fixtures.json` targets this repository (`opencode-aft`) only, so no external
checkout is required. The set includes the dogfood failure shapes from the plan:
identifier-shaped queries (`useState`, `aft_safety_history`, `subagent_type`), a
generic file-role query for `index.ts`, and an error-message query for process
group termination, plus broader mixed, natural-language, path, and generic-file
coverage.

## Embedding cache deferral

PR 2 does not add the optional in-process query-embedding cache. In the current
code, query embedding is performed in `commands/semantic_search.rs`, while the
optional cache was constrained to stay inside `semantic_index.rs`; exposing a hit
rate would also require response/API plumbing. That crosses the PR 2 harness-only
boundary, so the baseline reports `embedding_cache_hit_rate: null` and leaves the
cache/counter for PR 3.
