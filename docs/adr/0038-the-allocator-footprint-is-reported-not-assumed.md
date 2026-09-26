# The allocator's footprint is reported at startup, not assumed from one host

**Status:** accepted (2026-09-26)

## Context

Admission is bounded by process RSS (ADR-0037), so the RSS a live heap
produces must be predictable wherever the worker runs. Under a capacity ramp
jemalloc held twice the live heap in active pages (`active` 1758 MB for
`allocated` 869 MB at 189 k transactions), and the RSS ceiling refused calls
at half the heap it was sized for. The gap was measured off the cluster with
the real worker under SIPp at 1000 cps, 63.6 k transactions held, and
decomposed per size class:

- **Heap-profile sampling** at the lane's diagnostic interval (`lg_prof_sample:13`,
  a sample every 8 KiB) was 93 % of the gap. jemalloc allocates a sampled
  small object in an extent of its own, its size rounded up to a page plus one
  pad page (`cache_oblivious`), counted in `active` but not in `allocated`:
  two pages for every sampled 3 KiB transaction record, 31 % of them
  (1 − e^(−3072/8192)). Sampling costs `sample_pages × page / 2^lg_prof_sample`
  bytes per live small byte: 100 % at 2^13 on 4 KiB pages, 1.6 % at 2^19
  (jemalloc's default), 25 % at 2^19 on 64 KiB pages. The interval is a
  property of the page size the build fixed, not of a host.
- **Slab slack** was the rest, and it grows with the number of threads that
  share no arena: a freed region returns to the arena that owns its slab, and
  only that arena's threads refill it. With one arena per thread (jemalloc's
  default of 4 arenas per CPU) `active/allocated` settled at 1.27 after 5 min
  at 26 threads and reached 1.43 at 98 threads (RSS 685 MB against 372 MB
  for the same heap); with 8, 4, 2 and 1 shared arenas it settled at 1.14,
  1.10, 1.05 and 1.01 at 26 threads, and at 1.08, 1.07 and 1.01 for 4, 2 and
  1 arenas at 98 threads, the worker's CPU unchanged (0.59–0.72 cores) in
  every case: the thread caches carry the parallelism.
- **Transparent huge pages** refill purged extents as huge pages on a host
  whose THP mode is `always`, unless jemalloc marks its extents out.

## Decision

1. **The binaries carry allocator defaults that hold on any host**, compiled
   into the bundled jemalloc (`JEMALLOC_SYS_WITH_MALLOC_CONF` in
   `.cargo/config.toml`): `thp:never`, 1 s dirty and muzzy decay, and
   `narenas:1`, whose slack is 1 % at any thread count. `_RJEM_MALLOC_CONF`
   at run time overrides them key by key; a workload that contends on the one
   arena's locks sets `narenas` there, and the startup line reports it.
2. **The startup line reports the resolved settings and their footprint**
   (`jemalloc_stats::log_config`): page size, arenas, threads, decay, THP,
   profiling state and interval, the expected sampling overhead, and every
   hazard, each with the setting that removes it. `/metrics` carries the same
   as gauges: `jemalloc_prof_sample_overhead_ratio`, `jemalloc_footprint_hazards`,
   `jemalloc_page_bytes`, `jemalloc_cache_oblivious`.
3. **A hazard is a setting whose footprint cannot hold here**: a sampling
   overhead above 5 % of the small live heap, or a host THP mode of `always`
   with jemalloc's extents left eligible. It is reported, never corrected: an
   operator's value wins, visibly.
4. **The exposition shows the footprint per size class**: live bytes, slab
   bytes (`jemalloc_bin_active_bytes`), non-full slabs, and the sampled
   objects outside every slab (`jemalloc_bin_sampled_live` in the bin of their
   page-rounded size, `jemalloc_sampled_extent_bytes` in total), so a gap
   between `active` and `allocated` is attributed, not guessed.
5. **Profiling stays on at jemalloc's interval.** `/debug/heap` remains usable
   with `prof:true,prof_active:true,lg_prof_sample:19`: a 300 MB leak of 3 KiB
   objects yields about 600 samples.

## Consequences

- `B2BUA_MAX_RSS` is sized from the live heap plus the slab slack and the
  off-heap base, not from a profiling cost (ADR-0037, consequences).
- The footprint test (`crates/jemalloc-stats/tests/footprint.rs`) holds a
  transaction-table pattern on a multi-thread runtime and bounds
  `active/allocated` at 1.3 with profiling off and at the default interval;
  the diagnostic interval is kept as the falsifier of the sampling model.
- A build with 64 KiB pages reports the default interval as a hazard and
  names 2^23; the operator sets it, the report does not.
