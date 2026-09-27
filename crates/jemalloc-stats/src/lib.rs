//! Prometheus exposition of jemalloc's `mallctl` statistics.
//!
//! The runner binaries (`b2bua-runner`, `sip-proxy-runner`) install
//! `tikv_jemallocator::Jemalloc` as their `#[global_allocator]` to bound
//! steady-state RSS — glibc malloc retains freed arena chunks and ratchets RSS
//! under sustained SIP churn (a no-chaos soak measured ~209 MiB/h growth with
//! all logical state flat, → node-cgroup OOM). jemalloc returns dirty/muzzy
//! pages to the OS on a time-based decay; the workspace compiles a 1 s decay,
//! `thp:never` and `narenas:1` into jemalloc (`.cargo/config.toml`, ADR-0038)
//! and `_RJEM_MALLOC_CONF` overrides them key by key. [`footprint`] states what the
//! resolved settings cost on the host and [`log_config`] reports it at startup.
//!
//! That fix is only observable if we can SEE it. This crate reads jemalloc's own
//! counters and renders them as Prometheus text appended to each runner's
//! existing `/metrics`. It lets a soak distinguish the three outcomes that "RSS
//! looks flat" alone cannot:
//!
//! 1. **Is RSS actually bounded?** `jemalloc_resident_bytes` is the physical
//!    footprint; `jemalloc_allocated_bytes` is live app demand. Their *gap* is
//!    retention/fragmentation — with glibc it ratcheted; here it should stay
//!    bounded. Watch `resident - allocated`.
//! 2. **Did CPU trade places with RSS?** Aggressive decay costs `madvise`
//!    syscalls + re-faults. `jemalloc_dirty_nmadvise_total` / `_muzzy_nmadvise_total`
//!    count those syscalls (the purge cost), and `jemalloc_dirty_bytes` /
//!    `_muzzy_bytes` show the live backlog the decay is chewing through.
//! 3. **Did the decay config even parse?** A typo in `_RJEM_MALLOC_CONF` is
//!    IGNORED SILENTLY by jemalloc (you fall back to defaults). The resolved
//!    `jemalloc_opt_dirty_decay_ms` / `_muzzy_decay_ms` gauges read back what
//!    jemalloc actually adopted — assert they equal 1000, don't infer it from
//!    the RSS curve.
//!
//! Read-only: this crate NEVER sets the allocator (the binary's
//! `#[global_allocator]` does). On a non-jemalloc build it must not be linked —
//! `tikv-jemalloc-ctl` brings its own jemalloc, so depending on it without the
//! matching allocator would link a second, unused jemalloc whose stats are all
//! zero. Gate the dependency on the same `cfg(not(target_env = "msvc"))` as the
//! allocator. On msvc this crate is no-op stubs so callers need no second cfg.

pub mod footprint;

#[cfg(not(target_env = "msvc"))]
mod imp {
    use tikv_jemalloc_ctl::{epoch, stats};

    use crate::footprint::{host_thp_mode, Report, Resolved};

    /// Read a `mallctl` value by name, tolerating any error (feature-off build,
    /// unknown key on an older jemalloc, size mismatch) by yielding `None` so the
    /// metric is simply omitted rather than poisoning the whole exposition.
    ///
    /// SAFETY: `tikv_jemalloc_ctl::raw::read` is `unsafe` because it transmutes
    /// the `mallctl` byte buffer into `T`; we only ever call it with the correct
    /// width for each documented key (`size_t`→`usize`, `ssize_t`→`isize`,
    /// `unsigned`→`u32`, `uint64_t`→`u64`, `bool`→`bool`). `name` is a
    /// NUL-terminated byte string as the C API requires.
    fn raw<T: Copy>(name: &[u8]) -> Option<T> {
        // Wrong width → jemalloc returns the real length and the crate errors;
        // we map that (and any other error) to None.
        #[allow(unsafe_code)] // irreducible mallctl FFI; see SAFETY above
        unsafe {
            tikv_jemalloc_ctl::raw::read::<T>(name).ok()
        }
    }

    /// Read a string-valued `mallctl` (`opt.thp`); `None` on any error.
    ///
    /// SAFETY: `read_str` transmutes the returned pointer to a NUL-terminated
    /// C string, which is what every `const char *` mallctl yields.
    fn raw_str(name: &[u8]) -> Option<String> {
        #[allow(unsafe_code)] // irreducible mallctl FFI; see SAFETY above
        let bytes = unsafe { tikv_jemalloc_ctl::raw::read_str(name).ok()? };
        // The slice carries the C string's NUL terminator.
        Some(String::from_utf8_lossy(bytes).trim_end_matches('\0').to_owned())
    }

    fn push_gauge(s: &mut String, name: &str, help: &str, v: impl std::fmt::Display) {
        s.push_str("# HELP ");
        s.push_str(name);
        s.push(' ');
        s.push_str(help);
        s.push_str("\n# TYPE ");
        s.push_str(name);
        s.push_str(" gauge\n");
        s.push_str(name);
        s.push(' ');
        s.push_str(&v.to_string());
        s.push('\n');
    }

    fn push_counter(s: &mut String, name: &str, help: &str, v: impl std::fmt::Display) {
        s.push_str("# HELP ");
        s.push_str(name);
        s.push(' ');
        s.push_str(help);
        s.push_str("\n# TYPE ");
        s.push_str(name);
        s.push_str(" counter\n");
        s.push_str(name);
        s.push(' ');
        s.push_str(&v.to_string());
        s.push('\n');
    }

    // Merged-across-all-arenas stats index (`MALLCTL_ARENAS_ALL`); jemalloc
    // accepts it as the arena number in a `stats.arenas.<i>.*` mallctl name.
    const ALL: &str = "4096";

    /// The jemalloc counters as Prometheus text. Empty string if jemalloc is not
    /// answering (should never happen in a jemalloc build, but stays harmless).
    /// Job/instance labels from the scrape disambiguate b2bua vs proxy — the
    /// metric names are deliberately unprefixed.
    pub fn prometheus_text() -> String {
        // jemalloc caches stats; advancing the epoch refreshes the snapshot that
        // every subsequent read below observes. If even this fails, jemalloc is
        // not present/answering — emit nothing.
        if epoch::advance().is_err() {
            return String::new();
        }
        let mut s = String::with_capacity(2048);

        // --- footprint: the RSS-bounding evidence -------------------------------
        if let Ok(v) = stats::allocated::read() {
            push_gauge(
                &mut s,
                "jemalloc_allocated_bytes",
                "Bytes in live application allocations (app demand).",
                v,
            );
        }
        if let Ok(v) = stats::active::read() {
            push_gauge(
                &mut s,
                "jemalloc_active_bytes",
                "Bytes in active pages backing allocations.",
                v,
            );
        }
        if let Ok(v) = stats::resident::read() {
            push_gauge(
                &mut s,
                "jemalloc_resident_bytes",
                "Physical resident bytes (RSS-equivalent). Watch resident-allocated for retention.",
                v,
            );
        }
        if let Ok(v) = stats::mapped::read() {
            push_gauge(
                &mut s,
                "jemalloc_mapped_bytes",
                "Bytes mapped into the process address space.",
                v,
            );
        }
        if let Ok(v) = stats::retained::read() {
            push_gauge(
                &mut s,
                "jemalloc_retained_bytes",
                "Virtual bytes retained (unmapped, kept for fast reuse) — not resident.",
                v,
            );
        }
        if let Ok(v) = stats::metadata::read() {
            push_gauge(
                &mut s,
                "jemalloc_metadata_bytes",
                "Bytes of jemalloc internal metadata.",
                v,
            );
        }

        // --- size-class split: SIP fragments across many sizes -----------------
        // SIP messages/headers/dialog state span a wide range of sizes, so they
        // land in many small bins + the large class. Splitting `allocated` by
        // class shows WHERE live bytes accumulate; with `active`/`allocated` it
        // localises internal (slab) fragmentation — `active - allocated` is the
        // padding wasted inside half-full slabs, the classic variable-size cost.
        if let Some(v) = raw::<usize>(b"stats.arenas.4096.small.allocated\0") {
            push_gauge(
                &mut s,
                "jemalloc_small_allocated_bytes",
                "Live bytes in small size classes (most SIP allocations).",
                v,
            );
        }
        if let Some(v) = raw::<usize>(b"stats.arenas.4096.large.allocated\0") {
            push_gauge(
                &mut s,
                "jemalloc_large_allocated_bytes",
                "Live bytes in the large size class (big bodies/buffers).",
                v,
            );
        }
        // Net live small objects = nmalloc - ndalloc. A monotonic climb while
        // active_calls is flat is a per-size-class retention/leak fingerprint.
        if let Some(v) = raw::<u64>(b"stats.arenas.4096.small.nmalloc\0") {
            push_counter(
                &mut s,
                "jemalloc_small_nmalloc_total",
                "Cumulative small allocations (churn rate; vs ndalloc = net live).",
                v,
            );
        }
        if let Some(v) = raw::<u64>(b"stats.arenas.4096.small.ndalloc\0") {
            push_counter(&mut s, "jemalloc_small_ndalloc_total", "Cumulative small frees.", v);
        }

        // --- per-size-class live regions: SYMBOL-FREE leak localisation --------
        // `stats.arenas.<ALL>.bins.<j>.curregs` = live small regions in class j
        // (region size `arenas.bin.<j>.size`). The class whose live bytes climb
        // while every APP gauge is flat names the leaking object by its SIZE — no
        // backtrace/symbol resolution (unreliable on an optimised+inlined binary).
        // Emitted as `jemalloc_bin_live_bytes{size="N"}` (curregs×size).
        //
        // Beside it, what the class costs in pages: `jemalloc_bin_active_bytes`
        // is curslabs×slab_size, so active/live per class is that class's slab
        // utilisation. A heap-profiling sample of a small object lives in its
        // own page-rounded extent, outside every slab, plus the pad page of a
        // cache-oblivious large extent: jemalloc counts it in the nmalloc and
        // ndalloc of the bin of its ROUNDED size but in no curregs, so it is
        // absent from `allocated` and present in `active`.
        // `jemalloc_bin_sampled_live{size="4096"}` is that count for every
        // sampled object up to a page, nmalloc−ndalloc−curregs, and
        // `_sampled_bytes` the pages it holds.
        let page: usize = raw(b"arenas.page\0").unwrap_or(4096);
        let pad_pages = u64::from(raw::<bool>(b"opt.cache_oblivious\0").unwrap_or(true));
        let mut sampled_extent_bytes = 0u64;
        if let Some(nbins) = raw::<u32>(b"arenas.nbins\0") {
            for j in 0..nbins {
                let szname = format!("arenas.bin.{j}.size\0");
                let crname = format!("stats.arenas.4096.bins.{j}.curregs\0");
                let size = raw::<usize>(szname.as_bytes()).unwrap_or(0);
                let curregs = raw::<usize>(crname.as_bytes()).unwrap_or(0);
                if size == 0 {
                    continue;
                }
                s.push_str(&format!(
                    "jemalloc_bin_live_bytes{{size=\"{size}\"}} {}\n",
                    curregs.saturating_mul(size)
                ));
                let slab_size =
                    raw::<usize>(format!("arenas.bin.{j}.slab_size\0").as_bytes()).unwrap_or(0);
                let curslabs =
                    raw::<usize>(format!("stats.arenas.4096.bins.{j}.curslabs\0").as_bytes())
                        .unwrap_or(0);
                let nonfull =
                    raw::<usize>(format!("stats.arenas.4096.bins.{j}.nonfull_slabs\0").as_bytes())
                        .unwrap_or(0);
                s.push_str(&format!(
                    "jemalloc_bin_active_bytes{{size=\"{size}\"}} {}\n",
                    curslabs.saturating_mul(slab_size)
                ));
                s.push_str(&format!("jemalloc_bin_nonfull_slabs{{size=\"{size}\"}} {nonfull}\n"));
                let nmalloc =
                    raw::<u64>(format!("stats.arenas.4096.bins.{j}.nmalloc\0").as_bytes())
                        .unwrap_or(0);
                let ndalloc =
                    raw::<u64>(format!("stats.arenas.4096.bins.{j}.ndalloc\0").as_bytes())
                        .unwrap_or(0);
                let sampled = nmalloc.saturating_sub(ndalloc).saturating_sub(curregs as u64);
                let pages = size.div_ceil(page) as u64 + pad_pages;
                let bytes = sampled.saturating_mul(pages * page as u64);
                sampled_extent_bytes = sampled_extent_bytes.saturating_add(bytes);
                s.push_str(&format!("jemalloc_bin_sampled_live{{size=\"{size}\"}} {sampled}\n"));
                s.push_str(&format!("jemalloc_bin_sampled_bytes{{size=\"{size}\"}} {bytes}\n"));
            }
        }
        push_gauge(
            &mut s,
            "jemalloc_sampled_extent_bytes",
            "Active bytes held by heap-profile samples of small objects (outside allocated).",
            sampled_extent_bytes,
        );
        // Large (extent) classes: `lextents.<j>.curlextents` × `arenas.lextent.<j>.size`.
        if let Some(nlex) = raw::<u32>(b"arenas.nlextents\0") {
            for j in 0..nlex {
                let szname = format!("arenas.lextent.{j}.size\0");
                let crname = format!("stats.arenas.4096.lextents.{j}.curlextents\0");
                let size = raw::<usize>(szname.as_bytes()).unwrap_or(0);
                let cur = raw::<usize>(crname.as_bytes()).unwrap_or(0);
                if size == 0 || cur == 0 {
                    continue;
                }
                s.push_str(&format!(
                    "jemalloc_lextent_live_bytes{{size=\"{size}\"}} {}\n",
                    cur.saturating_mul(size)
                ));
            }
        }

        // --- decay backlog + activity: the CPU-cost evidence --------------------
        if let Some(p) = raw::<usize>(b"stats.arenas.4096.pdirty\0") {
            push_gauge(
                &mut s,
                "jemalloc_dirty_bytes",
                "Resident bytes freed but not yet purged (awaiting dirty decay).",
                p * page,
            );
        }
        if let Some(p) = raw::<usize>(b"stats.arenas.4096.pmuzzy\0") {
            push_gauge(&mut s, "jemalloc_muzzy_bytes", "Bytes madvise(FREE)'d, reclaimable by the OS under pressure (awaiting muzzy decay).", p * page);
        }
        // The number of madvise() syscalls issued returning pages to the OS — the
        // direct CPU cost of aggressive decay (purge *sweep* counts, npurges,
        // aren't exposed in the merged-arena view on jemalloc 5.3, so nmadvise is
        // the cost signal). A steady climb here while RSS is flat is the
        // CPU-traded-for-RSS outcome to watch for.
        if let Some(v) = raw::<u64>(b"stats.arenas.4096.dirty_nmadvise\0") {
            push_counter(
                &mut s,
                "jemalloc_dirty_nmadvise_total",
                "madvise() calls issued purging dirty pages.",
                v,
            );
        }
        if let Some(v) = raw::<u64>(b"stats.arenas.4096.muzzy_nmadvise\0") {
            push_counter(
                &mut s,
                "jemalloc_muzzy_nmadvise_total",
                "madvise() calls issued purging muzzy pages.",
                v,
            );
        }
        let _ = ALL; // documents the magic 4096 above; keeps it greppable.

        // --- resolved config: the "did MALLOC_CONF parse?" evidence ------------
        // opt.* reflects what jemalloc adopted at startup (post MALLOC_CONF). A
        // typo'd _RJEM_MALLOC_CONF is silently ignored, so these are the only
        // trustworthy confirmation the 1000ms tuning took.
        if let Some(v) = raw::<isize>(b"opt.dirty_decay_ms\0") {
            push_gauge(
                &mut s,
                "jemalloc_opt_dirty_decay_ms",
                "Resolved dirty_decay_ms (confirm _RJEM_MALLOC_CONF parsed; expect 1000).",
                v,
            );
        }
        if let Some(v) = raw::<isize>(b"opt.muzzy_decay_ms\0") {
            push_gauge(
                &mut s,
                "jemalloc_opt_muzzy_decay_ms",
                "Resolved muzzy_decay_ms (confirm _RJEM_MALLOC_CONF parsed; expect 1000).",
                v,
            );
        }
        if let Some(v) = raw::<u32>(b"arenas.narenas\0") {
            push_gauge(
                &mut s,
                "jemalloc_arenas",
                "Number of arenas (parallelism vs per-arena retention trade-off).",
                v,
            );
        }
        if let Some(v) = raw::<bool>(b"background_thread\0") {
            push_gauge(
                &mut s,
                "jemalloc_background_thread",
                "1 if purging runs on background threads (off the alloc hot path).",
                v as u8,
            );
        }
        // Heap profiling costs a page per sampled small object (see the bin
        // gauges), so the resolved sampling interval is part of the footprint.
        if let Some(v) = raw::<bool>(b"prof.active\0") {
            push_gauge(
                &mut s,
                "jemalloc_prof_active",
                "1 if heap profiling samples allocations.",
                v as u8,
            );
        }
        if let Some(v) = raw::<usize>(b"opt.lg_prof_sample\0") {
            push_gauge(
                &mut s,
                "jemalloc_opt_lg_prof_sample",
                "Resolved lg2 of the mean bytes between heap-profile samples (jemalloc default 19).",
                v,
            );
        }
        // The footprint report: what the resolved settings cost on this host
        // and whether they can hold here (the startup line's numbers, live).
        let report = Report::of(resolved());
        push_gauge(
            &mut s,
            "jemalloc_page_bytes",
            "Allocator page size, fixed at build.",
            report.resolved.page,
        );
        push_gauge(
            &mut s,
            "jemalloc_cache_oblivious",
            "1 if every large extent (a heap-profile sample included) carries one pad page.",
            u8::from(report.resolved.cache_oblivious),
        );
        push_gauge(
            &mut s,
            "jemalloc_prof_sample_overhead_ratio",
            "Expected sampled-extent bytes per live small byte at the resolved profiling interval (0 = no sampling).",
            format!("{:.6}", report.sample_overhead),
        );
        push_gauge(
            &mut s,
            "jemalloc_footprint_hazards",
            "Resolved settings whose footprint cannot hold on this host (see the startup line).",
            report.hazards.len(),
        );

        // --- OS ground truth: localise the leak ON or OFF the heap --------------
        // jemalloc only accounts for jemalloc-managed pages. The cgroup OOMs on
        // the kernel's RSS, which ALSO includes thread stacks (tokio worker +
        // blocking pool), socket/skb buffers, mmap'd files, and any non-jemalloc
        // C allocation. If process_resident_memory_bytes climbs while
        // jemalloc_resident_bytes is flat, the growth is OFF the heap and NO
        // allocator swap can fix it (look at threads / sockets next). This is the
        // make-or-break signal for "jemalloc didn't help."
        push_proc(&mut s);
        s
    }

    /// OS-level process memory + thread count from `/proc/self`. Linux-only;
    /// silently emits nothing elsewhere (the runners only deploy on linux).
    /// Standard Prometheus `process_*` names so it slots into existing panels.
    fn push_proc(s: &mut String) {
        // /proc/self/statm: size resident shared text lib data dt — all in pages.
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            let mut it = statm.split_whitespace();
            let page = 4096usize; // Linux base page; statm is always base-page units.
            if let (Some(vsz), Some(rss)) = (it.next(), it.next()) {
                if let Ok(p) = vsz.parse::<usize>() {
                    push_gauge(
                        s,
                        "process_virtual_memory_bytes",
                        "Virtual address space (RSS-independent; jemalloc retained shows here).",
                        p * page,
                    );
                }
                if let Ok(p) = rss.parse::<usize>() {
                    push_gauge(
                        s,
                        "process_resident_memory_bytes",
                        "OS RSS the cgroup OOMs on — compare to jemalloc_resident_bytes.",
                        p * page,
                    );
                }
            }
        }
        // Threads: each carries a stack (tokio worker pool + blocking pool); a
        // climbing count is an off-heap RSS source jemalloc can't see.
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Threads:") {
                    if let Ok(n) = rest.trim().parse::<u64>() {
                        push_gauge(
                            s,
                            "process_threads",
                            "OS thread count (each ~stack of RSS; off-heap growth source).",
                            n,
                        );
                    }
                }
            }
        }
    }

    /// Loud one-line startup confirmation in the pod log, so the resolved decay
    /// config is visible without scraping `/metrics`. Pairs with the
    /// `jemalloc_opt_*` gauges for the silent-MALLOC_CONF-failure check.
    pub fn log_config() {
        eprintln!("{}", Report::of(resolved()).line());
    }

    /// The footprint of the resolved configuration on this host; `None` only
    /// on a build without jemalloc.
    pub fn footprint_report() -> Option<Report> {
        Some(Report::of(resolved()))
    }

    /// The resolved settings, from `mallctl`, `/proc/self/status` and the
    /// host's THP switch. `prof.active` is absent on a build without
    /// profiling (the tikv-jemallocator "profiling" feature).
    fn resolved() -> Resolved {
        let threads = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|st| st.lines().find_map(|l| l.strip_prefix("Threads:")?.trim().parse().ok()))
            .unwrap_or(0);
        let host_thp = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .ok()
            .and_then(|e| host_thp_mode(&e));
        Resolved {
            page: raw(b"arenas.page\0").unwrap_or(4096),
            cache_oblivious: raw(b"opt.cache_oblivious\0").unwrap_or(true),
            prof_active: raw::<bool>(b"prof.active\0"),
            lg_prof_sample: raw::<usize>(b"opt.lg_prof_sample\0").unwrap_or(19) as u32,
            narenas: raw(b"arenas.narenas\0").unwrap_or(0),
            thp: raw_str(b"opt.thp\0").unwrap_or_else(|| "?".into()),
            host_thp,
            dirty_decay_ms: raw::<isize>(b"opt.dirty_decay_ms\0").unwrap_or(-1) as i64,
            muzzy_decay_ms: raw::<isize>(b"opt.muzzy_decay_ms\0").unwrap_or(-1) as i64,
            background_thread: raw(b"background_thread\0").unwrap_or(false),
            threads,
        }
    }

    /// `(allocated, active)` bytes from a fresh stats epoch; `None` when
    /// jemalloc is not answering.
    pub fn footprint_bytes() -> Option<(u64, u64)> {
        epoch::advance().ok()?;
        Some((stats::allocated::read().ok()? as u64, stats::active::read().ok()? as u64))
    }

    /// Trigger a jemalloc heap profile dump and return the raw profile bytes
    /// (jeprof/pprof text format). Requires the binary built with the
    /// tikv-jemallocator `profiling` feature AND `_RJEM_MALLOC_CONF=prof:true`
    /// at runtime — otherwise the `prof.dump` mallctl is absent and this returns
    /// `Err`. The profile lists currently-LIVE sampled allocations by call stack,
    /// so a dump taken after the leak has accumulated names every significant
    /// leak source at once (no guessing which one). Served by `/debug/heap`.
    pub fn dump_profile() -> Result<Vec<u8>, String> {
        use std::ffi::CString;
        let dir = "/tmp/jeprof";
        std::fs::create_dir_all(dir).map_err(|e| format!("create {dir}: {e}"))?;
        let path = format!("{dir}/manual.heap");
        let c = CString::new(path.as_str()).map_err(|e| format!("cstring: {e}"))?;
        // mallctl("prof.dump", NULL, NULL, &filename_ptr, sizeof(char*)) — write
        // the filename pointer (NUL-terminated) as the new value. `c` must outlive
        // the call (the pointer borrows it), hence the explicit drop after.
        let ptr: *const std::os::raw::c_char = c.as_ptr();
        #[allow(unsafe_code)] // irreducible mallctl FFI; `c` outlives the call (drop below)
        let res = unsafe {
            tikv_jemalloc_ctl::raw::write::<*const std::os::raw::c_char>(b"prof.dump\0", ptr)
        };
        drop(c);
        res.map_err(|e| {
            format!("prof.dump mallctl failed (built without profiling, or prof:false?): {e}")
        })?;
        std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))
    }
}

#[cfg(target_env = "msvc")]
mod imp {
    /// No jemalloc on msvc — the binary uses the system allocator there.
    pub fn prometheus_text() -> String {
        String::new()
    }
    pub fn log_config() {}
    pub fn footprint_bytes() -> Option<(u64, u64)> {
        None
    }
    /// No allocator to read: the system allocator's footprint is its own.
    pub fn footprint_report() -> Option<crate::footprint::Report> {
        None
    }
    pub fn dump_profile() -> Result<Vec<u8>, String> {
        Err("jemalloc unavailable on msvc".to_string())
    }
}

pub use imp::{dump_profile, footprint_bytes, footprint_report, log_config, prometheus_text};

#[cfg(all(test, not(target_env = "msvc")))]
mod exposition_tests {
    use super::prometheus_text;

    /// What a footprint reading needs beside `allocated`/`active`: the pages
    /// each size class holds, the sampled objects outside every slab, and the
    /// resolved profiling interval with the overhead it implies here.
    #[test]
    fn the_exposition_names_the_footprint_of_each_size_class_and_of_profiling() {
        let text = prometheus_text();
        for name in [
            "jemalloc_bin_live_bytes{size=\"3072\"}",
            "jemalloc_bin_active_bytes{size=\"3072\"}",
            "jemalloc_bin_sampled_live{size=\"4096\"}",
            "jemalloc_sampled_extent_bytes ",
            "jemalloc_page_bytes ",
            "jemalloc_opt_lg_prof_sample ",
            "jemalloc_prof_sample_overhead_ratio ",
            "jemalloc_footprint_hazards ",
        ] {
            assert!(text.contains(name), "missing {name} in:\n{text}");
        }
        // This process samples nothing and the workspace compiles `thp:never`
        // in, so no hazard holds here, whatever the host's THP mode.
        assert!(text.contains("\njemalloc_footprint_hazards 0\n"), "{text}");
    }
}
