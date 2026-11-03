//! Whether the external face can carry traffic, read from local kernel state
//! only: the external interface is present and up, and at least one of its
//! configured next hops is not `FAILED` in the neighbour table. Nothing is
//! probed on the wire. `/readyz` requires it, so a node whose caller-facing
//! uplink is gone stops announcing both faces' addresses together
//! (ADR-0056).

mod neighbours;

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub use neighbours::{dump_ipv4, Neighbour, NUD_FAILED};

/// The external interface and the next hops the face is reached through
/// (`PROXY_EXT_INTERFACE`, `PROXY_EXT_NEXT_HOPS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceCheck {
    pub interface: String,
    pub next_hops: Vec<Ipv4Addr>,
}

/// One reading of the external face.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaceState {
    Usable,
    /// The interface does not exist, or its state cannot be read.
    Missing,
    /// The interface exists and is not up.
    Down(String),
    /// Every configured next hop is `FAILED`.
    NextHopsFailed,
    /// The neighbour table cannot be read.
    Unreadable(String),
}

impl FaceState {
    pub fn is_usable(&self) -> bool {
        matches!(self, FaceState::Usable)
    }
}

/// Where the kernel publishes its network interfaces.
pub const SYS_CLASS_NET: &str = "/sys/class/net";

impl FaceCheck {
    /// The face as the running kernel reports it.
    pub fn read(&self) -> FaceState {
        self.read_at(Path::new(SYS_CLASS_NET), dump_ipv4)
    }

    /// The face read from the interface directory under `class_net` and the
    /// IPv4 neighbour table `neighbours` returns. Operstate `up` is up; an
    /// interface that reports `unknown` (one that tracks no carrier state) is
    /// up while its carrier reads 1. A next hop absent from the table is not
    /// `FAILED`.
    pub fn read_at(
        &self,
        class_net: &Path,
        neighbours: impl FnOnce() -> std::io::Result<Vec<Neighbour>>,
    ) -> FaceState {
        let dir: PathBuf = class_net.join(&self.interface);
        let Some(operstate) = read_trimmed(&dir.join("operstate")) else {
            return FaceState::Missing;
        };
        let up = match operstate.as_str() {
            "up" => true,
            "unknown" => read_trimmed(&dir.join("carrier")).as_deref() == Some("1"),
            _ => false,
        };
        if !up {
            return FaceState::Down(operstate);
        }
        if self.next_hops.is_empty() {
            return FaceState::Usable;
        }
        let Some(ifindex) = read_trimmed(&dir.join("ifindex")).and_then(|s| s.parse::<i32>().ok())
        else {
            return FaceState::Missing;
        };
        let table = match neighbours() {
            Ok(table) => table,
            Err(e) => return FaceState::Unreadable(e.to_string()),
        };
        let failed = |hop: &Ipv4Addr| {
            table
                .iter()
                .any(|n| n.ifindex == ifindex && n.addr == *hop && n.state & NUD_FAILED != 0)
        };
        if self.next_hops.iter().all(failed) {
            FaceState::NextHopsFailed
        } else {
            FaceState::Usable
        }
    }
}

/// An interface name the check reads under `/sys/class/net`: 1 to 15 bytes,
/// no `/`, no whitespace, not `.` or `..`.
pub fn valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(char::is_whitespace)
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests;
