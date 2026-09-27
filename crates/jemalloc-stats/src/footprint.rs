//! The allocator's footprint on this deployment, from the resolved jemalloc
//! configuration: what a live byte costs beyond itself, and the settings that
//! cannot hold here (a [`Hazard`]). Pure and target-independent: the mallctl
//! readers live in `lib.rs`. ADR-0038 states why the report exists.
//!
//! A heap-profile sample of a small object of `s` bytes occupies
//! `ceil(s / page)` pages of its own outside `allocated`, plus one pad page on
//! a cache-oblivious build, so sampling costs `pages(s) × page / 2^lg_sample`
//! bytes per live byte of that class: two pages up to a page, five for the
//! largest small class (3.5 pages). The report states both, and the hazard
//! reads the largest.

use std::fmt;

/// The sample overhead above which profiling is a footprint hazard.
pub const SAMPLE_OVERHEAD_LIMIT: f64 = 0.05;

/// The arena count above which slab slack is a footprint hazard. A freed
/// region returns to the arena owning its slab, so the empty slab share grows
/// with the arenas the threads spread over: measured `active/allocated` 1.08
/// to 1.10 at 4 arenas, 1.14 at 8, 1.27 at 24 or more (26 threads), 1.43 at 97
/// arenas with 98 threads (ADR-0038).
pub const ARENA_LIMIT: u32 = 8;

/// The resolved allocator settings a footprint depends on.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The options the binary carries (`opt.malloc_conf.global_var`),
    /// [`crate::defaults::MALLOC_CONF`] when this crate is linked.
    pub malloc_conf: Option<String>,
    /// Bytes per allocator page (`arenas.page`), fixed when jemalloc was built.
    pub page: usize,
    /// The largest small size class (`arenas.bin.<nbins-1>.size`).
    pub small_max: usize,
    /// Large extents carry one pad page (`opt.cache_oblivious`).
    pub cache_oblivious: bool,
    /// Heap profiling samples allocations (`prof.active`); `None` on a build
    /// without profiling.
    pub prof_active: Option<bool>,
    /// Mean bytes between samples, log2 (`prof.lg_sample`, the live value).
    pub lg_prof_sample: u32,
    /// Configured automatic arenas (`opt.narenas`).
    pub opt_narenas: u32,
    /// Arenas in use (`arenas.narenas`): the automatic ones plus the arena for
    /// huge allocations.
    pub narenas: u32,
    /// jemalloc's huge-page mode for its extents (`opt.thp`).
    pub thp: String,
    /// The host's `transparent_hugepage/enabled` mode, when readable.
    pub host_thp: Option<String>,
    /// Purge delays; negative never purges.
    pub dirty_decay_ms: i64,
    pub muzzy_decay_ms: i64,
    pub background_thread: bool,
    /// OS threads of the process, each with a thread cache.
    pub threads: u64,
}

/// A resolved setting whose footprint cannot hold on this deployment.
#[derive(Debug, Clone, PartialEq)]
pub enum Hazard {
    /// Sampling this often costs up to `overhead` bytes of sampled extents per
    /// live small byte; `recommended_lg` keeps it under the limit.
    ProfSampleInterval {
        lg_prof_sample: u32,
        page: usize,
        sample_pages_max: usize,
        overhead: f64,
        recommended_lg: u32,
    },
    /// More arenas than [`ARENA_LIMIT`]: slab slack grows with them.
    ArenaCount { opt_narenas: u32 },
    /// A negative decay never returns freed pages: RSS keeps the peak.
    NeverPurges { dirty_decay_ms: i64, muzzy_decay_ms: i64 },
    /// jemalloc's extents stay eligible for huge pages on a host whose THP
    /// mode is `always`, or whose mode cannot be read.
    HostThp { thp: String, host_thp: Option<String> },
}

impl fmt::Display for Hazard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hazard::ProfSampleInterval { lg_prof_sample, page, sample_pages_max, overhead, recommended_lg } => write!(
                f,
                "profiling interval 2^{lg_prof_sample} costs up to {:.0}% of the small live heap in sampled extents (up to {sample_pages_max} pages of {page} per sample); set lg_prof_sample:{recommended_lg} or more",
                overhead * 100.0
            ),
            Hazard::ArenaCount { opt_narenas } => write!(
                f,
                "narenas={opt_narenas} leaves slab pages empty in proportion to the arenas the threads spread over (active/allocated 1.10 at 4, 1.27 at 24 or more); set narenas:4"
            ),
            Hazard::NeverPurges { dirty_decay_ms, muzzy_decay_ms } => write!(
                f,
                "dirty_decay_ms={dirty_decay_ms} muzzy_decay_ms={muzzy_decay_ms}: a negative decay never purges and RSS keeps the peak heap; set both to 1000"
            ),
            Hazard::HostThp { thp, host_thp } => write!(
                f,
                "host THP is {} and jemalloc thp={thp}: purged extents may refill as huge pages and RSS follow the retained size; set thp:never",
                host_thp.as_deref().unwrap_or("unreadable")
            ),
        }
    }
}

/// The footprint of a [`Resolved`] configuration and its hazards.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub resolved: Resolved,
    /// Pages one sample of an object up to a page occupies.
    pub sample_pages: usize,
    /// Pages one sample of the largest small object occupies.
    pub sample_pages_max: usize,
    /// Sampled-extent bytes per live byte of objects up to a page; 0 without
    /// sampling.
    pub sample_overhead: f64,
    /// The same for the largest small class: the bound over the small heap.
    pub sample_overhead_max: f64,
    pub hazards: Vec<Hazard>,
}

impl Report {
    pub fn of(resolved: Resolved) -> Self {
        let pad = usize::from(resolved.cache_oblivious);
        let sample_pages = 1 + pad;
        let sample_pages_max = resolved.small_max.div_ceil(resolved.page).max(1) + pad;
        let interval = 2f64.powi(resolved.lg_prof_sample as i32);
        let per_sample = |pages: usize| match resolved.prof_active {
            Some(true) => (pages * resolved.page) as f64 / interval,
            _ => 0.0,
        };
        let sample_overhead = per_sample(sample_pages);
        let sample_overhead_max = per_sample(sample_pages_max);
        let mut hazards = Vec::new();
        if sample_overhead_max > SAMPLE_OVERHEAD_LIMIT {
            hazards.push(Hazard::ProfSampleInterval {
                lg_prof_sample: resolved.lg_prof_sample,
                page: resolved.page,
                sample_pages_max,
                overhead: sample_overhead_max,
                recommended_lg: recommended_lg_prof_sample(sample_pages_max * resolved.page),
            });
        }
        if resolved.opt_narenas > ARENA_LIMIT {
            hazards.push(Hazard::ArenaCount { opt_narenas: resolved.opt_narenas });
        }
        if resolved.dirty_decay_ms < 0 || resolved.muzzy_decay_ms < 0 {
            hazards.push(Hazard::NeverPurges {
                dirty_decay_ms: resolved.dirty_decay_ms,
                muzzy_decay_ms: resolved.muzzy_decay_ms,
            });
        }
        let host_may_refill = !matches!(resolved.host_thp.as_deref(), Some("madvise" | "never"));
        if host_may_refill && resolved.thp != "never" {
            hazards.push(Hazard::HostThp {
                thp: resolved.thp.clone(),
                host_thp: resolved.host_thp.clone(),
            });
        }
        Self {
            resolved,
            sample_pages,
            sample_pages_max,
            sample_overhead,
            sample_overhead_max,
            hazards,
        }
    }

    /// The one startup line: every resolved value, then each hazard.
    pub fn line(&self) -> String {
        let r = &self.resolved;
        let prof = match r.prof_active {
            Some(true) => "1",
            Some(false) => "0",
            None => "-1",
        };
        let mut s = format!(
            "jemalloc active: malloc_conf={} page={} opt.narenas={} narenas={} threads={} dirty_decay_ms={} muzzy_decay_ms={} background_thread={} thp={} host_thp={} cache_oblivious={} prof.active={prof} lg_prof_sample={} sample_pages={}..{} sample_overhead={:.3}..{:.3} hazards={}",
            r.malloc_conf.as_deref().unwrap_or("none"),
            r.page,
            r.opt_narenas,
            r.narenas,
            r.threads,
            r.dirty_decay_ms,
            r.muzzy_decay_ms,
            r.background_thread,
            r.thp,
            r.host_thp.as_deref().unwrap_or("unreadable"),
            r.cache_oblivious,
            r.lg_prof_sample,
            self.sample_pages,
            self.sample_pages_max,
            self.sample_overhead,
            self.sample_overhead_max,
            self.hazards.len()
        );
        for h in &self.hazards {
            s.push_str(" hazard: ");
            s.push_str(&h.to_string());
        }
        s
    }
}

/// The smallest `lg_prof_sample` at which a sample of `sample_bytes` costs at
/// most [`SAMPLE_OVERHEAD_LIMIT`] of the live bytes of its class.
pub fn recommended_lg_prof_sample(sample_bytes: usize) -> u32 {
    let floor = sample_bytes as f64 / SAMPLE_OVERHEAD_LIMIT;
    let mut lg = 0;
    while 2f64.powi(lg as i32) < floor {
        lg += 1;
    }
    lg
}

/// The bracketed mode of `/sys/kernel/mm/transparent_hugepage/enabled`
/// (`always [madvise] never` → `madvise`).
pub fn host_thp_mode(enabled: &str) -> Option<String> {
    let start = enabled.find('[')? + 1;
    let end = enabled[start..].find(']')? + start;
    Some(enabled[start..end].to_string())
}

#[cfg(test)]
mod footprint_tests {
    use super::*;

    /// The compiled defaults on 4 KiB pages, sampling at `lg` when `prof_active`.
    fn resolved(page: usize, prof_active: Option<bool>, lg: u32) -> Resolved {
        Resolved {
            malloc_conf: Some(crate::defaults::MALLOC_CONF.into()),
            page,
            small_max: page / 2 * 7,
            cache_oblivious: true,
            prof_active,
            lg_prof_sample: lg,
            opt_narenas: 4,
            narenas: 5,
            thp: "never".into(),
            host_thp: Some("madvise".into()),
            dirty_decay_ms: 1000,
            muzzy_decay_ms: 1000,
            background_thread: false,
            threads: 29,
        }
    }

    /// 2^13 on 4 KiB pages: two pages per sample up to a page, a byte per
    /// live byte; five for the largest small class.
    #[test]
    fn a_fine_interval_on_4k_pages_is_a_hazard_and_names_19() {
        let r = Report::of(resolved(4096, Some(true), 13));
        assert_eq!((r.sample_pages, r.sample_pages_max), (2, 5));
        assert_eq!((r.sample_overhead, r.sample_overhead_max), (1.0, 2.5));
        assert_eq!(
            r.hazards,
            vec![Hazard::ProfSampleInterval {
                lg_prof_sample: 13,
                page: 4096,
                sample_pages_max: 5,
                overhead: 2.5,
                recommended_lg: 19
            }]
        );
        let line = r.line();
        assert!(
            line.contains("lg_prof_sample=13") && line.contains("hazard: profiling interval 2^13"),
            "{line}"
        );
    }

    /// jemalloc's default interval on 4 KiB pages costs 1/64 up to a page and
    /// under 4 % for the largest small class.
    #[test]
    fn the_default_interval_on_4k_pages_holds() {
        let r = Report::of(resolved(4096, Some(true), 19));
        assert!((r.sample_overhead - 1.0 / 64.0).abs() < 1e-9);
        assert!((r.sample_overhead_max - 5.0 / 128.0).abs() < 1e-9);
        assert!(r.hazards.is_empty());
        assert!(r.line().ends_with("hazards=0"), "{}", r.line());
    }

    /// The same default interval on 64 KiB pages costs a quarter of the heap.
    #[test]
    fn the_default_interval_on_64k_pages_is_a_hazard_and_names_23() {
        let r = Report::of(resolved(65536, Some(true), 19));
        assert_eq!((r.sample_overhead, r.sample_overhead_max), (0.25, 0.625));
        assert!(matches!(r.hazards[..], [Hazard::ProfSampleInterval { recommended_lg: 23, .. }]));
    }

    /// Without the pad page a sample is one page up to a page, four at most.
    #[test]
    fn without_cache_oblivious_padding_a_sample_is_one_page() {
        let mut res = resolved(4096, Some(true), 13);
        res.cache_oblivious = false;
        let r = Report::of(res);
        assert_eq!((r.sample_pages, r.sample_overhead), (1, 0.5));
        assert_eq!((r.sample_pages_max, r.sample_overhead_max), (4, 2.0));
        assert!(matches!(r.hazards[..], [Hazard::ProfSampleInterval { recommended_lg: 19, .. }]));
    }

    /// No sampling, no cost: profiling inactive or built out.
    #[test]
    fn inactive_or_absent_profiling_costs_nothing() {
        for prof in [Some(false), None] {
            let r = Report::of(resolved(4096, prof, 13));
            assert_eq!((r.sample_overhead, r.sample_overhead_max), (0.0, 0.0));
            assert!(r.hazards.is_empty());
        }
        assert!(Report::of(resolved(4096, None, 13)).line().contains("prof.active=-1"));
    }

    /// Both arena counts are on the line; more than eight is a hazard.
    #[test]
    fn more_than_eight_arenas_is_a_hazard() {
        let r = Report::of(resolved(4096, Some(false), 19));
        assert!(r.line().contains("opt.narenas=4 narenas=5"), "{}", r.line());
        let mut res = resolved(4096, Some(false), 19);
        res.opt_narenas = 8;
        assert!(Report::of(res.clone()).hazards.is_empty());
        res.opt_narenas = 96;
        let r = Report::of(res);
        assert_eq!(r.hazards, vec![Hazard::ArenaCount { opt_narenas: 96 }]);
        assert!(r.line().contains("hazard: narenas=96"), "{}", r.line());
    }

    #[test]
    fn a_negative_decay_is_a_hazard() {
        for (dirty, muzzy) in [(-1, 1000), (1000, -1)] {
            let mut res = resolved(4096, Some(false), 19);
            (res.dirty_decay_ms, res.muzzy_decay_ms) = (dirty, muzzy);
            assert_eq!(
                Report::of(res).hazards,
                vec![Hazard::NeverPurges { dirty_decay_ms: dirty, muzzy_decay_ms: muzzy }]
            );
        }
        let mut res = resolved(4096, Some(false), 19);
        (res.dirty_decay_ms, res.muzzy_decay_ms) = (0, 0);
        assert!(Report::of(res).hazards.is_empty());
    }

    /// Eligible extents are a hazard on a host THP mode of `always` or an
    /// unreadable one; `thp:never` holds on either.
    #[test]
    fn host_thp_always_or_unreadable_is_a_hazard_unless_jemalloc_opts_out() {
        let mut res = resolved(4096, Some(false), 19);
        res.thp = "default".into();
        for host in [Some("always"), None] {
            res.host_thp = host.map(String::from);
            let r = Report::of(res.clone());
            assert_eq!(
                r.hazards,
                vec![Hazard::HostThp { thp: "default".into(), host_thp: host.map(String::from) }]
            );
        }
        assert!(Report::of(res.clone()).line().contains("host THP is unreadable"));
        for host in ["madvise", "never"] {
            res.host_thp = Some(host.into());
            assert!(Report::of(res.clone()).hazards.is_empty());
        }
        res.thp = "never".into();
        for host in [Some("always"), None] {
            res.host_thp = host.map(String::from);
            assert!(Report::of(res.clone()).hazards.is_empty());
        }
    }

    #[test]
    fn the_recommended_interval_keeps_a_sample_under_the_limit() {
        assert_eq!(recommended_lg_prof_sample(8192), 18);
        assert_eq!(recommended_lg_prof_sample(20480), 19);
        assert_eq!(recommended_lg_prof_sample(16384), 19);
        assert_eq!(recommended_lg_prof_sample(327680), 23);
    }

    #[test]
    fn the_host_thp_mode_is_the_bracketed_word() {
        assert_eq!(host_thp_mode("[always] madvise never\n"), Some("always".into()));
        assert_eq!(host_thp_mode("always [madvise] never"), Some("madvise".into()));
        assert_eq!(host_thp_mode("always madvise never"), None);
    }
}
