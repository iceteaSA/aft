# Grep/glob indexed latency audit

## Reproduce

Build an optimized binary and run the serial harness:

```sh
cargo build -p agent-file-tools --profile stage
python3 benchmarks/grep-glob-vs-rg.py \
  --aft before=/path/to/baseline/aft \
  --aft after=target/stage/aft \
  --corpus aft=/path/to/aft \
  --corpus openclaw=/path/to/openclaw \
  --output /tmp/grep-glob-ab.json
```

The harness sends AFT requests over one persistent NDJSON process per corpus and
binary, warms each query, then measures 20 serial samples. The ripgrep comparison
starts the `rg` CLI for each sample, as requested. It covers 20 grep queries split
between common and rare identifiers, regexes with literal cores, path-scoped
queries, and case-insensitive queries. Glob is compared with `rg --files -g` over
extension, directory, and rare patterns.

The harness refuses to run when one-minute load divided by logical CPU count is
greater than 0.75. Raising `--max-load-per-cpu` is useful only for diagnosis; such
results are not acceptance numbers.

## Finding and fix

The index was already used by both commands. Glob did not walk for result
discovery on a ready index. However, both command handlers independently called
an ignore-aware filesystem walk to populate `no_files_matched_scope` before using
the index. That rebuilt walk state and touched the filesystem on every warm query.
The indexed result path therefore paid a walk even though its actual lookup was
in memory.

The fix derives scope presence from the same immutable index snapshot used by the
query. Fallback and external paths retain the ignore-aware walk. The command glob
path also skips the index API's mtime sort because command output is immediately
re-sorted lexically, and uses linear selection before sorting its 100-result
prefix. This preserves the exact deterministic result set without sorting every
match twice. No freshness, writer-lease, cache lock, read-marker, or index
verification runs on the ready query path; watcher publication remains responsible
for snapshot freshness.

## Phase breakdown

The following diagnostic A/B used identical debug binaries and a serial harness.
The machine was contended (load 33.64 on 18 logical CPUs), so only the attribution
is meaningful, not the absolute latency. Values are medians across the query set.

| grep phase | before (ms) | after (ms) | conclusion |
|---|---:|---:|---|
| snapshot acquire (lock + clone) | 0.010 | 0.005 | negligible |
| trigram lookup | 0.868 | 4.094 | subdominant; noisy under contention |
| pread verify/match | 28.433 | 20.109 | intrinsic work scales with candidate bytes |
| post-filter/format | 4.427 | 4.438 | unchanged |
| separate scope walk | 14.697 | 0.000 | removable per-call cost |

| glob phase | before (ms) | after (ms) | conclusion |
|---|---:|---:|---|
| result-discovery filesystem walk | 0.000 | 0.000 | ready path already used index |
| entries inspected in index | 1,964 | 1,964 | linear in indexed file list |
| separate scope walk | 7.788 | 0.000 | removable per-call cost |

`pread_verify` includes reading candidate files and verifying the compiled matcher;
it is expected to dominate common-trigram and broad-regex queries. Candidate
postings are already sorted by posting-list rarity before intersection. Capping
candidate verification would change completeness/truncation behavior, so it was
not changed.

## Quiet-box A/B

The final run started at load 12.44 on 18 logical CPUs (0.69 runnable tasks per
CPU, below the harness's 0.75 refusal threshold). Both corpora were run serially
with 3 warmups and 20 measured requests per query. The large corpus was OpenClaw
at 21,052 indexed entries; the AFT corpus had 1,969 indexed entries.

| corpus / engine | common identifier p50/p90 | rare identifier p50/p90 | regex p50/p90 | path p50/p90 | case-insensitive p50/p90 |
|---|---:|---:|---:|---:|---:|
| AFT repo / before | 6.756 / 9.711 | 3.707 / 7.485 | 7.032 / 14.120 | 5.182 / 7.554 | 4.357 / 7.126 |
| AFT repo / after | **4.835 / 9.946** | **1.643 / 3.938** | **4.673 / 9.886** | **3.744 / 5.605** | **3.568 / 8.282** |
| AFT repo / rg CLI | 106.220 / 158.330 | 114.588 / 173.175 | 103.812 / 145.614 | 88.636 / 136.405 | 113.225 / 165.499 |
| OpenClaw / before | 111.481 / 281.771 | 34.505 / 83.132 | 51.459 / 160.555 | 88.286 / 181.195 | 44.579 / 112.825 |
| OpenClaw / after | **57.031 / 112.056** | **10.086 / 19.796** | **19.248 / 70.620** | **48.186 / 72.483** | **22.404 / 45.686** |
| OpenClaw / rg CLI | 700.490 / 1003.836 | 573.509 / 814.164 | 679.860 / 1103.853 | 392.923 / 957.671 | 623.449 / 867.654 |

| corpus / engine | extension glob p50/p90 | directory glob p50/p90 | rare glob p50/p90 |
|---|---:|---:|---:|
| AFT repo / before | 7.197 / 15.134 | 7.790 / 13.787 | 7.126 / 12.683 |
| AFT repo / after | **3.348 / 4.916** | **3.868 / 5.858** | **3.452 / 5.846** |
| AFT repo / rg CLI | 75.842 / 123.544 | 69.649 / 161.448 | 91.290 / 121.093 |
| OpenClaw / before | 83.595 / 125.408 | 60.129 / 69.751 | 60.010 / 73.696 |
| OpenClaw / after | **58.342 / 143.211** | 70.995 / 94.863 | 71.596 / 104.701 |
| OpenClaw / rg CLI | 116.915 / 151.396 | 111.879 / 135.660 | 126.561 / 185.547 |

The AFT-repo target is met: every identifier class is at or below 5 ms p50,
and all indexed glob classes are below 4 ms p50. Snapshot acquisition is 0.001
ms p50. Trigram lookup, pread verification, and post-filter/format are 0.188,
1.800, and 0.746 ms p50 respectively.

The 21k-file corpus does not meet the 5 ms target. Its median query examines 251
candidate files and verifies about 20.7 MB; broad queries reach 9,378 candidates.
The measured p50 phases are 4.272 ms trigram lookup, 9.220 ms pread verification,
and 2.500 ms post-filter/format. Indexed glob still scans all 21,052 paths, so its
cost is intrinsic to the current file-list design. A secondary path index would
be the next step for very large corpora; adding one was not justified by the AFT
repo target and would expand invalidation semantics. The OpenClaw glob classes did
not improve consistently, so no large-corpus glob win is claimed. Even on this
corpus, grep remains roughly 8-57x faster than the corresponding `rg` CLI classes.
