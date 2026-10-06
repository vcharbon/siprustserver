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
  small object of `s` bytes in an extent of its own, `ceil(s / page)` pages
  plus one pad page (`cache_oblivious`), counted in `active` but not in
  `allocated`: two pages for every sampled 3 KiB transaction record, 31 % of
  them (1 − e^(−3072/8192)), and up to five for the largest small class
  (3.5 pages). Sampling costs `pages(s) × page / 2^lg_prof_sample` bytes per
  live byte of a class: at 2^19 (jemalloc's default) 1.6 % to 3.9 % on 4 KiB
  pages, 25 % to 63 % on 64 KiB pages. The interval is a property of the page
  size the build fixed, not of a host.
- **Slab slack** was the rest, and it grows with the arenas the threads spread
  over: a freed region returns to the arena that owns its slab, and only that
  arena's threads refill it. At a held peak after 5 min, `active/allocated`
  was 1.27 with one arena per thread (jemalloc's default of 4 arenas per CPU,
  26 threads) and 1.43 at 98 threads (RSS 685 MB against 372 MB for the same
  heap); with 8, 4, 2 and 1 arenas it was 1.14, 1.10, 1.05 and 1.01 at 26
  threads, and 1.08, 1.07 and 1.01 for 4, 2 and 1 arenas at 98 threads. The
  worker used under one core in these runs, so they bound the slack, not the
  cost of sharing an arena's locks between busy threads.
- **Transparent huge pages** refill purged extents as huge pages on a host
  whose THP mode is `always`, unless jemalloc marks its extents out.

## Decision

1. **Every binary that links `jemalloc-stats` carries allocator defaults that
   hold on any host**: the crate defines jemalloc's `malloc_conf` symbol
   (`_rjem_malloc_conf` under tikv-jemalloc-sys's prefix) as `narenas:4`,
   `thp:never`, 1 s dirty and muzzy decay. A symbol, not a build variable, so
   no build path (another cwd, `cargo install`, a downstream workspace, an
   exported `JEMALLOC_SYS_WITH_MALLOC_CONF`) loses them. jemalloc applies its
   sources in order, each overriding the earlier key by key: the string
   compiled into the library, this symbol, the `/etc/_rjem_malloc.conf`
   symlink name, then `_RJEM_MALLOC_CONF`. An operator's environment wins.
2. **Four arenas, whatever the host's CPU count.** Four keep the slack within
   1.10 at any thread count measured, where one arena per thread reached
   1.43, and leave four locks per bin instead of one for busy threads. A
   workload that waits on them sets `narenas` in the environment and reads
   the waits below.
3. **The startup line reports the resolved settings and their footprint**
   (`jemalloc_stats::log_config`): the options the binary carries, page size,
   configured and in-use arenas (`opt.narenas`, `arenas.narenas`, which adds
   the arena for huge allocations), threads, decay, THP, profiling state and
   live interval, the sampling overhead for objects up to a page and for the
   largest small class, and every hazard, each with the setting that removes
   it. `/metrics` carries the same as gauges.
4. **A hazard is a setting whose footprint cannot hold here**: a sampling
   overhead above 5 % for the largest small class; more than 8 arenas; a
   negative (never-purging) decay; jemalloc's extents left eligible for huge
   pages on a host whose THP mode is `always` or cannot be read
   (`jemalloc_host_thp{mode="unreadable"}`). It is reported, never corrected:
   an operator's value wins, visibly.
5. **The exposition shows the footprint per size class and the allocator's
   lock waits**: live bytes, slab bytes (`jemalloc_bin_active_bytes`),
   non-full slabs, the sampled objects outside every slab
   (`jemalloc_bin_sampled_live`, `jemalloc_sampled_extent_bytes`), and
   `jemalloc_mutex_waits_total` / `jemalloc_mutex_wait_seconds_total` per
   arena mutex, for the bin mutexes, and per global mutex, so a gap between
   `active` and `allocated`, or a cost of fewer arenas, is attributed, not
   guessed.
6. **Profiling stays on at jemalloc's interval.** `/debug/heap` remains usable
   with `prof:true,prof_active:true,lg_prof_sample:19`: a 300 MB leak of 3 KiB
   objects yields about 600 samples.

## Consequences

- `B2BUA_MAX_RSS` is sized from the live heap plus the slab slack and what the
  allocator keeps of the preceding peak, not from a profiling cost (ADR-0037,
  consequences).
- The footprint test (`crates/jemalloc-stats/tests/footprint.rs`) checks that
  a binary linking the crate resolves the defaults with no setting, holds a
  transaction-table pattern on a multi-thread runtime and bounds
  `active/allocated` at 1.3 with profiling off and at the default interval,
  and keeps the diagnostic interval as the falsifier of the sampling model.
  Its 1.5 s hold cannot tell arena counts apart; minutes of churn do.
- A build with 64 KiB pages reports the default interval as a hazard and
  names 2^23; the operator sets it, the report does not. The page size is the
  build host's: an image built on 4 KiB pages for a 64 KiB-page kernel does
  not start.
- A binary on jemalloc that does not link `jemalloc-stats` runs on jemalloc's
  own defaults and reports nothing.
