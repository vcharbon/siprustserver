//! The families of the allocator's exposition, and the process memory the
//! kernel accounts beside it. A reading jemalloc or `/proc` does not answer
//! is rendered `NaN`.

use metric_catalogue::{Dim, Family, Labels};

/// The arena mutexes whose waits are read, merged over arenas; `bins` sums
/// the bin mutexes over size classes.
pub const ARENA_MUTEXES: [&str; 13] = [
    "large",
    "extent_avail",
    "extents_dirty",
    "extents_muzzy",
    "extents_retained",
    "decay_dirty",
    "decay_muzzy",
    "base",
    "tcache_list",
    "hpa_shard",
    "hpa_shard_grow",
    "hpa_sec",
    "bins",
];

/// The global mutexes whose waits are read.
pub const GLOBAL_MUTEXES: [&str; 9] = [
    "background_thread",
    "max_per_bg_thd",
    "ctl",
    "prof",
    "prof_thds_data",
    "prof_dump",
    "prof_recent_alloc",
    "prof_recent_dump",
    "prof_stats",
];

/// Each mutex, under its scope.
const MUTEX: Labels = Labels::Union(&[
    &[Dim::new("mutex", &ARENA_MUTEXES), Dim::new("scope", &["arena"])],
    &[Dim::new("mutex", &GLOBAL_MUTEXES), Dim::new("scope", &["global"])],
]);

/// A size class, by its region size in bytes: the classes the allocator was
/// built with.
const SIZE: Labels = Labels::Product(&[Dim::new("size", &[])]);

macro_rules! gauge {
    ($c:ident, $name:literal, $help:literal) => {
        pub const $c: Family = Family::gauge($name, Labels::None, $help);
    };
}

macro_rules! counter {
    ($c:ident, $name:literal, $help:literal) => {
        pub const $c: Family = Family::counter($name, Labels::None, $help);
    };
}

gauge!(
    ALLOCATED,
    "jemalloc_allocated_bytes",
    "Bytes in live application allocations (app demand)."
);
gauge!(ACTIVE, "jemalloc_active_bytes", "Bytes in active pages backing allocations.");
gauge!(
    RESIDENT,
    "jemalloc_resident_bytes",
    "Physical resident bytes (RSS-equivalent). Watch resident-allocated for retention."
);
gauge!(MAPPED, "jemalloc_mapped_bytes", "Bytes mapped into the process address space.");
gauge!(
    RETAINED,
    "jemalloc_retained_bytes",
    "Virtual bytes retained (unmapped, kept for fast reuse) — not resident."
);
gauge!(METADATA, "jemalloc_metadata_bytes", "Bytes of jemalloc internal metadata.");
gauge!(
    SMALL_ALLOCATED,
    "jemalloc_small_allocated_bytes",
    "Live bytes in small size classes (most SIP allocations)."
);
gauge!(
    LARGE_ALLOCATED,
    "jemalloc_large_allocated_bytes",
    "Live bytes in the large size class (big bodies/buffers)."
);
counter!(
    SMALL_NMALLOC,
    "jemalloc_small_nmalloc_total",
    "Cumulative small allocations (churn rate; vs ndalloc = net live)."
);
counter!(SMALL_NDALLOC, "jemalloc_small_ndalloc_total", "Cumulative small frees.");

pub const BIN_LIVE: Family = Family::gauge(
    "jemalloc_bin_live_bytes",
    SIZE,
    "Live bytes in one small size class (live regions × size): the class whose bytes climb while every app gauge is flat names the leaking object by its size.",
)
.semi_open();

pub const BIN_ACTIVE: Family = Family::gauge(
    "jemalloc_bin_active_bytes",
    SIZE,
    "Slab bytes one small size class holds (slabs × slab size).",
)
.semi_open();

pub const BIN_NONFULL_SLABS: Family = Family::gauge(
    "jemalloc_bin_nonfull_slabs",
    SIZE,
    "Slabs of one small size class with a free region.",
)
.semi_open();

pub const BIN_SAMPLED_LIVE: Family = Family::gauge(
    "jemalloc_bin_sampled_live",
    SIZE,
    "Heap-profile samples of one small size class live outside its slabs (allocations - frees - live regions).",
)
.semi_open();

pub const BIN_SAMPLED_BYTES: Family = Family::gauge(
    "jemalloc_bin_sampled_bytes",
    SIZE,
    "Extent bytes the heap-profile samples of one small size class hold.",
)
.semi_open();

gauge!(
    SAMPLED_EXTENT,
    "jemalloc_sampled_extent_bytes",
    "Active bytes held by heap-profile samples of small objects (outside allocated)."
);

pub const LEXTENT_LIVE: Family = Family::gauge(
    "jemalloc_lextent_live_bytes",
    SIZE,
    "Live bytes in one large size class (live extents × size); a class holding none is absent.",
)
.semi_open();

gauge!(
    DIRTY,
    "jemalloc_dirty_bytes",
    "Resident bytes freed but not yet purged (awaiting dirty decay)."
);
gauge!(
    MUZZY,
    "jemalloc_muzzy_bytes",
    "Bytes madvise(FREE)'d, reclaimable by the OS under pressure (awaiting muzzy decay)."
);
counter!(
    DIRTY_NMADVISE,
    "jemalloc_dirty_nmadvise_total",
    "madvise() calls issued purging dirty pages."
);
counter!(
    MUZZY_NMADVISE,
    "jemalloc_muzzy_nmadvise_total",
    "madvise() calls issued purging muzzy pages."
);

pub const MUTEX_WAITS: Family = Family::counter(
    "jemalloc_mutex_waits_total",
    MUTEX,
    "Lock acquisitions that waited for another thread.",
);

pub const MUTEX_WAIT_SECONDS: Family = Family::counter(
    "jemalloc_mutex_wait_seconds_total",
    MUTEX,
    "Time lock acquisitions spent waiting.",
);

gauge!(
    OPT_DIRTY_DECAY,
    "jemalloc_opt_dirty_decay_ms",
    "Resolved dirty_decay_ms (confirm _RJEM_MALLOC_CONF parsed; expect 1000)."
);
gauge!(
    OPT_MUZZY_DECAY,
    "jemalloc_opt_muzzy_decay_ms",
    "Resolved muzzy_decay_ms (confirm _RJEM_MALLOC_CONF parsed; expect 1000)."
);
gauge!(
    ARENAS,
    "jemalloc_arenas",
    "Arenas in use: the automatic ones plus the arena for huge allocations."
);
gauge!(
    OPT_NARENAS,
    "jemalloc_opt_narenas",
    "Configured automatic arenas (parallelism vs slab slack trade-off)."
);
gauge!(
    BACKGROUND_THREAD,
    "jemalloc_background_thread",
    "1 if purging runs on background threads (off the alloc hot path)."
);
gauge!(PROF_ACTIVE, "jemalloc_prof_active", "1 if heap profiling samples allocations.");
gauge!(
    OPT_LG_PROF_SAMPLE,
    "jemalloc_opt_lg_prof_sample",
    "Live lg2 of the mean bytes between heap-profile samples (jemalloc default 19)."
);
gauge!(PAGE, "jemalloc_page_bytes", "Allocator page size, fixed at build.");
gauge!(
    CACHE_OBLIVIOUS,
    "jemalloc_cache_oblivious",
    "1 if every large extent (a heap-profile sample included) carries one pad page."
);
gauge!(
    PROF_SAMPLE_OVERHEAD,
    "jemalloc_prof_sample_overhead_ratio",
    "Sampled-extent bytes per live byte of objects up to a page at the live profiling interval (0 = no sampling)."
);
gauge!(
    PROF_SAMPLE_OVERHEAD_MAX,
    "jemalloc_prof_sample_overhead_max_ratio",
    "The same for the largest small size class: the bound over the small heap."
);

pub const HOST_THP: Family = Family::gauge(
    "jemalloc_host_thp",
    Labels::Product(&[Dim::new("mode", &[])]),
    "The host's transparent_hugepage mode (unreadable when /sys does not say).",
)
.semi_open();

gauge!(
    FOOTPRINT_HAZARDS,
    "jemalloc_footprint_hazards",
    "Resolved settings whose footprint cannot hold on this host (see the startup line)."
);
gauge!(
    PROCESS_VIRTUAL_MEMORY,
    "process_virtual_memory_bytes",
    "Virtual address space (RSS-independent; jemalloc retained shows here)."
);
gauge!(
    PROCESS_RESIDENT_MEMORY,
    "process_resident_memory_bytes",
    "OS RSS the cgroup OOMs on — compare to jemalloc_resident_bytes."
);
gauge!(
    PROCESS_THREADS,
    "process_threads",
    "OS thread count (each ~stack of RSS; off-heap growth source)."
);

/// Every family, in exposition order.
pub const FAMILIES: &[Family] = &[
    ALLOCATED,
    ACTIVE,
    RESIDENT,
    MAPPED,
    RETAINED,
    METADATA,
    SMALL_ALLOCATED,
    LARGE_ALLOCATED,
    SMALL_NMALLOC,
    SMALL_NDALLOC,
    BIN_LIVE,
    BIN_ACTIVE,
    BIN_NONFULL_SLABS,
    BIN_SAMPLED_LIVE,
    BIN_SAMPLED_BYTES,
    SAMPLED_EXTENT,
    LEXTENT_LIVE,
    DIRTY,
    MUZZY,
    DIRTY_NMADVISE,
    MUZZY_NMADVISE,
    MUTEX_WAITS,
    MUTEX_WAIT_SECONDS,
    OPT_DIRTY_DECAY,
    OPT_MUZZY_DECAY,
    ARENAS,
    OPT_NARENAS,
    BACKGROUND_THREAD,
    PROF_ACTIVE,
    OPT_LG_PROF_SAMPLE,
    PAGE,
    CACHE_OBLIVIOUS,
    PROF_SAMPLE_OVERHEAD,
    PROF_SAMPLE_OVERHEAD_MAX,
    HOST_THP,
    FOOTPRINT_HAZARDS,
    PROCESS_VIRTUAL_MEMORY,
    PROCESS_RESIDENT_MEMORY,
    PROCESS_THREADS,
];
