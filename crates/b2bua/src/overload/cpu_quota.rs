//! The process's cgroup CPU quota in cores, kept fractional: a 250m limit is
//! `0.25`, where `std::thread::available_parallelism` floors it to `1`. The
//! tightest level from the process's cgroup up to the hierarchy root wins
//! (`cpu.max` on v2, `cpu.cfs_quota_us / cpu.cfs_period_us` on v1), following
//! std's lookup of the same files. `None` when no level sets a quota.

use std::path::{Path, PathBuf};

/// Reads a file's contents; `None` when it is absent or unreadable.
type ReadFile<'a> = &'a dyn Fn(&Path) -> Option<String>;

const V2_MOUNT: &str = "/sys/fs/cgroup";
const V1_MOUNTS: [&str; 2] = ["/sys/fs/cgroup/cpu", "/sys/fs/cgroup/cpu,cpuacct"];

/// The quota of the running process, read from `/proc` and `/sys/fs/cgroup`.
pub(super) fn cpu_quota_cores() -> Option<f64> {
    quota_cores(&|p: &Path| std::fs::read_to_string(p).ok())
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
/// A v1 hierarchy carrying the `cpu` controller wins over the v2 line.
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
        if version == Cgroup::V2 && found.is_some() {
            continue;
        }
        found = Some((version, PathBuf::from(path.trim_start_matches('/'))));
    }
    found
}

/// v2: `cpu.max` at every level from the group to the mount. A group path
/// absent under the mount (a container sharing the host's cgroup namespace,
/// its own group mounted at the root) reads the mount root alone.
fn quota_v2(group: &Path, read: ReadFile) -> Option<f64> {
    let mount = Path::new(V2_MOUNT);
    let leaf = mount.join(group);
    let start = if read(&leaf.join("cgroup.controllers")).is_some() { leaf } else { mount.into() };
    tightest(mount, &start, |dir| parse_cpu_max(&read(&dir.join("cpu.max"))?))
}

/// v1: the two standard `cpu` mounts, then the `cpu` mount in mountinfo with a
/// bind-mounted prefix trimmed off the group path.
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
    let (mount, group) = v1_mountpoint(&read(Path::new("/proc/self/mountinfo"))?, group)?;
    tightest(&mount, &mount.join(group), level)
}

/// The mountinfo `cgroup` mount carrying the `cpu` option whose root contains
/// `group`, and `group` relative to that root.
fn v1_mountpoint(mountinfo: &str, group: &Path) -> Option<(PathBuf, PathBuf)> {
    mountinfo.lines().find_map(|line| {
        let mut items = line.split(' ');
        let root = items.nth(3)?;
        let mount_point = items.next()?;
        let super_opts = items.next_back()?;
        let fs_type = items.nth_back(1)?;
        if fs_type != "cgroup" || !super_opts.split(',').any(|o| o == "cpu") {
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
mod cpu_quota_tests {
    use super::*;
    use std::collections::HashMap;

    fn fs(files: &[(&str, &str)]) -> HashMap<PathBuf, String> {
        files.iter().map(|(p, c)| (PathBuf::from(p), c.to_string())).collect()
    }

    fn quota_of(files: &[(&str, &str)]) -> Option<f64> {
        let fs = fs(files);
        quota_cores(&|p: &Path| fs.get(p).cloned())
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

    /// A host-namespace path absent under the mount reads the root alone.
    #[test]
    fn v2_group_absent_under_the_mount_reads_the_root() {
        let q = quota_of(&[
            ("/proc/self/cgroup", "0::/kubepods/pod1/ctr\n"),
            ("/sys/fs/cgroup/cgroup.controllers", "cpu"),
            ("/sys/fs/cgroup/cpu.max", "50000 100000"),
        ]);
        assert_eq!(q, Some(0.5));
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
