//! The allocator's footprint on this deployment, from the resolved jemalloc
//! configuration: what a live byte costs beyond itself, and the settings that
//! cannot hold here (a [`Hazard`]). Pure and target-independent: the mallctl
//! readers live in `lib.rs`. ADR-0038 states why the report exists.
//!
//! A heap-profile sample of a small object occupies `sample_pages` pages of
//! its own outside `allocated`, so sampling costs
//! `sample_pages × page / 2^lg_prof_sample` bytes per live small byte, a
//! function of the page size the build fixed. A host whose THP mode is
//! `always` refills purged extents as huge pages unless jemalloc opts out.

use std::fmt;

/// The sample overhead above which profiling is a footprint hazard.
pub const SAMPLE_OVERHEAD_LIMIT: f64 = 0.05;

/// The resolved allocator settings a footprint depends on.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// Bytes per allocator page (`arenas.page`).
    pub page: usize,
    /// Large extents carry one pad page (`opt.cache_oblivious`).
    pub cache_oblivious: bool,
    /// Heap profiling samples allocations (`prof.active`); `None` on a build
    /// without profiling.
    pub prof_active: Option<bool>,
    /// Mean bytes between samples, log2 (`opt.lg_prof_sample`).
    pub lg_prof_sample: u32,
    /// Automatic arenas (`arenas.narenas`).
    pub narenas: u32,
    /// jemalloc's huge-page mode for its extents (`opt.thp`).
    pub thp: String,
    /// The host's `transparent_hugepage/enabled` mode, when readable.
    pub host_thp: Option<String>,
    pub dirty_decay_ms: i64,
    pub muzzy_decay_ms: i64,
    pub background_thread: bool,
    /// OS threads of the process, each with a thread cache and an arena.
    pub threads: u64,
}

/// A resolved setting whose footprint cannot hold on this deployment.
#[derive(Debug, Clone, PartialEq)]
pub enum Hazard {
    /// Sampling this often costs `overhead` bytes of sampled extents per live
    /// small byte; `recommended_lg` keeps it at 1/64.
    ProfSampleInterval {
        lg_prof_sample: u32,
        page: usize,
        sample_pages: usize,
        overhead: f64,
        recommended_lg: u32,
    },
    /// The host's THP mode is `always` and jemalloc's extents stay eligible.
    HostThpAlways { thp: String },
}

impl fmt::Display for Hazard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hazard::ProfSampleInterval { lg_prof_sample, page, sample_pages, overhead, recommended_lg } => write!(
                f,
                "profiling interval 2^{lg_prof_sample} costs up to {:.0}% of the small live heap in sampled extents ({sample_pages} pages of {page} per sample); set lg_prof_sample:{recommended_lg} or more",
                overhead * 100.0
            ),
            Hazard::HostThpAlways { thp } => write!(
                f,
                "host THP is always and jemalloc thp={thp}: purged extents refill as huge pages and RSS follows the retained size; set thp:never"
            ),
        }
    }
}

/// The footprint of a [`Resolved`] configuration and its hazards.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub resolved: Resolved,
    /// Pages one sample of a small object occupies.
    pub sample_pages: usize,
    /// Expected sampled-extent bytes per live small byte; 0 without sampling.
    pub sample_overhead: f64,
    pub hazards: Vec<Hazard>,
}

impl Report {
    pub fn of(resolved: Resolved) -> Self {
        let sample_pages = 1 + usize::from(resolved.cache_oblivious);
        let sample_bytes = (sample_pages * resolved.page) as f64;
        let sample_overhead = match resolved.prof_active {
            Some(true) => sample_bytes / 2f64.powi(resolved.lg_prof_sample as i32),
            _ => 0.0,
        };
        let mut hazards = Vec::new();
        if sample_overhead > SAMPLE_OVERHEAD_LIMIT {
            hazards.push(Hazard::ProfSampleInterval {
                lg_prof_sample: resolved.lg_prof_sample,
                page: resolved.page,
                sample_pages,
                overhead: sample_overhead,
                recommended_lg: recommended_lg_prof_sample(sample_pages * resolved.page),
            });
        }
        if resolved.host_thp.as_deref() == Some("always") && resolved.thp != "never" {
            hazards.push(Hazard::HostThpAlways { thp: resolved.thp.clone() });
        }
        Self { resolved, sample_pages, sample_overhead, hazards }
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
            "jemalloc active: page={} narenas={} threads={} dirty_decay_ms={} muzzy_decay_ms={} background_thread={} thp={} host_thp={} cache_oblivious={} prof.active={prof} lg_prof_sample={} sample_pages={} sample_overhead={:.3} hazards={}",
            r.page,
            r.narenas,
            r.threads,
            r.dirty_decay_ms,
            r.muzzy_decay_ms,
            r.background_thread,
            r.thp,
            r.host_thp.as_deref().unwrap_or("?"),
            r.cache_oblivious,
            r.lg_prof_sample,
            self.sample_pages,
            self.sample_overhead,
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
/// most 1/64 of the live small heap: `ceil(log2(sample_bytes)) + 6`.
pub fn recommended_lg_prof_sample(sample_bytes: usize) -> u32 {
    let lg = usize::BITS - sample_bytes.max(1).leading_zeros() - 1;
    let lg = if sample_bytes.is_power_of_two() { lg } else { lg + 1 };
    lg + 6
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

    fn resolved(page: usize, prof_active: Option<bool>, lg: u32) -> Resolved {
        Resolved {
            page,
            cache_oblivious: true,
            prof_active,
            lg_prof_sample: lg,
            narenas: 97,
            thp: "never".into(),
            host_thp: Some("madvise".into()),
            dirty_decay_ms: 1000,
            muzzy_decay_ms: 1000,
            background_thread: false,
            threads: 29,
        }
    }

    /// 2^13 on 4 KiB pages: two pages per sample, a byte per live byte.
    #[test]
    fn a_fine_interval_on_4k_pages_is_a_hazard_and_names_19() {
        let r = Report::of(resolved(4096, Some(true), 13));
        assert_eq!(r.sample_pages, 2);
        assert_eq!(r.sample_overhead, 1.0);
        assert_eq!(
            r.hazards,
            vec![Hazard::ProfSampleInterval {
                lg_prof_sample: 13,
                page: 4096,
                sample_pages: 2,
                overhead: 1.0,
                recommended_lg: 19
            }]
        );
        let line = r.line();
        assert!(
            line.contains("lg_prof_sample=13") && line.contains("hazard: profiling interval 2^13"),
            "{line}"
        );
    }

    /// jemalloc's default interval on 4 KiB pages costs 1/64.
    #[test]
    fn the_default_interval_on_4k_pages_holds() {
        let r = Report::of(resolved(4096, Some(true), 19));
        assert!((r.sample_overhead - 1.0 / 64.0).abs() < 1e-9);
        assert!(r.hazards.is_empty());
        assert!(r.line().ends_with("hazards=0"), "{}", r.line());
    }

    /// The same default interval on 64 KiB pages costs a quarter of the heap.
    #[test]
    fn the_default_interval_on_64k_pages_is_a_hazard_and_names_23() {
        let r = Report::of(resolved(65536, Some(true), 19));
        assert_eq!(r.sample_overhead, 0.25);
        assert!(matches!(r.hazards[..], [Hazard::ProfSampleInterval { recommended_lg: 23, .. }]));
    }

    /// Without the pad page a sample is one page: 2^13 costs half.
    #[test]
    fn without_cache_oblivious_padding_a_sample_is_one_page() {
        let mut res = resolved(4096, Some(true), 13);
        res.cache_oblivious = false;
        let r = Report::of(res);
        assert_eq!((r.sample_pages, r.sample_overhead), (1, 0.5));
        assert!(matches!(r.hazards[..], [Hazard::ProfSampleInterval { recommended_lg: 18, .. }]));
    }

    /// No sampling, no cost: profiling inactive or built out.
    #[test]
    fn inactive_or_absent_profiling_costs_nothing() {
        for prof in [Some(false), None] {
            let r = Report::of(resolved(4096, prof, 13));
            assert_eq!(r.sample_overhead, 0.0);
            assert!(r.hazards.is_empty());
        }
        assert!(Report::of(resolved(4096, None, 13)).line().contains("prof.active=-1"));
    }

    #[test]
    fn host_thp_always_is_a_hazard_unless_jemalloc_opts_out() {
        let mut res = resolved(4096, Some(false), 19);
        res.host_thp = Some("always".into());
        res.thp = "default".into();
        let r = Report::of(res.clone());
        assert_eq!(r.hazards, vec![Hazard::HostThpAlways { thp: "default".into() }]);
        res.thp = "never".into();
        assert!(Report::of(res.clone()).hazards.is_empty());
        res.thp = "default".into();
        res.host_thp = Some("madvise".into());
        assert!(Report::of(res.clone()).hazards.is_empty());
        res.host_thp = None;
        assert!(Report::of(res).hazards.is_empty());
    }

    #[test]
    fn the_recommended_interval_is_six_above_the_sample_size() {
        assert_eq!(recommended_lg_prof_sample(8192), 19);
        assert_eq!(recommended_lg_prof_sample(4096), 18);
        assert_eq!(recommended_lg_prof_sample(131072), 23);
        assert_eq!(recommended_lg_prof_sample(12288), 20);
    }

    #[test]
    fn the_host_thp_mode_is_the_bracketed_word() {
        assert_eq!(host_thp_mode("[always] madvise never\n"), Some("always".into()));
        assert_eq!(host_thp_mode("always [madvise] never"), Some("madvise".into()));
        assert_eq!(host_thp_mode("always madvise never"), None);
    }
}
