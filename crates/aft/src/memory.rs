use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// A cold-path estimate of memory AFT can attribute without allocator hooks.
///
/// `estimated_bytes` is `None` when a subsystem is busy or its resident bytes
/// are not cheaply observable. Counts remain available in those cases so the
/// status response never substitutes a fabricated byte estimate.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemoryEstimate {
    pub status: &'static str,
    pub bytes_status: &'static str,
    pub estimated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_estimated: Vec<String>,
    #[serde(flatten)]
    pub counts: BTreeMap<String, u64>,
}

impl MemoryEstimate {
    pub fn estimated(bytes: u64) -> Self {
        Self {
            status: "ready",
            bytes_status: "estimated",
            estimated_bytes: Some(bytes),
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn partial(bytes: u64) -> Self {
        Self {
            status: "ready",
            bytes_status: "partial",
            estimated_bytes: Some(bytes),
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn not_estimated() -> Self {
        Self {
            status: "ready",
            bytes_status: "not_estimated",
            estimated_bytes: None,
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn busy() -> Self {
        Self {
            status: "busy",
            bytes_status: "not_estimated",
            estimated_bytes: None,
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn count(mut self, name: impl Into<String>, value: usize) -> Self {
        self.counts.insert(name.into(), usize_to_u64(value));
        self
    }

    pub fn count_u64(mut self, name: impl Into<String>, value: u64) -> Self {
        self.counts.insert(name.into(), value);
        self
    }

    pub fn gap(mut self, name: impl Into<String>) -> Self {
        self.not_estimated.push(name.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RootMemorySnapshot {
    pub status: &'static str,
    pub attributed_bytes: u64,
    pub semantic: MemoryEstimate,
    pub trigram: MemoryEstimate,
    pub symbols: MemoryEstimate,
    pub callgraph: MemoryEstimate,
    pub inspect: MemoryEstimate,
    pub bash: MemoryEstimate,
    pub lsp: MemoryEstimate,
    pub parser_pool: MemoryEstimate,
}

impl RootMemorySnapshot {
    pub fn new(
        semantic: MemoryEstimate,
        trigram: MemoryEstimate,
        symbols: MemoryEstimate,
        callgraph: MemoryEstimate,
        inspect: MemoryEstimate,
        bash: MemoryEstimate,
        lsp: MemoryEstimate,
        parser_pool: MemoryEstimate,
    ) -> Self {
        let estimates = [
            &semantic,
            &trigram,
            &symbols,
            &callgraph,
            &inspect,
            &bash,
            &lsp,
            &parser_pool,
        ];
        let attributed_bytes = estimates
            .iter()
            .filter_map(|estimate| estimate.estimated_bytes)
            .fold(0u64, u64::saturating_add);
        let status = if estimates.iter().any(|estimate| estimate.status == "busy") {
            "busy"
        } else {
            "ready"
        };
        Self {
            status,
            attributed_bytes,
            semantic,
            trigram,
            symbols,
            callgraph,
            inspect,
            bash,
            lsp,
            parser_pool,
        }
    }

    pub fn busy_subsystem_count(&self) -> usize {
        self.estimates()
            .iter()
            .filter(|estimate| estimate.status == "busy")
            .count()
    }

    pub fn not_estimated_subsystem_count(&self) -> usize {
        self.estimates()
            .iter()
            .filter(|estimate| estimate.estimated_bytes.is_none())
            .count()
    }

    fn estimates(&self) -> [&MemoryEstimate; 8] {
        [
            &self.semantic,
            &self.trigram,
            &self.symbols,
            &self.callgraph,
            &self.inspect,
            &self.bash,
            &self.lsp,
            &self.parser_pool,
        ]
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SqliteMemorySnapshot {
    pub status: &'static str,
    pub memory_used_bytes: u64,
    pub memory_highwater_bytes: u64,
}

impl SqliteMemorySnapshot {
    fn measure() -> Self {
        // SQLite's allocator counters are process-wide and internally synchronized.
        // They intentionally replace per-connection guesses in root estimates.
        let memory_used = unsafe { rusqlite::ffi::sqlite3_memory_used() };
        let memory_highwater = unsafe { rusqlite::ffi::sqlite3_memory_highwater(0) };
        Self {
            status: "measured",
            memory_used_bytes: nonnegative_i64_to_u64(memory_used),
            memory_highwater_bytes: nonnegative_i64_to_u64(memory_highwater),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AllocatorMemorySnapshot {
    pub status: &'static str,
    pub bytes_in_use: Option<u64>,
    pub size_allocated: Option<u64>,
    pub retained_slack_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_estimated: Option<&'static str>,
}

impl AllocatorMemorySnapshot {
    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    fn measured(bytes_in_use: u64, size_allocated: u64) -> Self {
        Self {
            status: "measured",
            bytes_in_use: Some(bytes_in_use),
            size_allocated: Some(size_allocated),
            retained_slack_bytes: Some(size_allocated.saturating_sub(bytes_in_use)),
            not_estimated: None,
        }
    }

    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    fn not_estimated(reason: &'static str) -> Self {
        Self {
            status: "not_estimated_on_this_platform",
            bytes_in_use: None,
            size_allocated: None,
            retained_slack_bytes: None,
            not_estimated: Some(reason),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProcessMemorySnapshot {
    pub rss_status: &'static str,
    pub rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_not_estimated: Option<&'static str>,
    pub sqlite: SqliteMemorySnapshot,
    /// Allocator bytes overlap the attributed subsystem totals and are an
    /// allocation envelope, not another amount to subtract from RSS.
    pub allocator: AllocatorMemorySnapshot,
    pub total_attributed_bytes: u64,
    pub unattributed_bytes: Option<i64>,
    pub root_count: usize,
    pub busy_subsystems: usize,
    pub not_estimated_subsystems: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatorPressureRelief {
    pub bytes_released: u64,
    pub rss_before_bytes: Option<u64>,
    pub rss_after_bytes: Option<u64>,
    pub allocator_before: AllocatorMemorySnapshot,
    pub allocator_after: AllocatorMemorySnapshot,
}

impl ProcessMemorySnapshot {
    pub fn from_roots(
        roots: &BTreeMap<String, RootMemorySnapshot>,
        shared_semantic_bases: &MemoryEstimate,
    ) -> Self {
        let sqlite = SqliteMemorySnapshot::measure();
        let allocator = allocator_memory_snapshot();
        let total_attributed_bytes = roots
            .values()
            .map(|root| root.attributed_bytes)
            .fold(0u64, u64::saturating_add)
            .saturating_add(shared_semantic_bases.estimated_bytes.unwrap_or(0))
            .saturating_add(sqlite.memory_used_bytes);
        let busy_subsystems = roots
            .values()
            .map(RootMemorySnapshot::busy_subsystem_count)
            .sum();
        let not_estimated_subsystems = roots
            .values()
            .map(RootMemorySnapshot::not_estimated_subsystem_count)
            .sum();
        let rss_bytes = process_rss_bytes();
        let unattributed_bytes =
            rss_bytes.map(|rss| signed_difference(rss, total_attributed_bytes));
        Self {
            rss_status: if rss_bytes.is_some() {
                "estimated"
            } else {
                "not_estimated_on_this_platform"
            },
            rss_bytes,
            rss_not_estimated: rss_bytes
                .is_none()
                .then_some("platform_process_rss_unavailable"),
            sqlite,
            allocator,
            total_attributed_bytes,
            unattributed_bytes,
            root_count: roots.len(),
            busy_subsystems,
            not_estimated_subsystems,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemorySnapshot {
    pub roots_status: &'static str,
    pub roots: BTreeMap<String, RootMemorySnapshot>,
    /// Immutable borrowed semantic snapshots, attributed once process-wide.
    pub shared_semantic_bases: MemoryEstimate,
    pub process: ProcessMemorySnapshot,
}

impl MemorySnapshot {
    pub fn new(roots_status: &'static str, roots: BTreeMap<String, RootMemorySnapshot>) -> Self {
        let shared_semantic_bases = crate::semantic_index::shared_semantic_bases_memory();
        let process = ProcessMemorySnapshot::from_roots(&roots, &shared_semantic_bases);
        Self {
            roots_status,
            roots,
            shared_semantic_bases,
            process,
        }
    }
}

pub fn path_bytes(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        usize_to_u64(path.as_os_str().as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        usize_to_u64(path.as_os_str().encode_wide().count())
            .saturating_mul(std::mem::size_of::<u16>() as u64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        usize_to_u64(path.to_string_lossy().len())
    }
}

pub fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

pub fn estimated_json_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => std::mem::size_of::<bool>() as u64,
        Value::Number(_) => std::mem::size_of::<serde_json::Number>() as u64,
        Value::String(value) => usize_to_u64(value.len()),
        Value::Array(values) => values
            .iter()
            .map(estimated_json_bytes)
            .fold(0u64, u64::saturating_add),
        Value::Object(values) => values.iter().fold(0u64, |bytes, (key, value)| {
            bytes
                .saturating_add(usize_to_u64(key.len()))
                .saturating_add(estimated_json_bytes(value))
        }),
    }
}

fn signed_difference(lhs: u64, rhs: u64) -> i64 {
    let difference = i128::from(lhs) - i128::from(rhs);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn nonnegative_i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn allocator_memory_snapshot() -> AllocatorMemorySnapshot {
    let mut statistics = std::mem::MaybeUninit::<libc::malloc_statistics_t>::zeroed();
    unsafe {
        libc::malloc_zone_statistics(libc::malloc_default_zone(), statistics.as_mut_ptr());
    }
    let statistics = unsafe { statistics.assume_init() };
    AllocatorMemorySnapshot::measured(
        usize_to_u64(statistics.size_in_use),
        usize_to_u64(statistics.size_allocated),
    )
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_memory_snapshot() -> AllocatorMemorySnapshot {
    let statistics = unsafe { libc::mallinfo2() };
    let mapped_bytes = statistics.hblkhd as u64;
    let bytes_in_use = (statistics.uordblks as u64).saturating_add(mapped_bytes);
    let size_allocated = (statistics.arena as u64).saturating_add(mapped_bytes);
    AllocatorMemorySnapshot::measured(bytes_in_use, size_allocated)
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
fn allocator_memory_snapshot() -> AllocatorMemorySnapshot {
    AllocatorMemorySnapshot::not_estimated("platform_allocator_statistics_unavailable")
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut libc::malloc_zone_t, goal: usize) -> usize;
}

/// Ask the macOS allocator to return unused pages after a process-wide idle gate.
/// Callers own that gate because allocator pressure relief can add latency.
#[cfg(target_os = "macos")]
pub fn relieve_allocator_pressure() -> AllocatorPressureRelief {
    let rss_before_bytes = process_rss_bytes();
    let allocator_before = allocator_memory_snapshot();
    let bytes_released = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    let allocator_after = allocator_memory_snapshot();
    let rss_after_bytes = process_rss_bytes();
    AllocatorPressureRelief {
        bytes_released: usize_to_u64(bytes_released),
        rss_before_bytes,
        rss_after_bytes,
        allocator_before,
        allocator_after,
    }
}

#[cfg(target_os = "macos")]
fn process_rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>();
    let written = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(size).ok()?,
        )
    };
    if written != i32::try_from(size).ok()? {
        return None;
    }
    Some(unsafe { info.assume_init() }.pti_resident_size)
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    resident_pages.checked_mul(page_size as u64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_preserves_negative_residuals() {
        assert_eq!(signed_difference(5, 8), -3);
    }

    #[test]
    fn json_estimator_scales_with_payload_content() {
        let empty = estimated_json_bytes(&serde_json::json!({}));
        let populated = estimated_json_bytes(&serde_json::json!({"message": "hello"}));
        assert_eq!(empty, 0);
        assert!(populated >= 12);
    }

    #[test]
    fn process_snapshot_exposes_sqlite_and_allocator_sections() {
        let shared = MemoryEstimate::estimated(7);
        let snapshot = ProcessMemorySnapshot::from_roots(&BTreeMap::new(), &shared);
        assert_eq!(snapshot.sqlite.status, "measured");
        assert!(snapshot.sqlite.memory_highwater_bytes >= snapshot.sqlite.memory_used_bytes);
        assert_eq!(
            snapshot.total_attributed_bytes,
            snapshot.sqlite.memory_used_bytes.saturating_add(7)
        );

        let serialized = serde_json::to_value(&snapshot).expect("serialize process memory");
        assert!(serialized["sqlite"]["memory_used_bytes"].is_u64());
        assert!(serialized["allocator"].get("bytes_in_use").is_some());
        assert!(serialized["allocator"].get("size_allocated").is_some());
        assert!(serialized["allocator"]
            .get("retained_slack_bytes")
            .is_some());
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn allocator_snapshot_reports_measured_slack() {
        let allocator = allocator_memory_snapshot();
        assert_eq!(allocator.status, "measured");
        let in_use = allocator.bytes_in_use.expect("allocator bytes in use");
        let allocated = allocator.size_allocated.expect("allocator size allocated");
        assert_eq!(
            allocator.retained_slack_bytes,
            Some(allocated.saturating_sub(in_use))
        );
    }

    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    #[test]
    fn allocator_snapshot_is_honest_when_platform_counters_are_unavailable() {
        let allocator = allocator_memory_snapshot();
        assert_eq!(allocator.status, "not_estimated_on_this_platform");
        assert_eq!(allocator.bytes_in_use, None);
        assert_eq!(allocator.size_allocated, None);
        assert_eq!(allocator.retained_slack_bytes, None);
        assert_eq!(
            allocator.not_estimated,
            Some("platform_allocator_statistics_unavailable")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "bounded live RSS experiment; run explicitly after allocator changes"]
    fn allocator_pressure_relief_warm_then_idle_measurement() {
        let warm_pages = (0..16 * 1024)
            .map(|seed| {
                let mut page = Box::new([0u8; 4096]);
                page[0] = seed as u8;
                page
            })
            .collect::<Vec<_>>();
        std::hint::black_box(&warm_pages);
        drop(warm_pages);

        let relief = relieve_allocator_pressure();
        let sqlite = SqliteMemorySnapshot::measure();
        eprintln!(
            "warm-then-idle pressure relief: rss_before={:?} rss_after={:?} allocator_in_use_before={:?} allocator_in_use_after={:?} allocator_allocated_before={:?} allocator_allocated_after={:?} allocator_slack_before={:?} allocator_slack_after={:?} allocator_reported_released={} sqlite_used={} sqlite_highwater={}",
            relief.rss_before_bytes,
            relief.rss_after_bytes,
            relief.allocator_before.bytes_in_use,
            relief.allocator_after.bytes_in_use,
            relief.allocator_before.size_allocated,
            relief.allocator_after.size_allocated,
            relief.allocator_before.retained_slack_bytes,
            relief.allocator_after.retained_slack_bytes,
            relief.bytes_released,
            sqlite.memory_used_bytes,
            sqlite.memory_highwater_bytes,
        );
        assert_eq!(relief.allocator_before.status, "measured");
        assert_eq!(relief.allocator_after.status, "measured");
    }
}
