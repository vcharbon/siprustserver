//! The nftables ruleset the guard owns: one `inet` table whose output chain
//! drops the node's BFD control (UDP 3784, single-hop) and BGP (TCP 179,
//! either end) toward its routers unless the router's address
//! is in the `let_out` set. Each write replaces the whole table in one
//! transaction, so no packet ever meets a half-written table and a guard
//! restart rebuilds it from scratch.
//!
//! BGP is dropped with BFD: a router whose BFD session went down tears its
//! BGP session down, and without the BGP drop the node's next connect retry
//! would re-establish it and announce the routes again while BFD stays down.

use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::time::Duration;

/// The table's name, in the `inet` family.
pub const TABLE: &str = "route_guard";

/// Single-hop BFD control (RFC 5881). Echo packets carry the node's own
/// address as destination; without control packets the session goes down.
pub const BFD_CONTROL_PORT: u16 = 3784;

/// BGP (RFC 4271).
pub const BGP_PORT: u16 = 179;

/// The table `route-guard --check` writes and deletes: the guard's ruleset
/// under another name, so the check touches no live table.
pub const CHECK_TABLE: &str = "route_guard_check";

/// The script that replaces the guard's table: the routers in `peers`, and,
/// with `let_out`, each of them let out for that long from the commit.
pub fn ruleset(peers: &[Ipv4Addr], let_out: Option<Duration>) -> String {
    ruleset_in(TABLE, peers, let_out)
}

/// The script that deletes `table`, present or not.
pub fn delete(table: &str) -> String {
    format!("table inet {table}\ndelete table inet {table}\n")
}

/// [`ruleset`] under the table `table`.
pub fn ruleset_in(table: &str, peers: &[Ipv4Addr], let_out: Option<Duration>) -> String {
    let list =
        |suffix: &str| peers.iter().map(|p| format!("{p}{suffix}")).collect::<Vec<_>>().join(", ");
    let mut s = String::new();
    // Declaring the table first makes the delete succeed on a first run.
    let _ = writeln!(s, "table inet {table}");
    let _ = writeln!(s, "delete table inet {table}");
    let _ = writeln!(s, "table inet {table} {{");
    let _ = writeln!(s, "\tset peers {{");
    let _ = writeln!(s, "\t\ttype ipv4_addr");
    let _ = writeln!(s, "\t\telements = {{ {} }}", list(""));
    let _ = writeln!(s, "\t}}");
    let _ = writeln!(s, "\tset let_out {{");
    let _ = writeln!(s, "\t\ttype ipv4_addr");
    let _ = writeln!(s, "\t\tflags timeout");
    if let Some(timeout) = let_out {
        let _ = writeln!(
            s,
            "\t\telements = {{ {} }}",
            list(&format!(" timeout {}ms", timeout.as_millis()))
        );
    }
    let _ = writeln!(s, "\t}}");
    let _ = writeln!(s, "\tchain output {{");
    let _ = writeln!(s, "\t\ttype filter hook output priority filter; policy accept;");
    let held = "ip daddr @peers ip daddr != @let_out";
    let _ = writeln!(s, "\t\t{held} udp dport {BFD_CONTROL_PORT} counter drop");
    let _ = writeln!(s, "\t\t{held} tcp dport {BGP_PORT} counter drop");
    let _ = writeln!(s, "\t\t{held} tcp sport {BGP_PORT} counter drop");
    let _ = writeln!(s, "\t}}");
    let _ = writeln!(s, "}}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEERS: [Ipv4Addr; 2] = [Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::new(198, 51, 100, 1)];

    #[test]
    fn a_let_out_ruleset_times_each_router_out_after_the_bound() {
        let s = ruleset(&PEERS, Some(Duration::from_millis(1_000)));
        assert!(s.starts_with(
            "table inet route_guard\ndelete table inet route_guard\ntable inet route_guard {\n"
        ));
        assert!(s.contains("elements = { 192.0.2.1, 198.51.100.1 }\n"), "{s}");
        assert!(
            s.contains("elements = { 192.0.2.1 timeout 1000ms, 198.51.100.1 timeout 1000ms }\n"),
            "{s}"
        );
    }

    #[test]
    fn the_check_writes_the_same_ruleset_under_its_own_table() {
        let check = ruleset_in(CHECK_TABLE, &PEERS, None);
        assert_eq!(check, ruleset(&PEERS, None).replace("route_guard", "route_guard_check"));
        assert_eq!(
            delete(CHECK_TABLE),
            "table inet route_guard_check\ndelete table inet route_guard_check\n"
        );
    }

    #[test]
    fn a_withdrawing_ruleset_lets_no_router_out() {
        let s = ruleset(&PEERS, None);
        assert_eq!(s.matches("elements").count(), 1, "the let_out set is empty: {s}");
        for rule in [
            "ip daddr @peers ip daddr != @let_out udp dport 3784 counter drop\n",
            "ip daddr @peers ip daddr != @let_out tcp dport 179 counter drop\n",
            "ip daddr @peers ip daddr != @let_out tcp sport 179 counter drop\n",
        ] {
            assert!(s.contains(rule), "{rule}: {s}");
        }
    }
}
