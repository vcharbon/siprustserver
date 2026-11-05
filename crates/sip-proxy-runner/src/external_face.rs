//! Whether the external face can carry traffic, read from local kernel state:
//! the external interface is present and up, and at least one of its
//! configured next hops resolved in the neighbour table. The next hops are the
//! external plane's BGP peers, whose BFD and keepalive traffic keeps their
//! entries resolved while the node is let out; while the face is latched
//! unusable the route guard drops that traffic, so each reading solicits the
//! next hops itself. One reading of the interface down or of the next hops
//! unresolved holds a usable face, so a carrier flap withdraws nothing when the
//! first ARP retry after it is answered; a router answering later latches the
//! face until the solicit is answered. `/readyz` requires the face usable, so a
//! node whose caller-facing uplink is gone stops announcing both faces'
//! addresses together (ADR-0056).

mod monitor;
mod neighbours;
mod solicit;

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub use monitor::FaceMonitor;
pub use neighbours::{dump_ipv4, Neighbour, NUD_FAILED, NUD_INCOMPLETE, NUD_RESOLVED};

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
    /// The interface exists and is not up, at the first reading since it read
    /// usable: a carrier flap unless the next reading finds it down too.
    /// Usable.
    DownOnce(String),
    /// No next hop resolved and one `FAILED` or `INCOMPLETE`, at the first
    /// such reading since one resolved on a usable face: a carrier flap leaves
    /// the entry unresolved until an ARP retry is answered, past the flap.
    /// Usable.
    UnresolvedOnce,
    /// The interface does not exist, or its state cannot be read.
    Missing,
    /// The interface exists and is not up.
    Down(String),
    /// No next hop resolved, and one read `FAILED` or `INCOMPLETE` twice since
    /// the last that did, or once on a face that was not usable.
    NextHopsUnresolved,
    /// The neighbour table cannot be read, or no reading has completed yet.
    Unreadable(String),
}

impl FaceState {
    pub fn is_usable(&self) -> bool {
        matches!(self, FaceState::Usable | FaceState::DownOnce(_) | FaceState::UnresolvedOnce)
    }
}

/// What the readings before this one saw.
///
/// `unresolved`: the latch, set as [`FaceState::NextHopsUnresolved`] states,
/// cleared only by a reading that finds a next hop resolved. While set, a
/// reading that finds every next hop absent from the table stays unusable, so
/// an entry that flaps between `INCOMPLETE` and gone never reads usable.
///
/// `unresolved_once`: an [`FaceState::UnresolvedOnce`] reading since a next
/// hop last resolved; an absent reading leaves it standing.
///
/// `usable` and `down_once`: whether the latest reading was usable, and
/// whether it was [`FaceState::DownOnce`].
///
/// `solicit_failed`: a solicit failed to send since a next hop last resolved,
/// so one latch logs one warning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FaceLatch {
    unresolved: bool,
    unresolved_once: bool,
    usable: bool,
    down_once: bool,
    solicit_failed: bool,
}

/// One next hop in the neighbour table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hop {
    /// `REACHABLE`, `STALE`, `DELAY`, `PROBE` or `PERMANENT`.
    Resolved,
    /// `FAILED` or `INCOMPLETE`.
    Unresolved,
    /// No entry, or one in no state above.
    Absent,
}

/// Where the kernel publishes its network interfaces.
pub const SYS_CLASS_NET: &str = "/sys/class/net";

impl FaceCheck {
    /// The face as the running kernel reports it, `latch` carried from the
    /// previous reading; a latched next hop solicited on the wire.
    pub fn read(&self, latch: &mut FaceLatch) -> FaceState {
        self.read_soliciting(solicit::DISCARD_PORT, latch)
    }

    /// [`Self::read`], a latched next hop solicited on `port`.
    fn read_soliciting(&self, port: u16, latch: &mut FaceLatch) -> FaceState {
        let send = |hop| solicit::solicit(hop, port);
        self.read_at(Path::new(SYS_CLASS_NET), dump_ipv4, send, latch)
    }

    /// The face read from the interface directory under `class_net` and the
    /// IPv4 neighbour table `neighbours` returns, `solicit` called for each
    /// next hop while the reading leaves the latch set. Operstate `up` is up;
    /// an interface that reports `unknown` (one that tracks no carrier state)
    /// is up while its carrier reads 1.
    pub fn read_at(
        &self,
        class_net: &Path,
        neighbours: impl FnOnce() -> std::io::Result<Vec<Neighbour>>,
        solicit: impl FnMut(Ipv4Addr) -> std::io::Result<()>,
        latch: &mut FaceLatch,
    ) -> FaceState {
        let state = self.reading(class_net, neighbours, solicit, latch);
        latch.usable = state.is_usable();
        state
    }

    fn reading(
        &self,
        class_net: &Path,
        neighbours: impl FnOnce() -> std::io::Result<Vec<Neighbour>>,
        mut solicit: impl FnMut(Ipv4Addr) -> std::io::Result<()>,
        latch: &mut FaceLatch,
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
            latch.down_once = latch.usable && !latch.down_once;
            return if latch.down_once {
                FaceState::DownOnce(operstate)
            } else {
                FaceState::Down(operstate)
            };
        }
        latch.down_once = false;
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
        let hops: Vec<Hop> = self.next_hops.iter().map(|h| hop(&table, ifindex, *h)).collect();
        if hops.contains(&Hop::Resolved) {
            latch.unresolved = false;
            latch.unresolved_once = false;
            latch.solicit_failed = false;
        } else if hops.contains(&Hop::Unresolved) {
            if latch.usable && !latch.unresolved_once {
                latch.unresolved_once = true;
                return FaceState::UnresolvedOnce;
            }
            latch.unresolved = true;
        }
        if latch.unresolved {
            for hop in &self.next_hops {
                if let Err(e) = solicit(*hop) {
                    if !latch.solicit_failed {
                        tracing::warn!(
                            %hop,
                            error = %e,
                            "next hop solicit not sent: nothing else resolves it while the node \
                             is withdrawn; the node's firewall must let it out"
                        );
                    }
                    latch.solicit_failed = true;
                }
            }
            FaceState::NextHopsUnresolved
        } else {
            FaceState::Usable
        }
    }
}

/// The state of `addr` on `ifindex` in `table`.
fn hop(table: &[Neighbour], ifindex: i32, addr: Ipv4Addr) -> Hop {
    let Some(n) = table.iter().find(|n| n.ifindex == ifindex && n.addr == addr) else {
        return Hop::Absent;
    };
    if n.state & NUD_RESOLVED != 0 {
        Hop::Resolved
    } else if n.state & (NUD_FAILED | NUD_INCOMPLETE) != 0 {
        Hop::Unresolved
    } else {
        Hop::Absent
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
