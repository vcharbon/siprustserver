//! Prometheus exposition of jemalloc's `mallctl` statistics.
//!
//! The runner binaries (`b2bua-runner`, `sip-proxy-runner`) install
//! `tikv_jemallocator::Jemalloc` as their `#[global_allocator]` to bound
//! steady-state RSS — glibc malloc retains freed arena chunks and ratchets RSS
//! under sustained SIP churn (a no-chaos soak measured ~209 MiB/h growth with
//! all logical state flat, → node-cgroup OOM). jemalloc returns dirty/muzzy
//! pages to the OS on a time-based decay; the worker tunes that aggressively via
//! `_RJEM_MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:1000`.
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

#[cfg(not(target_env = "msvc"))]
mod imp {
    use tikv_jemalloc_ctl::{epoch, stats};

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
            push_gauge(&mut s, "jemalloc_allocated_bytes", "Bytes in live application allocations (app demand).", v);
        }
        if let Ok(v) = stats::active::read() {
            push_gauge(&mut s, "jemalloc_active_bytes", "Bytes in active pages backing allocations.", v);
        }
        if let Ok(v) = stats::resident::read() {
            push_gauge(&mut s, "jemalloc_resident_bytes", "Physical resident bytes (RSS-equivalent). Watch resident-allocated for retention.", v);
        }
        if let Ok(v) = stats::mapped::read() {
            push_gauge(&mut s, "jemalloc_mapped_bytes", "Bytes mapped into the process address space.", v);
        }
        if let Ok(v) = stats::retained::read() {
            push_gauge(&mut s, "jemalloc_retained_bytes", "Virtual bytes retained (unmapped, kept for fast reuse) — not resident.", v);
        }
        if let Ok(v) = stats::metadata::read() {
            push_gauge(&mut s, "jemalloc_metadata_bytes", "Bytes of jemalloc internal metadata.", v);
        }

        // --- size-class split: SIP fragments across many sizes -----------------
        // SIP messages/headers/dialog state span a wide range of sizes, so they
        // land in many small bins + the large class. Splitting `allocated` by
        // class shows WHERE live bytes accumulate; with `active`/`allocated` it
        // localises internal (slab) fragmentation — `active - allocated` is the
        // padding wasted inside half-full slabs, the classic variable-size cost.
        if let Some(v) = raw::<usize>(b"stats.arenas.4096.small.allocated\0") {
            push_gauge(&mut s, "jemalloc_small_allocated_bytes", "Live bytes in small size classes (most SIP allocations).", v);
        }
        if let Some(v) = raw::<usize>(b"stats.arenas.4096.large.allocated\0") {
            push_gauge(&mut s, "jemalloc_large_allocated_bytes", "Live bytes in the large size class (big bodies/buffers).", v);
        }
        // Net live small objects = nmalloc - ndalloc. A monotonic climb while
        // active_calls is flat is a per-size-class retention/leak fingerprint.
        if let Some(v) = raw::<u64>(b"stats.arenas.4096.small.nmalloc\0") {
            push_counter(&mut s, "jemalloc_small_nmalloc_total", "Cumulative small allocations (churn rate; vs ndalloc = net live).", v);
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
            }
        }
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
        let page: usize = raw(b"arenas.page\0").unwrap_or(4096);
        if let Some(p) = raw::<usize>(b"stats.arenas.4096.pdirty\0") {
            push_gauge(&mut s, "jemalloc_dirty_bytes", "Resident bytes freed but not yet purged (awaiting dirty decay).", p * page);
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
            push_counter(&mut s, "jemalloc_dirty_nmadvise_total", "madvise() calls issued purging dirty pages.", v);
        }
        if let Some(v) = raw::<u64>(b"stats.arenas.4096.muzzy_nmadvise\0") {
            push_counter(&mut s, "jemalloc_muzzy_nmadvise_total", "madvise() calls issued purging muzzy pages.", v);
        }
        let _ = ALL; // documents the magic 4096 above; keeps it greppable.

        // --- resolved config: the "did MALLOC_CONF parse?" evidence ------------
        // opt.* reflects what jemalloc adopted at startup (post MALLOC_CONF). A
        // typo'd _RJEM_MALLOC_CONF is silently ignored, so these are the only
        // trustworthy confirmation the 1000ms tuning took.
        if let Some(v) = raw::<isize>(b"opt.dirty_decay_ms\0") {
            push_gauge(&mut s, "jemalloc_opt_dirty_decay_ms", "Resolved dirty_decay_ms (confirm _RJEM_MALLOC_CONF parsed; expect 1000).", v);
        }
        if let Some(v) = raw::<isize>(b"opt.muzzy_decay_ms\0") {
            push_gauge(&mut s, "jemalloc_opt_muzzy_decay_ms", "Resolved muzzy_decay_ms (confirm _RJEM_MALLOC_CONF parsed; expect 1000).", v);
        }
        if let Some(v) = raw::<u32>(b"arenas.narenas\0") {
            push_gauge(&mut s, "jemalloc_arenas", "Number of arenas (parallelism vs per-arena retention trade-off).", v);
        }
        if let Some(v) = raw::<bool>(b"background_thread\0") {
            push_gauge(&mut s, "jemalloc_background_thread", "1 if purging runs on background threads (off the alloc hot path).", v as u8);
        }

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
                    push_gauge(s, "process_virtual_memory_bytes", "Virtual address space (RSS-independent; jemalloc retained shows here).", p * page);
                }
                if let Ok(p) = rss.parse::<usize>() {
                    push_gauge(s, "process_resident_memory_bytes", "OS RSS the cgroup OOMs on — compare to jemalloc_resident_bytes.", p * page);
                }
            }
        }
        // Threads: each carries a stack (tokio worker pool + blocking pool); a
        // climbing count is an off-heap RSS source jemalloc can't see.
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Threads:") {
                    if let Ok(n) = rest.trim().parse::<u64>() {
                        push_gauge(s, "process_threads", "OS thread count (each ~stack of RSS; off-heap growth source).", n);
                    }
                }
            }
        }
    }

    /// Loud one-line startup confirmation in the pod log, so the resolved decay
    /// config is visible without scraping `/metrics`. Pairs with the
    /// `jemalloc_opt_*` gauges for the silent-MALLOC_CONF-failure check.
    pub fn log_config() {
        let dirty: isize = raw(b"opt.dirty_decay_ms\0").unwrap_or(-1);
        let muzzy: isize = raw(b"opt.muzzy_decay_ms\0").unwrap_or(-1);
        let narenas: u32 = raw(b"arenas.narenas\0").unwrap_or(0);
        let bg: bool = raw(b"background_thread\0").unwrap_or(false);
        // prof.active is the live profiling switch; absent (Err) on a build
        // without --enable-prof (the tikv-jemallocator "profiling" feature).
        let prof: i8 = match raw::<bool>(b"prof.active\0") {
            Some(true) => 1,
            Some(false) => 0,
            None => -1,
        };
        eprintln!(
            "jemalloc active: dirty_decay_ms={dirty} muzzy_decay_ms={muzzy} narenas={narenas} background_thread={bg} prof.active={prof} (confirm decay == _RJEM_MALLOC_CONF; prof=-1 means built WITHOUT profiling)"
        );
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
    pub fn dump_profile() -> Result<Vec<u8>, String> {
        Err("jemalloc unavailable on msvc".to_string())
    }
}

pub use imp::{dump_profile, log_config, prometheus_text};
