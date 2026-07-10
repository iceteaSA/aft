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
pub struct ProcessMemorySnapshot {
    pub rss_status: &'static str,
    pub rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_not_estimated: Option<&'static str>,
    pub total_attributed_bytes: u64,
    pub unattributed_bytes: Option<i64>,
    pub root_count: usize,
    pub busy_subsystems: usize,
    pub not_estimated_subsystems: usize,
}

impl ProcessMemorySnapshot {
    pub fn from_roots(roots: &BTreeMap<String, RootMemorySnapshot>) -> Self {
        let total_attributed_bytes = roots
            .values()
            .map(|root| root.attributed_bytes)
            .fold(0u64, u64::saturating_add);
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
    pub process: ProcessMemorySnapshot,
}

impl MemorySnapshot {
    pub fn new(roots_status: &'static str, roots: BTreeMap<String, RootMemorySnapshot>) -> Self {
        let process = ProcessMemorySnapshot::from_roots(&roots);
        Self {
            roots_status,
            roots,
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
}
