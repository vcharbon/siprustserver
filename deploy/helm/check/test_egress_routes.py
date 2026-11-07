"""Traffic from the external VIP leaves only through the external interface
(ADR-0056 §11): what the init step writes, the render rules, and the
egress-routes container's turns against a stand-in `ip`."""
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

from charts import SIPRUST, Doc, container, env, find, kind_values, render, render_error

KIND = kind_values(SIPRUST)
SCRIPT = SIPRUST / "files" / "vip-egress-routes.sh"


def script_of(c: Doc) -> str:
    return str(c["args"][0])


def proxy(*args: str) -> Doc:
    return find(render(SIPRUST, *KIND, *args), "Deployment", "sip-front-proxy")


class InitStep(unittest.TestCase):
    def test_a_miss_never_falls_through_and_the_chain_holds_no_address_list(self) -> None:
        c = container(proxy(), "vip-loopback")
        script = script_of(c)
        self.assertIn("ip route replace table 5060 unreachable default metric 4294967295", script)
        self.assertIn('ip rule add priority 5060 from "$EXTERNAL_VIP" lookup 5060', script)
        self.assertIn("-A SIP-EXT-EGRESS -s $EXTERNAL_VIP/32 ! -o $EXTERNAL_INTERFACE -j DROP", script)
        self.assertIn("iptables-restore --noflush", script)
        self.assertEqual(env(c)["EXTERNAL_INTERFACE"], "eth1")
        written = script[script.index("ip route replace table 5060"):script.index("echo \"vip-loopback:")]
        self.assertNotIn("/proc/sys", written)
        for needs_the_interface in ("dev $EXTERNAL_INTERFACE", "ip link"):
            self.assertNotIn(needs_the_interface, written)

    def test_the_correct_rule_goes_in_before_any_other_at_that_priority_goes(self) -> None:
        script = script_of(container(proxy(), "vip-loopback"))
        self.assertLess(script.index("ip rule add priority 5060"),
                        script.index("ip rule del priority 5060"))


class Rendered(unittest.TestCase):
    def test_the_container_has_net_admin_alone_on_the_network_tools_image(self) -> None:
        c = container(proxy(), "egress-routes")
        self.assertEqual(script_of(c).strip(), SCRIPT.read_text().strip())
        self.assertIn("nettools", c["image"])
        self.assertEqual(c["securityContext"]["capabilities"],
                         {"drop": ["ALL"], "add": ["NET_ADMIN"]})
        self.assertEqual(env(c), {"EXTERNAL_INTERFACE": "eth1",
                                  "EXTERNAL_NEXT_HOPS": "192.168.60.1", "PERIOD": "1"})

    def test_the_pod_is_not_ready_while_the_table_holds_no_next_hop(self) -> None:
        probe = container(proxy(), "egress-routes")["readinessProbe"]
        command = probe["exec"]["command"]
        self.assertEqual(command[:2], ["/bin/sh", "-c"])
        self.assertIn("ip -o route show table 5060 default", command[2])
        for table, ready in ((UNREACHABLE, False), (UNREACHABLE + BOTH, True),
                             ("default via 192.0.2.1 dev eth1 \n" + UNREACHABLE, True)):
            read = subprocess.run(["sh", "-c", command[2].replace(
                "ip -o route show table 5060 default", 'printf "%s" "$TABLE"')],
                env={"PATH": os.environ["PATH"], "TABLE": table})
            self.assertEqual(read.returncode == 0, ready, table)

    def test_empty_next_hops_refuse_the_render(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "network.externalNextHops=null",
                           "--set", "proxy.announce.metallb.enabled=false",
                           "--set", "proxy.routeGuard.peers={192.168.60.1}")
        self.assertIn("network.externalNextHops is required", err)

    def test_a_period_at_the_guards_hold_refuses_the_render(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.egressRoutes.periodSeconds=3")
        self.assertIn("proxy.egressRoutes.periodSeconds=3 s must stay below proxy.routeGuard.holdMs=3000", err)
        proxy("--set", "proxy.egressRoutes.periodSeconds=2")

    def test_the_period_is_free_with_the_guard_off(self) -> None:
        proxy("--set", "proxy.egressRoutes.periodSeconds=3", "--set", "proxy.routeGuard.enabled=false")

    def test_the_proxy_states_no_address_plan(self) -> None:
        self.assertNotIn("PROXY_FACE_INT_CIDRS", env(container(proxy(), "sip-front-proxy")))
        err = render_error(SIPRUST, *KIND, "--set", "network.internalCidrs=10.0.0.0/8")
        self.assertIn("internalCidrs", err)


IP = """#!/bin/sh
# Stand-in ip: the table in $STATE/routes, the interface's address in $STATE/addr.
case "$*" in
  "-o route show table 5060 default") cat "$STATE/routes" ;;
  "-o -4 addr show dev eth1 up") cat "$STATE/addr" ;;
  "route replace table 5060 default "*)
    echo "$*" >> "$STATE/writes"
    if [ -e "$STATE/refuse" ]; then echo "Nexthop has invalid gateway" >&2; exit 2; fi
    printf 'default \\\\\\tnexthop via 192.0.2.1 dev eth1 weight 1 \\\\\\tnexthop via 192.0.2.2 dev eth1 weight 1\\n' > "$STATE/routes"
    ;;
  *) echo "unscripted ip $*" >&2; exit 97 ;;
esac
"""

# One turn per call; the turn budget spent, the container is told to stop.
SLEEP = """#!/bin/sh
turns=$(($(cat "$STATE/turns") - 1))
echo "$turns" > "$STATE/turns"
[ "$turns" -gt 0 ] || kill -TERM "$PPID"
"""

UNREACHABLE = "unreachable default metric 4294967295\n"
BOTH = ("default \\\tnexthop via 192.0.2.2 dev eth1 weight 1 "
        "\\\tnexthop via 192.0.2.1 dev eth1 weight 1\n")


class Turns(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.state = Path(self.dir.name)
        for name, text in (("ip", IP), ("sleep", SLEEP)):
            tool = self.state / name
            tool.write_text(text)
            tool.chmod(tool.stat().st_mode | stat.S_IEXEC)

    def run_turns(self, routes: str, address: str, turns: int = 6, refuse: bool = False):
        (self.state / "routes").write_text(routes)
        (self.state / "addr").write_text(address)
        (self.state / "turns").write_text(str(turns))
        if refuse:
            (self.state / "refuse").write_text("")
        out = subprocess.run(
            ["sh", str(SCRIPT)], capture_output=True, text=True, timeout=30,
            env={"PATH": f"{self.state}:{os.environ['PATH']}", "STATE": str(self.state),
                 "EXTERNAL_INTERFACE": "eth1", "EXTERNAL_NEXT_HOPS": "192.0.2.1 192.0.2.2",
                 "PERIOD": "1"})
        writes = self.state / "writes"
        return out, writes.read_text().splitlines() if writes.exists() else []

    def test_the_next_hops_go_in_once(self) -> None:
        out, writes = self.run_turns(UNREACHABLE, "2: eth1 inet 192.0.2.5/24\n")
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(writes, ["route replace table 5060 default nexthop via 192.0.2.1 dev eth1 "
                                  "nexthop via 192.0.2.2 dev eth1"])

    def test_the_same_next_hops_in_another_order_are_not_written(self) -> None:
        out, writes = self.run_turns(UNREACHABLE + BOTH, "2: eth1 inet 192.0.2.5/24\n")
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(writes, [])

    def test_an_interface_down_is_no_failure_and_draws_no_write(self) -> None:
        out, writes = self.run_turns(UNREACHABLE, "", turns=8)
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(writes, [])

    def test_the_third_refused_write_in_a_row_exits(self) -> None:
        out, writes = self.run_turns(UNREACHABLE, "2: eth1 inet 192.0.2.5/24\n", refuse=True)
        self.assertEqual(out.returncode, 1, out.stdout + out.stderr)
        self.assertEqual(len(writes), 3)
        self.assertIn("write 3 failed: Nexthop has invalid gateway", out.stdout)


if __name__ == "__main__":
    unittest.main()
