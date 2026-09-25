//! The CPU a tokio runtime may use, in cores: [`CpuBudget`] reads the process's
//! cgroup CPU quota and affinity set, and [`CpuBudget::capacity`] turns them and
//! a worker count into the denominator of the live busy ratio.
//!
//! The quota stays fractional (a 250m limit is `0.25`, where
//! `std::thread::available_parallelism` floors it to `1`) and is the tightest
//! level from the process's cgroup up to the root of the hierarchy it can see
//! (`cpu.max` on v2, `cpu.cfs_quota_us / cpu.cfs_period_us` on v1), following
//! std's lookup of the same files. A quota set on a shared ancestor counts in
//! full for this process, and a limit above the visible root (an outer
//! container's) is not seen.

use std::cell::Cell;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Reads a file's contents; `None` when it is absent or unreadable.
type ReadFile<'a> = &'a dyn Fn(&Path) -> Option<String>;

const V2_MOUNT: &str = "/sys/fs/cgroup";
const V1_MOUNTS: [&str; 2] = ["/sys/fs/cgroup/cpu", "/sys/fs/cgroup/cpu,cpuacct"];

/// What the process may run on, as the kernel reports it at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CpuBudget {
    /// The cgroup CPU quota in cores; `None` when no visible level sets one.
    pub(super) quota: Option<f64>,
    /// CPUs in the process's affinity set; `None` when unreadable.
    pub(super) affinity: Option<usize>,
}

impl CpuBudget {
    /// The running process's budget, read from `/proc` and `/sys/fs/cgroup`.
    /// `None` when a file failed to read for any reason but being absent (fd
    /// exhaustion under load): a partial read would pass for "no quota".
    pub(super) fn read() -> Option<Self> {
        Self::read_checked(&|p: &Path| std::fs::read_to_string(p))
    }

    fn read_checked(read: &dyn Fn(&Path) -> io::Result<String>) -> Option<Self> {
        let failed = Cell::new(false);
        let budget = Self::read_with(&|p: &Path| match read(p) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => {
                failed.set(true);
                None
            }
        });
        (!failed.get()).then_some(budget)
    }

    fn read_with(read: ReadFile) -> Self {
        Self {
            quota: quota_cores(read),
            affinity: read(Path::new("/proc/self/status")).and_then(|s| affinity_count(&s)),
        }
    }

    /// The cores a runtime of `workers` workers may keep busy: the quota when
    /// it is below the worker count and the affinity set gives every worker a
    /// CPU, the worker count otherwise. Workers time-sliced on fewer CPUs
    /// accrue overlapping busy time, which a quota-sized capacity over-reads.
    pub(super) fn capacity(&self, workers: usize) -> f64 {
        let workers = workers.max(1);
        match (self.quota, self.affinity) {
            (Some(q), Some(cpus)) if q < workers as f64 && cpus >= workers => q,
            _ => workers as f64,
        }
    }
}

/// The CPU count of `Cpus_allowed_list:` (`0-3,8,10-11`) in `/proc/self/status`.
fn affinity_count(status: &str) -> Option<usize> {
    let list = status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?.trim();
    list.split(',').try_fold(0usize, |n, range| {
        let (lo, hi) = range.split_once('-').unwrap_or((range, range));
        let (lo, hi): (usize, usize) = (lo.parse().ok()?, hi.parse().ok()?);
        Some(n + hi.checked_sub(lo)? + 1)
    })
}

#[derive(Debug, PartialEq)]
enum Cgroup {
    V1,
    V2,
}

fn quota_cores(read: ReadFile) -> Option<f64> {
    let (version, group) = own_cgroup(&read(Path::new("/proc/self/cgroup"))?)?;
    match version {
        Cgroup::V2 => quota_v2(&group, read),
        Cgroup::V1 => quota_v1(&group, read),
    }
}

/// The process's cgroup path (without its leading `/`) from `/proc/self/cgroup`.
/// A v1 hierarchy carrying the `cpu` controller wins over the v2 line. A path
/// with a `..` component (the process sits outside its cgroup namespace root)
/// names no directory under the mount and is skipped.
fn own_cgroup(proc_cgroup: &str) -> Option<(Cgroup, PathBuf)> {
    let mut found = None;
    for line in proc_cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        let (Some(_), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let version = if controllers.is_empty() {
            Cgroup::V2
        } else if controllers.split(',').any(|c| c == "cpu") {
            Cgroup::V1
        } else {
            continue;
        };
        let path = PathBuf::from(path.trim_start_matches('/'));
        if (version == Cgroup::V2 && found.is_some())
            || path.components().any(|c| c == Component::ParentDir)
        {
            continue;
        }
        found = Some((version, path));
    }
    found
}

/// v2: `cpu.max` at every level from the group to the mount; the standard
/// mount first, then the `cgroup2` mount in mountinfo whose root contains the
/// group (a container's own group mounted at its root).
fn quota_v2(group: &Path, read: ReadFile) -> Option<f64> {
    let level = |dir: &Path| parse_cpu_max(&read(&dir.join("cpu.max"))?);
    let leaf = Path::new(V2_MOUNT).join(group);
    if read(&leaf.join("cgroup.controllers")).is_some() {
        return tightest(Path::new(V2_MOUNT), &leaf, level);
    }
    let (mount, rel) = mountpoint(&read(Path::new("/proc/self/mountinfo"))?, group, "cgroup2")?;
    tightest(&mount, &mount.join(rel), level)
}

/// v1: the two standard `cpu` mounts, then the `cpu` mount in mountinfo whose
/// root contains the group.
fn quota_v1(group: &Path, read: ReadFile) -> Option<f64> {
    let level = |dir: &Path| {
        parse_cfs(&read(&dir.join("cpu.cfs_quota_us"))?, &read(&dir.join("cpu.cfs_period_us"))?)
    };
    for mount in V1_MOUNTS {
        let leaf = Path::new(mount).join(group);
        if read(&leaf.join("cpu.cfs_period_us")).is_some() {
            return tightest(Path::new(mount), &leaf, level);
        }
    }
    let (mount, rel) = mountpoint(&read(Path::new("/proc/self/mountinfo"))?, group, "cgroup")?;
    tightest(&mount, &mount.join(rel), level)
}

/// The mountinfo mount of `fs_type` (a v1 `cgroup` must carry the `cpu`
/// option) whose root contains `group`, and `group` relative to that root.
fn mountpoint(mountinfo: &str, group: &Path, fs_type: &str) -> Option<(PathBuf, PathBuf)> {
    mountinfo.lines().find_map(|line| {
        let mut items = line.split(' ');
        let root = items.nth(3)?;
        let mount_point = items.next()?;
        let super_opts = items.next_back()?;
        if items.nth_back(1)? != fs_type
            || (fs_type == "cgroup" && !super_opts.split(',').any(|o| o == "cpu"))
        {
            return None;
        }
        let rel = group.strip_prefix(root.trim_start_matches('/')).ok()?;
        Some((PathBuf::from(mount_point), rel.to_path_buf()))
    })
}

/// The smallest `level` quota from `start` up to and including `mount`.
fn tightest(mount: &Path, start: &Path, level: impl Fn(&Path) -> Option<f64>) -> Option<f64> {
    start
        .ancestors()
        .take_while(|dir| dir.starts_with(mount))
        .filter_map(level)
        .min_by(f64::total_cmp)
}

/// `cpu.max`: `"<quota> <period>"` in µs, or `"max <period>"` for no quota.
fn parse_cpu_max(s: &str) -> Option<f64> {
    let mut it = s.split_whitespace();
    ratio(it.next()?.parse().ok()?, it.next()?.parse().ok()?)
}

/// v1 `cpu.cfs_quota_us` (`-1` for no quota) over `cpu.cfs_period_us`.
fn parse_cfs(quota: &str, period: &str) -> Option<f64> {
    ratio(quota.trim().parse().ok()?, period.trim().parse().ok()?)
}

fn ratio(quota: i64, period: i64) -> Option<f64> {
    (quota > 0 && period > 0).then(|| quota as f64 / period as f64)
}

#[cfg(test)]
mod cpu_budget_tests {
    use super::*;
    use std::collections::HashMap;

    fn quota_of(files: &[(&str, &str)]) -> Option<f64> {
        let fs: HashMap<PathBuf, String> =
            files.iter().map(|(p, c)| (PathBuf::from(p), c.to_string())).collect();
        quota_cores(&|p: &Path| fs.get(p).cloned())
    }

    fn budget(quota: Option<f64>, affinity: Option<usize>) -> CpuBudget {
        CpuBudget { quota, affinity }
    }

    #[test]
    fn capacity_is_the_quota_only_below_the_workers_with_a_cpu_each() {
        // Sub-core limit, one worker: the fraction of a core.
        assert_eq!(budget(Some(0.25), Some(24)).capacity(1), 0.25);
        // Pool sized past the quota, workers free to run in parallel.
        assert_eq!(budget(Some(1.0), Some(24)).capacity(24), 1.0);
        // Quota above the pool: the workers bound it.
        assert_eq!(budget(Some(8.0), Some(24)).capacity(4), 4.0);
        // Four workers time-sliced on one CPU: busy time overlaps, keep workers.
        assert_eq!(budget(Some(1.0), Some(1)).capacity(4), 4.0);
        // No quota, or affinity unknown.
        assert_eq!(budget(None, Some(24)).capacity(4), 4.0);
        assert_eq!(budget(Some(1.0), None).capacity(4), 4.0);
        // A runtime reporting no worker counts as one.
        assert_eq!(budget(None, None).capacity(0), 1.0);
    }

    #[test]
    fn affinity_counts_cpu_ranges() {
        let status = "Name:\tb2bua\nCpus_allowed:\tff0f\nCpus_allowed_list:\t0-3,8-11,15\n";
        assert_eq!(affinity_count(status), Some(9));
        assert_eq!(affinity_count("Cpus_allowed_list:\t0\n"), Some(1));
        assert_eq!(affinity_count("Cpus_allowed_list:\t3-1\n"), None);
        assert_eq!(affinity_count("Name:\tx\n"), None);
    }

    #[test]
    fn read_with_takes_quota_and_affinity_from_the_kernel_files() {
        let fs: HashMap<PathBuf, String> = [
            ("/proc/self/cgroup", "0::/\n"),
            ("/proc/self/status", "Cpus_allowed_list:\t0-7\n"),
            ("/sys/fs/cgroup/cgroup.controllers", "cpu"),
            ("/sys/fs/cgroup/cpu.max", "50000 100000"),
        ]
        .iter()
        .map(|(p, c)| (PathBuf::from(p), c.to_string()))
        .collect();
        let b = CpuBudget::read_with(&|p: &Path| fs.get(p).cloned());
        assert_eq!(b, budget(Some(0.5), Some(8)));
    }

    /// An absent file is an answer; any other read error voids the whole read.
    #[test]
    fn read_checked_voids_a_read_with_an_error_other_than_absent() {
        let files = |fail: bool| {
            move |p: &Path| -> io::Result<String> {
                match p.to_str() {
                    Some("/proc/self/cgroup") => Ok("0::/\n".into()),
                    Some("/proc/self/status") => Ok("Cpus_allowed_list:\t0-3\n".into()),
                    Some("/sys/fs/cgroup/cgroup.controllers") => Ok("cpu".into()),
                    Some("/sys/fs/cgroup/cpu.max") if fail => {
                        Err(io::Error::from_raw_os_error(24)) // EMFILE
                    }
                    _ => Err(io::ErrorKind::NotFound.into()),
                }
            }
        };
        assert_eq!(CpuBudget::read_checked(&files(false)), Some(budget(None, Some(4))));
        assert_eq!(CpuBudget::read_checked(&files(true)), None);
    }

    #[test]
    fn cpu_max_is_fractional_and_max_means_none() {
        assert_eq!(parse_cpu_max("25000 100000\n"), Some(0.25));
        assert_eq!(parse_cpu_max("150000 100000"), Some(1.5));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("100000 0"), None);
        assert_eq!(parse_cpu_max(""), None);
        assert_eq!(parse_cfs("-1\n", "100000\n"), None);
        assert_eq!(parse_cfs("50000\n", "100000\n"), Some(0.5));
    }

    /// A pod with its own cgroup namespace sees `0::/` and its limit at the mount root.
    #[test]
    fn v2_namespaced_container_reads_the_mount_root() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/\n"),
            ("/sys/fs/cgroup/cgroup.controllers", "cpu memory"),
            ("/sys/fs/cgroup/cpu.max", "25000 100000\n"),
        ]);
        assert_eq!(q, Some(0.25));
    }

    /// The tightest level on the path to the root wins, not the leaf's.
    #[test]
    fn v2_ancestor_quota_tighter_than_the_leaf_wins() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/kubepods/pod1/ctr\n"),
            ("/sys/fs/cgroup/kubepods/pod1/ctr/cgroup.controllers", "cpu"),
            ("/sys/fs/cgroup/kubepods/pod1/ctr/cpu.max", "max 100000"),
            ("/sys/fs/cgroup/kubepods/pod1/cpu.max", "150000 100000"),
            ("/sys/fs/cgroup/kubepods/cpu.max", "400000 100000"),
        ]);
        assert_eq!(q, Some(1.5));
    }

    /// A container sharing the host's cgroup namespace, its own group mounted
    /// at the root: mountinfo names that group as the mount's root.
    #[test]
    fn v2_group_mounted_at_its_root_is_found_in_mountinfo() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/kubepods/pod1/ctr\n"),
            (
                "/proc/self/mountinfo",
                "28 22 0:26 /kubepods/pod1/ctr /sys/fs/cgroup ro,nosuid - cgroup2 cgroup rw\n",
            ),
            ("/sys/fs/cgroup/cpu.max", "50000 100000"),
        ]);
        assert_eq!(q, Some(0.5));
    }

    /// A process outside its namespace root reads no quota, never the root's.
    #[test]
    fn v2_path_above_the_namespace_root_reads_none() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/../../user.slice/x\n"),
            ("/proc/self/mountinfo", "28 22 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n"),
            ("/sys/fs/cgroup/cgroup.controllers", "cpu"),
            ("/sys/fs/cgroup/cpu.max", "50000 100000"),
        ]);
        assert_eq!(q, None);
    }

    #[test]
    fn no_quota_anywhere_is_none() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/user.slice\n"),
            ("/sys/fs/cgroup/user.slice/cgroup.controllers", "cpu"),
            ("/sys/fs/cgroup/user.slice/cpu.max", "max 100000"),
        ]);
        assert_eq!(q, None);
        assert_eq!(quota_of(&[]), None);
    }

    /// A v1 `cpu` line wins over the v2 line of a hybrid host.
    #[test]
    fn v1_cpu_controller_wins_over_v2_and_reads_the_standard_mount() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/\n4:cpu,cpuacct:/docker/abc\n3:memory:/docker/abc\n"),
            ("/sys/fs/cgroup/cpu,cpuacct/docker/abc/cpu.cfs_quota_us", "200000\n"),
            ("/sys/fs/cgroup/cpu,cpuacct/docker/abc/cpu.cfs_period_us", "100000\n"),
            ("/sys/fs/cgroup/cpu,cpuacct/docker/cpu.cfs_quota_us", "-1\n"),
            ("/sys/fs/cgroup/cpu,cpuacct/docker/cpu.cfs_period_us", "100000\n"),
        ]);
        assert_eq!(q, Some(2.0));
    }

    /// A non-standard v1 mount is found in mountinfo, its bind root trimmed.
    #[test]
    fn v1_bind_mount_found_in_mountinfo() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "4:cpu,cpuacct:/docker/abc\n"),
            (
                "/proc/self/mountinfo",
                "30 25 0:26 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n\
                 31 25 0:27 /docker /cg/cpu rw,nosuid - cgroup cgroup rw,cpu,cpuacct\n",
            ),
            ("/cg/cpu/abc/cpu.cfs_quota_us", "75000"),
            ("/cg/cpu/abc/cpu.cfs_period_us", "100000"),
        ]);
        assert_eq!(q, Some(0.75));
    }
}
