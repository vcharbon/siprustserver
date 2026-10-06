//! Prometheus exposition of jemalloc's `mallctl` statistics.
//!
//! The runner binaries (`b2bua-runner`, `sip-proxy-runner`) install
//! `tikv_jemallocator::Jemalloc` as their `#[global_allocator]` to bound
//! steady-state RSS — glibc malloc retains freed arena chunks and ratchets RSS
//! under sustained SIP churn (a no-chaos soak measured ~209 MiB/h growth with
//! all logical state flat, → node-cgroup OOM). jemalloc returns dirty/muzzy
//! pages to the OS on a time-based decay. Linking this crate sets jemalloc's
//! defaults ([`defaults`]: 1 s decay, `thp:never`, four arenas) and
//! `_RJEM_MALLOC_CONF` overrides them key by key. [`footprint`] states what the
//! resolved settings cost on the host and [`log_config`] reports it at startup.
//!
//! The tuning is only observable if we can SEE it. This crate reads jemalloc's own
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
//! This crate never installs the allocator (the binary's `#[global_allocator]`
//! does); it sets jemalloc's defaults and reads its statistics. On a
//! non-jemalloc build it must not be linked — `tikv-jemalloc-ctl` brings its
//! own jemalloc, so depending on it without the matching allocator would link
//! a second, unused jemalloc whose stats are all zero. Gate the dependency on
//! the same `cfg(not(target_env = "msvc"))` as the allocator. On msvc this
//! crate is no-op stubs so callers need no second cfg.

pub mod catalogue;
pub mod defaults;
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

    /// Read a string-valued `mallctl` (`opt.thp`, `opt.malloc_conf.*`);
    /// `None` on any error or a null string.
    ///
    /// SAFETY: every such mallctl yields a `const char *` that is null or
    /// NUL-terminated and lives for the process; null is checked before
    /// `CStr::from_ptr`.
    fn raw_str(name: &[u8]) -> Option<String> {
        #[allow(unsafe_code)] // irreducible mallctl FFI; see SAFETY above
        let text = unsafe {
            let ptr = tikv_jemalloc_ctl::raw::read::<*const std::os::raw::c_char>(name).ok()?;
            if ptr.is_null() {
                return None;
            }
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        };
        Some(text)
    }

    /// A reading, or `NaN` where jemalloc or `/proc` did not answer.
    struct Reading<T>(Option<T>);

    impl<T: std::fmt::Display> std::fmt::Display for Reading<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match &self.0 {
                Some(v) => v.fmt(f),
                None => f.write_str("NaN"),
            }
        }
    }

    // Merged-across-all-arenas stats index (`MALLCTL_ARENAS_ALL`); jemalloc
    // accepts it as the arena number in a `stats.arenas.<i>.*` mallctl name.
    const ALL: &str = "4096";

    /// The jemalloc counters as Prometheus text, every family of
    /// [`crate::catalogue::FAMILIES`]. Empty if jemalloc is not answering
    /// (should never happen in a jemalloc build, but stays harmless).
    /// Job/instance labels from the scrape disambiguate b2bua vs proxy — the
    /// metric names are deliberately unprefixed.
    pub fn prometheus_text() -> String {
        use crate::catalogue as c;
        // jemalloc caches stats; advancing the epoch refreshes the snapshot that
        // every subsequent read below observes. If even this fails, jemalloc is
        // not present/answering — emit nothing.
        if epoch::advance().is_err() {
            return String::new();
        }
        let mut s = String::with_capacity(2048);
        let r = |s: &mut String, f: &metric_catalogue::Family, v: Option<u64>| {
            f.render_value(s, Reading(v));
        };

        // --- footprint: the RSS-bounding evidence -------------------------------
        r(&mut s, &c::ALLOCATED, stats::allocated::read().ok().map(|v| v as u64));
        r(&mut s, &c::ACTIVE, stats::active::read().ok().map(|v| v as u64));
        r(&mut s, &c::RESIDENT, stats::resident::read().ok().map(|v| v as u64));
        r(&mut s, &c::MAPPED, stats::mapped::read().ok().map(|v| v as u64));
        r(&mut s, &c::RETAINED, stats::retained::read().ok().map(|v| v as u64));
        r(&mut s, &c::METADATA, stats::metadata::read().ok().map(|v| v as u64));

        // --- size-class split: SIP fragments across many sizes -----------------
        // SIP messages/headers/dialog state span a wide range of sizes, so they
        // land in many small bins + the large class. Splitting `allocated` by
        // class shows WHERE live bytes accumulate; with `active`/`allocated` it
        // localises internal (slab) fragmentation — `active - allocated` is the
        // padding wasted inside half-full slabs, the classic variable-size cost.
        let small = raw::<usize>(b"stats.arenas.4096.small.allocated\0").map(|v| v as u64);
        r(&mut s, &c::SMALL_ALLOCATED, small);
        let large = raw::<usize>(b"stats.arenas.4096.large.allocated\0").map(|v| v as u64);
        r(&mut s, &c::LARGE_ALLOCATED, large);
        // Net live small objects = nmalloc - ndalloc. A monotonic climb while
        // active_calls is flat is a per-size-class retention/leak fingerprint.
        r(&mut s, &c::SMALL_NMALLOC, raw::<u64>(b"stats.arenas.4096.small.nmalloc\0"));
        r(&mut s, &c::SMALL_NDALLOC, raw::<u64>(b"stats.arenas.4096.small.ndalloc\0"));

        // --- per-size-class live regions: SYMBOL-FREE leak localisation --------
        // `stats.arenas.<ALL>.bins.<j>.curregs` = live small regions in class j
        // (region size `arenas.bin.<j>.size`). The class whose live bytes climb
        // while every APP gauge is flat names the leaking object by its SIZE — no
        // backtrace/symbol resolution (unreliable on an optimised+inlined binary).
        //
        // Beside it, the class's slab pages (`_active_bytes`) and the heap-profile
        // samples outside every slab: jemalloc counts a sampled object in the
        // nmalloc/ndalloc of the bin of its size but in no curregs, so
        // nmalloc−ndalloc−curregs is its live count, in extents of its own.
        let page: usize = raw(b"arenas.page\0").unwrap_or(4096);
        let pad_pages = u64::from(raw::<bool>(b"opt.cache_oblivious\0").unwrap_or(true));
        let mut sampled_extent_bytes = 0u64;
        let mut bins: [Vec<(Vec<String>, u64)>; 5] = Default::default();
        if let Some(nbins) = raw::<u32>(b"arenas.nbins\0") {
            for j in 0..nbins {
                let szname = format!("arenas.bin.{j}.size\0");
                let crname = format!("stats.arenas.4096.bins.{j}.curregs\0");
                let size = raw::<usize>(szname.as_bytes()).unwrap_or(0);
                let curregs = raw::<usize>(crname.as_bytes()).unwrap_or(0);
                if size == 0 {
                    continue;
                }
                let slab_size =
                    raw::<usize>(format!("arenas.bin.{j}.slab_size\0").as_bytes()).unwrap_or(0);
                let curslabs =
                    raw::<usize>(format!("stats.arenas.4096.bins.{j}.curslabs\0").as_bytes())
                        .unwrap_or(0);
                let nonfull =
                    raw::<usize>(format!("stats.arenas.4096.bins.{j}.nonfull_slabs\0").as_bytes())
                        .unwrap_or(0);
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
                let values = [
                    curregs.saturating_mul(size) as u64,
                    curslabs.saturating_mul(slab_size) as u64,
                    nonfull as u64,
                    sampled,
                    bytes,
                ];
                for (rows, v) in bins.iter_mut().zip(values) {
                    rows.push((vec![size.to_string()], v));
                }
            }
        }
        let bin_families = [
            c::BIN_LIVE,
            c::BIN_ACTIVE,
            c::BIN_NONFULL_SLABS,
            c::BIN_SAMPLED_LIVE,
            c::BIN_SAMPLED_BYTES,
        ];
        for (family, rows) in bin_families.iter().zip(bins) {
            family.render_rows(&mut s, rows);
        }
        c::SAMPLED_EXTENT.render_value(&mut s, sampled_extent_bytes);
        // Large (extent) classes: `lextents.<j>.curlextents` × `arenas.lextent.<j>.size`.
        let mut lextents = Vec::new();
        if let Some(nlex) = raw::<u32>(b"arenas.nlextents\0") {
            for j in 0..nlex {
                let szname = format!("arenas.lextent.{j}.size\0");
                let crname = format!("stats.arenas.4096.lextents.{j}.curlextents\0");
                let size = raw::<usize>(szname.as_bytes()).unwrap_or(0);
                let cur = raw::<usize>(crname.as_bytes()).unwrap_or(0);
                if size == 0 || cur == 0 {
                    continue;
                }
                lextents.push((vec![size.to_string()], cur.saturating_mul(size) as u64));
            }
        }
        c::LEXTENT_LIVE.render_rows(&mut s, lextents);

        // --- decay backlog + activity: the CPU-cost evidence --------------------
        let dirty = raw::<usize>(b"stats.arenas.4096.pdirty\0").map(|p| (p * page) as u64);
        r(&mut s, &c::DIRTY, dirty);
        let muzzy = raw::<usize>(b"stats.arenas.4096.pmuzzy\0").map(|p| (p * page) as u64);
        r(&mut s, &c::MUZZY, muzzy);
        // The number of madvise() syscalls issued returning pages to the OS — the
        // direct CPU cost of aggressive decay (purge *sweep* counts, npurges,
        // aren't exposed in the merged-arena view on jemalloc 5.3, so nmadvise is
        // the cost signal). A steady climb here while RSS is flat is the
        // CPU-traded-for-RSS outcome to watch for.
        r(&mut s, &c::DIRTY_NMADVISE, raw::<u64>(b"stats.arenas.4096.dirty_nmadvise\0"));
        r(&mut s, &c::MUZZY_NMADVISE, raw::<u64>(b"stats.arenas.4096.muzzy_nmadvise\0"));
        push_mutex_waits(&mut s);
        let _ = ALL; // documents the magic 4096 above; keeps it greppable.

        // --- resolved config: the "did MALLOC_CONF parse?" evidence ------------
        // opt.* reflects what jemalloc adopted at startup (post MALLOC_CONF). A
        // typo'd _RJEM_MALLOC_CONF is silently ignored, so these are the only
        // trustworthy confirmation the 1000ms tuning took.
        c::OPT_DIRTY_DECAY.render_value(&mut s, Reading(raw::<isize>(b"opt.dirty_decay_ms\0")));
        c::OPT_MUZZY_DECAY.render_value(&mut s, Reading(raw::<isize>(b"opt.muzzy_decay_ms\0")));
        c::ARENAS.render_value(&mut s, Reading(raw::<u32>(b"arenas.narenas\0")));
        c::OPT_NARENAS.render_value(&mut s, Reading(raw::<u32>(b"opt.narenas\0")));
        let background = raw::<bool>(b"background_thread\0").map(u8::from);
        c::BACKGROUND_THREAD.render_value(&mut s, Reading(background));
        // Heap profiling costs a page per sampled small object (see the bin
        // gauges), so the resolved sampling interval is part of the footprint.
        c::PROF_ACTIVE.render_value(&mut s, Reading(raw::<bool>(b"prof.active\0").map(u8::from)));
        c::OPT_LG_PROF_SAMPLE.render_value(&mut s, Reading(lg_prof_sample()));
        // The footprint report: what the resolved settings cost on this host
        // and whether they can hold here (the startup line's numbers, live).
        let report = Report::of(resolved());
        c::PAGE.render_value(&mut s, report.resolved.page);
        c::CACHE_OBLIVIOUS.render_value(&mut s, u8::from(report.resolved.cache_oblivious));
        c::PROF_SAMPLE_OVERHEAD.render_value(&mut s, format!("{:.6}", report.sample_overhead));
        c::PROF_SAMPLE_OVERHEAD_MAX
            .render_value(&mut s, format!("{:.6}", report.sample_overhead_max));
        let host_thp = report.resolved.host_thp.as_deref().unwrap_or("unreadable");
        c::HOST_THP.render_rows(&mut s, [([host_thp], 1u8)]);
        c::FOOTPRINT_HAZARDS.render_value(&mut s, report.hazards.len());

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

    /// jemalloc's own lock contention: acquisitions that waited and the time
    /// they waited, per arena mutex (merged over arenas), for the bin mutexes
    /// summed over size classes, and per global mutex. Fewer arenas trade slab
    /// slack for these waits (ADR-0038).
    fn push_mutex_waits(s: &mut String) {
        use crate::catalogue as c;
        let read = |prefix: &str| {
            let waits = raw::<u64>(format!("{prefix}.num_wait\0").as_bytes())?;
            let ns = raw::<u64>(format!("{prefix}.total_wait_time\0").as_bytes())?;
            Some((waits, ns))
        };
        let bins = raw::<u32>(b"arenas.nbins\0").map(|nbins| {
            (0..nbins)
                .filter_map(|j| read(&format!("stats.arenas.4096.bins.{j}.mutex")))
                .fold((0u64, 0u64), |(w, ns), (a, b)| (w.saturating_add(a), ns.saturating_add(b)))
        });
        let mut rows: Vec<Option<(u64, u64)>> = Vec::new();
        c::MUTEX_WAITS.labels.for_each_series(|series| {
            let mutex = series.labels().next().map_or("", |(_, v)| v);
            rows.push(match (series.block(), mutex) {
                (0, "bins") => bins,
                (0, m) => read(&format!("stats.arenas.4096.mutexes.{m}")),
                (_, m) => read(&format!("stats.mutexes.{m}")),
            });
        });
        let mut at = 0;
        c::MUTEX_WAITS.render(s, |_| {
            at += 1;
            Reading(rows[at - 1].map(|(w, _)| w))
        });
        let mut at = 0;
        c::MUTEX_WAIT_SECONDS.render(s, |_| {
            at += 1;
            Reading(rows[at - 1].map(|(_, ns)| format!("{:.9}", ns as f64 / 1e9)))
        });
    }

    /// OS-level process memory + thread count from `/proc/self`, Linux-only;
    /// `NaN` where unreadable (the runners only deploy on linux). Standard
    /// Prometheus `process_*` names so it slots into existing panels.
    fn push_proc(s: &mut String) {
        use crate::catalogue as c;
        // /proc/self/statm: size resident shared text lib data dt — all in pages.
        let page = 4096u64; // Linux base page; statm is always base-page units.
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        let mut it = statm.split_whitespace().map(|f| f.parse::<u64>().ok().map(|p| p * page));
        let (vsz, rss) = (it.next().flatten(), it.next().flatten());
        c::PROCESS_VIRTUAL_MEMORY.render_value(s, Reading(vsz));
        c::PROCESS_RESIDENT_MEMORY.render_value(s, Reading(rss));
        // Threads: each carries a stack (tokio worker pool + blocking pool); a
        // climbing count is an off-heap RSS source jemalloc can't see.
        let threads = std::fs::read_to_string("/proc/self/status").ok().and_then(|status| {
            status.lines().find_map(|l| l.strip_prefix("Threads:")?.trim().parse::<u64>().ok())
        });
        c::PROCESS_THREADS.render_value(s, Reading(threads));
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
        let small_max = raw::<u32>(b"arenas.nbins\0")
            .and_then(|n| {
                raw::<usize>(format!("arenas.bin.{}.size\0", n.checked_sub(1)?).as_bytes())
            })
            .unwrap_or(0);
        Resolved {
            malloc_conf: raw_str(b"opt.malloc_conf.global_var\0"),
            page: raw(b"arenas.page\0").unwrap_or(4096),
            small_max,
            cache_oblivious: raw(b"opt.cache_oblivious\0").unwrap_or(true),
            prof_active: raw::<bool>(b"prof.active\0"),
            lg_prof_sample: lg_prof_sample().unwrap_or(19) as u32,
            opt_narenas: raw(b"opt.narenas\0").unwrap_or(0),
            narenas: raw(b"arenas.narenas\0").unwrap_or(0),
            thp: raw_str(b"opt.thp\0").unwrap_or_else(|| "?".into()),
            host_thp,
            dirty_decay_ms: raw::<isize>(b"opt.dirty_decay_ms\0").unwrap_or(-1) as i64,
            muzzy_decay_ms: raw::<isize>(b"opt.muzzy_decay_ms\0").unwrap_or(-1) as i64,
            background_thread: raw(b"background_thread\0").unwrap_or(false),
            threads,
        }
    }

    /// The live sampling interval (`prof.lg_sample`, which `prof.reset` can
    /// change) when profiling is built in and on, else the configured one
    /// (`prof.lg_sample` reads 0 with `prof:false`).
    fn lg_prof_sample() -> Option<usize> {
        let live = raw::<bool>(b"opt.prof\0")
            .filter(|on| *on)
            .and_then(|_| raw::<usize>(b"prof.lg_sample\0"));
        live.or_else(|| raw::<usize>(b"opt.lg_prof_sample\0"))
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
            "jemalloc_prof_sample_overhead_max_ratio ",
            "jemalloc_footprint_hazards ",
            "jemalloc_opt_narenas 4\n",
            "jemalloc_host_thp{mode=",
            "jemalloc_mutex_waits_total{mutex=\"bins\",scope=\"arena\"}",
            "jemalloc_mutex_wait_seconds_total{mutex=\"extents_dirty\",scope=\"arena\"}",
            "jemalloc_mutex_waits_total{mutex=\"ctl\",scope=\"global\"}",
        ] {
            assert!(text.contains(name), "missing {name} in:\n{text}");
        }
        // This process samples nothing and runs on the crate's defaults, so no
        // hazard holds here, whatever the host's THP mode.
        assert!(text.contains("\njemalloc_footprint_hazards 0\n"), "{text}");
    }
}
