//! glibc `malloc_info(3)` capture and parsing, for the fragmentation
//! experiment (cortexkit/aft#205).
//!
//! `mallinfo2` — which `crate::memory` already exposes — reports only
//! process-wide aggregates. It cannot distinguish *many low-occupancy arenas*
//! from *few arenas with deep internal fragmentation*, and those two have
//! different remedies: the first is answered by bounding the arena count, the
//! second is not. `malloc_info` emits per-arena and per-bin detail, which is
//! the discriminator.
//!
//! Test-only. This deliberately does not ship in the product binary.

#![allow(dead_code)]

use std::ffi::CString;

/// One glibc arena, as reported in a `<heap nr="N">` element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArenaStats {
    pub(crate) nr: usize,
    /// `<total type="rest">` — free space held by this arena.
    pub(crate) rest_size: u64,
    /// `<system type="current">` — address space the arena has obtained.
    pub(crate) system_current: u64,
    /// `<total type="fast">` + `<total type="rest">` counts, summed.
    pub(crate) free_chunk_count: u64,
    /// Free bytes per reported bin, in document order. Covers both `<size>`
    /// (the binned free lists) and `<unsorted>` (glibc's unsorted bin, which
    /// is also free space and is frequently where a fragmented heap parks the
    /// bulk of it).
    pub(crate) bin_sizes: Vec<u64>,
    /// Free bytes in the unsorted bin specifically. Broken out because a heap
    /// whose free space is nearly all unsorted has a different shape from one
    /// spread across sized bins.
    pub(crate) unsorted_size: u64,
}

impl ArenaStats {
    /// Fraction of this arena's obtained address space that is free.
    ///
    /// The discriminator the experiment turns on: an arena at ~1.0 is holding
    /// almost nothing live and would be reclaimed by bounding the arena count;
    /// an arena at 0.3-0.7 has live allocations pinning its free space and
    /// would not.
    pub(crate) fn free_fraction(&self) -> f64 {
        if self.system_current == 0 {
            return 0.0;
        }
        self.rest_size as f64 / self.system_current as f64
    }
}

/// A parsed `malloc_info` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MallocInfoSnapshot {
    pub(crate) arenas: Vec<ArenaStats>,
    /// Process-wide `<total type="rest">` from the trailing summary.
    pub(crate) total_rest: u64,
    /// Process-wide `<system type="current">` from the trailing summary.
    pub(crate) total_system: u64,
}

impl MallocInfoSnapshot {
    pub(crate) fn arena_count(&self) -> usize {
        self.arenas.len()
    }

    /// Arenas whose free fraction is at or above `threshold`.
    ///
    /// "Many low-occupancy arenas" is the arena-cap-wins signature; counting
    /// them is how that outcome is recognised without eyeballing the XML.
    pub(crate) fn low_occupancy_arenas(&self, threshold: f64) -> usize {
        self.arenas
            .iter()
            .filter(|arena| arena.free_fraction() >= threshold)
            .count()
    }

    /// Free bytes held by arenas at or above `threshold` free fraction.
    pub(crate) fn low_occupancy_rest(&self, threshold: f64) -> u64 {
        self.arenas
            .iter()
            .filter(|arena| arena.free_fraction() >= threshold)
            .map(|arena| arena.rest_size)
            .sum()
    }

    /// A one-line summary for the experiment report.
    pub(crate) fn summary_line(&self) -> String {
        format!(
            "arenas={} total_rest={} total_system={} low_occupancy(>=0.9)={} low_occupancy_rest={} unsorted={}",
            self.arena_count(),
            self.total_rest,
            self.total_system,
            self.low_occupancy_arenas(0.9),
            self.low_occupancy_rest(0.9),
            self.arenas.iter().map(|a| a.unsorted_size).sum::<u64>(),
        )
    }
}

/// Capture the raw `malloc_info` XML for the current process.
///
/// Returns `None` on any platform or allocator without the symbol — musl and
/// jemalloc/mimalloc do not provide it, and arm C of the experiment runs under
/// a replaced allocator, so absence is an expected answer rather than a bug.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn capture_malloc_info() -> Option<String> {
    let mut buffer: *mut libc::c_char = std::ptr::null_mut();
    let mut size: libc::size_t = 0;

    // SAFETY: open_memstream writes the buffer pointer and length through the
    // two out-params, which live for the whole call. A null return means the
    // stream could not be created and neither out-param was written.
    let stream = unsafe { libc::open_memstream(&mut buffer, &mut size) };
    if stream.is_null() {
        return None;
    }

    // SAFETY: `stream` is a live FILE* from open_memstream. malloc_info writes
    // to it and returns 0 on success; a nonzero return leaves the stream valid
    // but the buffer content unusable, so we still close it below.
    let rc = unsafe { libc::malloc_info(0, stream) };

    // SAFETY: fclose flushes and finalises the memstream, after which `buffer`
    // points at a NUL-terminated allocation of `size` bytes owned by us.
    unsafe { libc::fclose(stream) };

    if rc != 0 || buffer.is_null() {
        if !buffer.is_null() {
            // SAFETY: open_memstream's buffer is allocated with malloc and is
            // the caller's to free even when the content is unusable.
            unsafe { libc::free(buffer.cast()) };
        }
        return None;
    }

    // SAFETY: fclose has finalised the stream, so `buffer` is a valid
    // NUL-terminated C string of `size` bytes.
    let xml = unsafe { std::ffi::CStr::from_ptr(buffer) }
        .to_string_lossy()
        .into_owned();

    // SAFETY: we own the buffer; nothing else holds a pointer to it.
    unsafe { libc::free(buffer.cast()) };

    Some(xml)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn capture_malloc_info() -> Option<String> {
    None
}

/// Capture and parse in one step.
pub(crate) fn snapshot() -> Option<MallocInfoSnapshot> {
    capture_malloc_info().as_deref().map(parse_malloc_info)
}

/// Parse `malloc_info` XML into per-arena statistics.
///
/// Hand-rolled rather than pulling an XML dependency into the test tree: the
/// document is machine-generated by glibc with a fixed shape, and the fields
/// wanted are a small fixed set of attributes. A real parser would be more
/// code and one more dependency for no additional correctness here.
pub(crate) fn parse_malloc_info(xml: &str) -> MallocInfoSnapshot {
    let mut arenas = Vec::new();
    let mut current: Option<ArenaStats> = None;
    let mut in_heap = false;
    let mut total_rest = 0u64;
    let mut total_system = 0u64;

    for line in xml.lines() {
        let line = line.trim();

        if let Some(nr) = attr_usize(line, "<heap nr=") {
            in_heap = true;
            current = Some(ArenaStats {
                nr,
                rest_size: 0,
                system_current: 0,
                free_chunk_count: 0,
                bin_sizes: Vec::new(),
                unsorted_size: 0,
            });
            continue;
        }

        if line.starts_with("</heap>") {
            if let Some(arena) = current.take() {
                arenas.push(arena);
            }
            in_heap = false;
            continue;
        }

        if line.starts_with("<sizes>") || line.starts_with("</sizes>") {
            continue;
        }

        // <size from="17" to="32" total="128" count="4"/> — a binned free list.
        if line.starts_with("<size ") {
            if let (Some(arena), Some(total)) = (current.as_mut(), attr_u64(line, "total=")) {
                arena.bin_sizes.push(total);
            }
            continue;
        }

        // <unsorted from="N" to="N" total="N" count="N"/> — also free space.
        // Missing this undercounts free bytes on every real heap: glibc parks
        // recently-freed chunks here before sorting them into bins, and a
        // fragmented arena often holds most of its free space unsorted.
        if line.starts_with("<unsorted ") {
            if let (Some(arena), Some(total)) = (current.as_mut(), attr_u64(line, "total=")) {
                arena.bin_sizes.push(total);
                arena.unsorted_size = arena.unsorted_size.saturating_add(total);
            }
            continue;
        }

        // <total type="rest" count="N" size="M"/>
        if line.starts_with("<total type=\"rest\"") {
            let size = attr_u64(line, "size=").unwrap_or(0);
            let count = attr_u64(line, "count=").unwrap_or(0);
            match (in_heap, current.as_mut()) {
                (true, Some(arena)) => {
                    arena.rest_size = size;
                    arena.free_chunk_count = arena.free_chunk_count.saturating_add(count);
                }
                // Outside a <heap> block this is the trailing process summary.
                _ => total_rest = size,
            }
            continue;
        }

        if line.starts_with("<total type=\"fast\"") {
            if let (Some(arena), Some(count)) = (current.as_mut(), attr_u64(line, "count=")) {
                arena.free_chunk_count = arena.free_chunk_count.saturating_add(count);
            }
            continue;
        }

        // <system type="current" size="N"/>
        if line.starts_with("<system type=\"current\"") {
            let size = attr_u64(line, "size=").unwrap_or(0);
            match (in_heap, current.as_mut()) {
                (true, Some(arena)) => arena.system_current = size,
                _ => total_system = size,
            }
        }
    }

    // A malformed document that never closes its last <heap> would otherwise
    // silently drop that arena.
    if let Some(arena) = current.take() {
        arenas.push(arena);
    }

    MallocInfoSnapshot {
        arenas,
        total_rest,
        total_system,
    }
}

fn attr_u64(line: &str, key: &str) -> Option<u64> {
    attr_str(line, key)?.parse().ok()
}

fn attr_usize(line: &str, key: &str) -> Option<usize> {
    attr_str(line, key)?.parse().ok()
}

/// Extract `key"value"` from a glibc-emitted attribute list.
fn attr_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(key)? + key.len();
    let rest = line.get(start..)?;
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    rest.get(..end)
}

/// Force a `CString` dependency to stay used on non-glibc builds without
/// cfg-gating the import.
#[allow(dead_code)]
fn _unused_cstring() -> Option<CString> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real glibc 2.44 process (`malloc` 64 x 4096, free 60,
    /// then `malloc_info(0, stdout)`), then extended with a second arena to
    /// cover the multi-arena case. Structure is verbatim glibc, including the
    /// `<unsorted>` element and the `<aspace>` lines the parser ignores.
    const SAMPLE: &str = r#"<malloc version="1">
<heap nr="0">
<sizes>
<size from="17" to="32" total="128" count="4"/>
<size from="33" to="48" total="480" count="10"/>
  <unsorted from="242609" to="242609" total="242609" count="1"/>
</sizes>
<total type="fast" count="14" size="608"/>
<total type="rest" count="3" size="1048576"/>
<system type="current" size="2097152"/>
<system type="max" size="2097152"/>
<aspace type="total" size="2097152"/>
</heap>
<heap nr="1">
<sizes>
</sizes>
<total type="fast" count="0" size="0"/>
<total type="rest" count="1" size="66584576"/>
<system type="current" size="67108864"/>
<system type="max" size="67108864"/>
</heap>
<total type="fast" count="14" size="608"/>
<total type="rest" count="4" size="67633152"/>
<system type="current" size="69206016"/>
<system type="max" size="69206016"/>
</malloc>"#;

    #[test]
    fn parses_per_arena_totals_and_process_summary() {
        let parsed = parse_malloc_info(SAMPLE);

        assert_eq!(parsed.arena_count(), 2);
        assert_eq!(parsed.arenas[0].nr, 0);
        assert_eq!(parsed.arenas[0].rest_size, 1_048_576);
        assert_eq!(parsed.arenas[0].system_current, 2_097_152);
        assert_eq!(parsed.arenas[0].bin_sizes, vec![128, 480, 242_609]);
        assert_eq!(parsed.arenas[0].unsorted_size, 242_609);
        assert_eq!(parsed.arenas[0].free_chunk_count, 17);

        assert_eq!(parsed.arenas[1].nr, 1);
        assert_eq!(parsed.arenas[1].rest_size, 66_584_576);
        assert!(parsed.arenas[1].bin_sizes.is_empty());

        // The trailing summary must not be attributed to the last arena.
        assert_eq!(parsed.total_rest, 67_633_152);
        assert_eq!(parsed.total_system, 69_206_016);
    }

    #[test]
    fn free_fraction_separates_occupied_from_low_occupancy_arenas() {
        let parsed = parse_malloc_info(SAMPLE);

        // Arena 0 holds live allocations: half its space is in use.
        assert!((parsed.arenas[0].free_fraction() - 0.5).abs() < 0.01);
        // Arena 1 is the low-occupancy shape an arena cap would reclaim.
        assert!(parsed.arenas[1].free_fraction() > 0.99);

        assert_eq!(parsed.low_occupancy_arenas(0.9), 1);
        assert_eq!(parsed.low_occupancy_rest(0.9), 66_584_576);
        assert_eq!(parsed.low_occupancy_arenas(0.4), 2);
    }

    /// Verbatim glibc 2.44 output, unmodified. Guards against the parser
    /// drifting from the real document shape as the sample above is edited.
    #[test]
    fn parses_a_verbatim_glibc_document() {
        const REAL: &str = r#"<malloc version="1">
<heap nr="0">
<sizes>
  <unsorted from="242609" to="242609" total="242609" count="1"/>
</sizes>
<total type="rest" count="2" size="253873"/>
<system type="current" size="274432"/>
<system type="max" size="274432"/>
<aspace type="total" size="274432"/>
<aspace type="mprotect" size="274432"/>
</heap>
<total type="rest" count="2" size="253873"/>
<total type="mmap" count="0" size="0"/>
<system type="current" size="274432"/>
<system type="max" size="274432"/>
<aspace type="total" size="274432"/>
<aspace type="mprotect" size="274432"/>
</malloc>"#;

        let parsed = parse_malloc_info(REAL);

        assert_eq!(parsed.arena_count(), 1);
        assert_eq!(parsed.arenas[0].rest_size, 253_873);
        assert_eq!(parsed.arenas[0].system_current, 274_432);
        assert_eq!(parsed.arenas[0].unsorted_size, 242_609);
        // The trailing summary repeats the same numbers here; the parser must
        // still attribute them to the process, not add them to the arena.
        assert_eq!(parsed.total_rest, 253_873);
        assert_eq!(parsed.total_system, 274_432);
    }

    #[test]
    fn unclosed_final_heap_is_not_dropped() {
        let truncated = r#"<malloc version="1">
<heap nr="0">
<total type="rest" count="1" size="4096"/>
<system type="current" size="8192"/>
"#;

        let parsed = parse_malloc_info(truncated);

        assert_eq!(parsed.arena_count(), 1);
        assert_eq!(parsed.arenas[0].rest_size, 4096);
    }

    #[test]
    fn empty_document_parses_to_an_empty_snapshot() {
        let parsed = parse_malloc_info("");

        assert_eq!(parsed.arena_count(), 0);
        assert_eq!(parsed.total_rest, 0);
        assert_eq!(parsed.total_system, 0);
        assert_eq!(parsed.low_occupancy_arenas(0.9), 0);
    }

    /// The binding itself, against the live allocator. Skips rather than fails
    /// where the symbol is absent, since arm C runs under a replaced allocator
    /// and musl builds never have it.
    #[test]
    fn captures_live_malloc_info_from_this_process() {
        let Some(xml) = capture_malloc_info() else {
            eprintln!("malloc_info unavailable on this platform/allocator — skipping");
            return;
        };

        assert!(
            xml.contains("<malloc version="),
            "expected a malloc_info document, got {} bytes starting: {}",
            xml.len(),
            &xml[..xml.len().min(120)]
        );

        let parsed = parse_malloc_info(&xml);
        assert!(
            parsed.arena_count() >= 1,
            "a live process has at least the main arena, got {} in {} bytes",
            parsed.arena_count(),
            xml.len()
        );
        assert!(
            parsed.total_system > 0,
            "a live process has obtained address space, got total_system=0"
        );

        eprintln!("live malloc_info: {}", parsed.summary_line());
    }
}
