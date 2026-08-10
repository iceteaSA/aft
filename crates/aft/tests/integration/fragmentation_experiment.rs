//! Fragmentation experiment driver (cortexkit/aft#205).
//!
//! Answers one question: is aft's retained allocator slack *ordinary
//! fragmentation smeared across per-root artifact lifecycles*, or *many
//! low-occupancy arenas* (which a bounded arena count would reclaim), or *one
//! pathological allocation pattern* (fixable at a site)?
//!
//! Observation on the live daemon could not separate these. `malloc_trim(0)`
//! releases zero against a climbing slack number, and three candidate causes
//! changed at once. This drives deterministic churn instead and reads the
//! per-arena free distribution at both ends.
//!
//! # Arms
//!
//! Selected by `AFT_FRAG_ARM`, one process per arm, because two of them are
//! process-level settings that cannot be switched mid-run:
//!
//! | arm | what it is | how it is configured |
//! |-----|-----------|----------------------|
//! | `0` | scaffolding only, zero workload | `AFT_FRAG_ARM=0` |
//! | `A` | stock allocator, current thread regime | `AFT_FRAG_ARM=A` |
//! | `B` | arena cap (rough sanity check only) | `+ MALLOC_ARENA_MAX=2` |
//! | `D` | stock allocator, wide inspect pool | `+ AFT_INSPECT_POOL_THREADS=88` |
//!
//! Arm C (decay-purging allocator) needs a different `#[global_allocator]` and
//! is therefore a different binary, not an env switch. It is out of scope here.
//!
//! **Arm B is a rough sanity check, not a clean control.** Its production
//! reference was measured on the pre-`8bd35ff4` binary at 211 threads, while
//! these arms run the post-fix regime — different arena regimes, and thread
//! count is itself an input to arena count. A B/A result cannot validate the
//! rig on its own.
//!
//! # Reading the result
//!
//! Registered before running, so the outcome cannot be interpreted after:
//!
//! - free space spread across many bins, arena count low ⇒ ordinary
//!   fragmentation; an arena cap will not help
//! - many arenas at high `free_fraction`, and D materially worse than A ⇒
//!   arena multiplication; `mallopt(M_ARENA_MAX)` is the remedy
//! - free space concentrated in one or few size classes ⇒ a pathological site,
//!   and the dominant class names it (maintainer assigns this a low prior)
//! - arm A shows no slack growth ⇒ the rig does not reproduce; nothing else in
//!   the run is interpretable
//!
//! Arm 0 measures the scaffolding's own allocation delta, which is what turns
//! "the scaffolding is constant across arms" from an assumption into a number.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::malloc_info_probe::{self, MallocInfoSnapshot};

/// Which arm this process is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arm {
    /// Scaffolding only: daemon and runtime up, zero workload.
    Scaffolding,
    /// Stock allocator, whatever thread regime the process was launched with.
    Stock,
}

impl Arm {
    fn from_env() -> Option<Self> {
        match std::env::var("AFT_FRAG_ARM").ok()?.trim() {
            "0" => Some(Self::Scaffolding),
            // A, B and D differ only in process-level configuration the test
            // cannot see or set; from in here they are the same workload.
            "A" | "B" | "D" => Some(Self::Stock),
            _ => None,
        }
    }

    fn label() -> String {
        std::env::var("AFT_FRAG_ARM").unwrap_or_else(|_| "unset".to_string())
    }
}

/// One end of the measurement.
#[derive(Debug)]
struct Marker {
    at: Instant,
    malloc_info: Option<MallocInfoSnapshot>,
    rss_kb: u64,
    swap_kb: u64,
    threads: u64,
    large_anon_regions: usize,
    large_anon_rss_kb: u64,
    large_anon_swap_kb: u64,
}

impl Marker {
    fn take() -> Self {
        let (rss_kb, swap_kb, threads) = read_status();
        let (large_anon_regions, large_anon_rss_kb, large_anon_swap_kb) = read_large_anon_regions();
        Self {
            at: Instant::now(),
            malloc_info: malloc_info_probe::snapshot(),
            rss_kb,
            swap_kb,
            threads,
            large_anon_regions,
            large_anon_rss_kb,
            large_anon_swap_kb,
        }
    }
}

/// Read `VmRSS`, `VmSwap` and `Threads` from `/proc/self/status`.
///
/// Cross-checks `malloc_info` against a measure we already trust: slack that
/// exceeds RSS plus swap cannot be live, which is the arithmetic that refuted
/// the first (leak) reading of this problem.
fn read_status() -> (u64, u64, u64) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (0, 0, 0);
    };
    let field = |name: &str| -> u64 {
        status
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    (field("VmRSS:"), field("VmSwap:"), field("Threads:"))
}

/// Census of large anonymous mappings from `/proc/self/smaps`.
///
/// The production investigation tracked these; keeping the same measure makes
/// the rig's numbers comparable to the live series. Counting map entries alone
/// is misleading — glibc reserves 64 MB per heap eagerly and commits lazily —
/// so RSS and swap are summed alongside the count.
fn read_large_anon_regions() -> (usize, u64, u64) {
    const MIN_REGION_KB: u64 = 32 * 1024;

    let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps") else {
        return (0, 0, 0);
    };

    let mut count = 0usize;
    let mut rss_kb = 0u64;
    let mut swap_kb = 0u64;
    let mut in_large_anon = false;

    for line in smaps.lines() {
        if let Some((range, rest)) = line.split_once(' ') {
            if let Some((start, end)) = range.split_once('-') {
                if let (Ok(start), Ok(end)) =
                    (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
                {
                    // A mapping header. Anonymous rw-p regions only: file-backed
                    // mappings are the disk index, already acquitted upstream.
                    let size_kb = (end - start) / 1024;
                    let anonymous = !rest.contains('/');
                    in_large_anon =
                        anonymous && rest.starts_with("rw-p") && size_kb >= MIN_REGION_KB;
                    if in_large_anon {
                        count += 1;
                    }
                    continue;
                }
            }
        }
        if !in_large_anon {
            continue;
        }
        if let Some(value) = line.strip_prefix("Rss:") {
            rss_kb += parse_kb(value);
        } else if let Some(value) = line.strip_prefix("Swap:") {
            swap_kb += parse_kb(value);
        }
    }

    (count, rss_kb, swap_kb)
}

fn parse_kb(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

/// Render the start/end pair as the report the experiment consumes.
fn render_report(arm: &str, start: &Marker, end: &Marker) -> String {
    let mut out = String::new();
    out.push_str(&format!("# fragmentation experiment — arm {arm}\n\n"));
    out.push_str(ARM_C_ABSENCE_HEADER);
    out.push_str(&format!(
        "duration_s = {:.1}\n",
        end.at.duration_since(start.at).as_secs_f64()
    ));
    out.push_str(&format!(
        "inspect_pool_threads_env = {}\n",
        std::env::var("AFT_INSPECT_POOL_THREADS").unwrap_or_else(|_| "unset".into())
    ));
    out.push_str(&format!(
        "malloc_arena_max_env = {}\n\n",
        std::env::var("MALLOC_ARENA_MAX").unwrap_or_else(|_| "unset".into())
    ));

    out.push_str("## process\n\n");
    out.push_str(&format!(
        "| metric | start | end | delta |\n|---|---|---|---|\n\
         | VmRSS kB | {} | {} | {} |\n\
         | VmSwap kB | {} | {} | {} |\n\
         | Threads | {} | {} | {} |\n\
         | large anon regions | {} | {} | {} |\n\
         | large anon RSS kB | {} | {} | {} |\n\
         | large anon Swap kB | {} | {} | {} |\n\n",
        start.rss_kb,
        end.rss_kb,
        end.rss_kb as i64 - start.rss_kb as i64,
        start.swap_kb,
        end.swap_kb,
        end.swap_kb as i64 - start.swap_kb as i64,
        start.threads,
        end.threads,
        end.threads as i64 - start.threads as i64,
        start.large_anon_regions,
        end.large_anon_regions,
        end.large_anon_regions as i64 - start.large_anon_regions as i64,
        start.large_anon_rss_kb,
        end.large_anon_rss_kb,
        end.large_anon_rss_kb as i64 - start.large_anon_rss_kb as i64,
        start.large_anon_swap_kb,
        end.large_anon_swap_kb,
        end.large_anon_swap_kb as i64 - start.large_anon_swap_kb as i64,
    ));

    out.push_str("## allocator\n\n");
    match (&start.malloc_info, &end.malloc_info) {
        (Some(start_info), Some(end_info)) => {
            out.push_str(&format!("start: {}\n", start_info.summary_line()));
            out.push_str(&format!("end:   {}\n\n", end_info.summary_line()));
            out.push_str(&format!(
                "arena_count_delta = {}\ntotal_rest_delta = {}\ntotal_system_delta = {}\n\n",
                end_info.arena_count() as i64 - start_info.arena_count() as i64,
                end_info.total_rest as i64 - start_info.total_rest as i64,
                end_info.total_system as i64 - start_info.total_system as i64,
            ));
            out.push_str("### per-arena at end\n\n");
            out.push_str("| nr | system kB | rest kB | free_fraction | bins | unsorted kB |\n");
            out.push_str("|---|---|---|---|---|---|\n");
            for arena in &end_info.arenas {
                out.push_str(&format!(
                    "| {} | {} | {} | {:.3} | {} | {} |\n",
                    arena.nr,
                    arena.system_current / 1024,
                    arena.rest_size / 1024,
                    arena.free_fraction(),
                    arena.bin_sizes.len(),
                    arena.unsorted_size / 1024,
                ));
            }
        }
        _ => out.push_str(
            "malloc_info unavailable in this process (non-glibc, or a replaced allocator)\n",
        ),
    }

    out
}

/// Stated in every report HEADER, never a footnote.
///
/// A three-arm report that reads as complete invites the conclusion that the
/// fourth arm was tested and unremarkable. An unstated absent arm collapses
/// into "checked, nothing there" exactly the way an unstated skipped check
/// reads as a passed check.
const ARM_C_ABSENCE_HEADER: &str = concat!(
    "> **Arm C (decay-purging allocator) was NOT run and is NOT represented here.**\n",
    "> It is a `#[global_allocator]` change — a different binary, not an env\n",
    "> switch — so it cannot ride this run. This report therefore CANNOT say\n",
    "> whether decay-purging would help.\n",
    ">\n",
    "> C is built only if BOTH registered conditions hold: (1) the arena cap is\n",
    "> ruled out — D not materially worse than A, and no cluster of arenas at\n",
    "> high free_fraction; AND (2) fragmentation-spread is confirmed — free\n",
    "> space across many bins with no dominant size class, and arm A showing\n",
    "> real slack growth. Either failing routes elsewhere: (1) to\n",
    "> `mallopt(M_ARENA_MAX)`, (2) to a pathological site, or to 'the rig did\n",
    "> not reproduce, stop'.\n\n",
);

fn report_path(arm: &str) -> PathBuf {
    let dir = std::env::var("AFT_FRAG_REPORT_DIR")
        .unwrap_or_else(|_| "/tmp/opencode/aft-frag".to_string());
    let _ = std::fs::create_dir_all(&dir);
    Path::new(&dir).join(format!("arm-{arm}.md"))
}

/// The experiment. Skips unless `AFT_FRAG_ARM` selects an arm, so it stays
/// inert in the normal test suite and only runs when driven deliberately.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "measures allocator behaviour under a release-shaped workload; a debug build's allocation profile is not the one under investigation — run via the experiment runner"
)]
fn fragmentation_arm() {
    let Some(arm) = Arm::from_env() else {
        eprintln!("AFT_FRAG_ARM unset — skipping (set 0, A, B or D to run an arm)");
        return;
    };
    let label = Arm::label();

    if malloc_info_probe::capture_malloc_info().is_none() {
        eprintln!("malloc_info unavailable — arm {label} cannot be measured on this allocator");
        return;
    }

    let start = Marker::take();

    match arm {
        Arm::Scaffolding => {
            // Arm 0 deliberately does no work. It measures what the harness
            // itself allocates over the same wall-clock window, which is the
            // number that decides whether the in-process deltas of the other
            // arms are trustworthy.
            std::thread::sleep(workload_duration());
        }
        Arm::Stock => drive_churn_workload(),
    }

    let end = Marker::take();

    let report = render_report(&label, &start, &end);
    let path = report_path(&label);
    std::fs::write(&path, &report).expect("write experiment report");

    eprintln!("{report}");
    eprintln!("report written to {}", path.display());
}

fn workload_duration() -> Duration {
    Duration::from_secs(
        std::env::var("AFT_FRAG_WORKLOAD_SECS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(120),
    )
}

/// Drive allocation churn of the shape the production hoard tracks.
///
/// Deliberately NOT idle: the live series showed the hoard grows with churn and
/// plateaus across idle hours, so a sleeping process reproduces nothing. The
/// shape that matters is many large, long-lived, differently-sized buffers
/// arriving and departing on independent schedules — which is what per-root
/// artifacts do under bind, watcher churn, and eviction.
fn drive_churn_workload() {
    let deadline = Instant::now() + workload_duration();
    let mut retained: Vec<Vec<u8>> = Vec::new();
    let mut round = 0usize;

    while Instant::now() < deadline {
        round += 1;

        // Sizes spanning KBs to hundreds of MB, mirroring the artifact mix
        // (symbol caches through search-index postings blobs).
        let size = match round % 6 {
            0 => 4 * 1024,
            1 => 64 * 1024,
            2 => 1024 * 1024,
            3 => 8 * 1024 * 1024,
            4 => 32 * 1024 * 1024,
            _ => 512 * 1024,
        };

        let mut buffer = vec![0u8; size];
        // Touch every page: an untouched allocation is reserved, not committed,
        // and reserved space is exactly what misled the first reading of this
        // problem.
        for page in buffer.chunks_mut(4096) {
            page[0] = round as u8;
        }
        retained.push(buffer);

        // Release out of order and on a different cadence from allocation, so
        // free chunks land beneath live ones rather than at the top of a heap.
        // Freeing in LIFO order would let malloc_trim reclaim everything and
        // would not reproduce the pinning.
        if retained.len() > 24 {
            let victim = (round * 7) % retained.len();
            retained.swap_remove(victim);
        }
    }

    // Keep the retained set alive to the end of the measurement window.
    std::hint::black_box(&retained);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_a_live_process() {
        let (rss, _swap, threads) = read_status();
        assert!(rss > 0, "a live process has resident memory");
        assert!(threads >= 1, "a live process has at least one thread");
    }

    #[test]
    fn large_anon_census_is_self_consistent() {
        let (count, rss, swap) = read_large_anon_regions();
        // A test binary may legitimately have zero regions >= 32 MB; what must
        // hold is that bytes are only attributed when regions were counted.
        if count == 0 {
            assert_eq!(rss, 0, "no regions counted but RSS attributed");
            assert_eq!(swap, 0, "no regions counted but swap attributed");
        }
    }

    #[test]
    fn arm_parsing_accepts_the_documented_values_only() {
        // Guards the arm table against a typo silently running the wrong
        // workload — an unrecognised value must skip, never default to Stock.
        assert_eq!(parse_arm("0"), Some(Arm::Scaffolding));
        assert_eq!(parse_arm("A"), Some(Arm::Stock));
        assert_eq!(parse_arm("B"), Some(Arm::Stock));
        assert_eq!(parse_arm("D"), Some(Arm::Stock));
        assert_eq!(parse_arm("a"), None);
        assert_eq!(parse_arm("C"), None);
        assert_eq!(parse_arm(""), None);
    }

    /// Mirror of `Arm::from_env`'s match, so the mapping is testable without
    /// mutating process-global environment in a concurrently-running binary.
    fn parse_arm(raw: &str) -> Option<Arm> {
        match raw.trim() {
            "0" => Some(Arm::Scaffolding),
            "A" | "B" | "D" => Some(Arm::Stock),
            _ => None,
        }
    }

    #[test]
    fn report_renders_both_markers_without_malloc_info() {
        let marker = || Marker {
            at: Instant::now(),
            malloc_info: None,
            rss_kb: 1000,
            swap_kb: 0,
            threads: 4,
            large_anon_regions: 2,
            large_anon_rss_kb: 500,
            large_anon_swap_kb: 0,
        };

        let report = render_report("A", &marker(), &marker());

        assert!(report.contains("# fragmentation experiment — arm A"));
        assert!(report.contains("malloc_info unavailable"));
    }

    /// The absent arm must be impossible to miss. A reader who skims only the
    /// top of the report has to learn that C was not run.
    #[test]
    fn every_report_declares_arm_c_absent_in_its_header() {
        let marker = || Marker {
            at: Instant::now(),
            malloc_info: None,
            rss_kb: 1,
            swap_kb: 0,
            threads: 1,
            large_anon_regions: 0,
            large_anon_rss_kb: 0,
            large_anon_swap_kb: 0,
        };

        let report = render_report("D", &marker(), &marker());

        assert!(report.contains("Arm C (decay-purging allocator) was NOT run"));
        assert!(report.contains("CANNOT say"));
        // Header, not footnote: it must precede the first data section.
        let notice = report
            .find("Arm C (decay-purging allocator) was NOT run")
            .expect("arm C notice present");
        let first_section = report.find("## process").expect("process section present");
        assert!(
            notice < first_section,
            "arm C absence must appear before the first data section"
        );
    }
}
