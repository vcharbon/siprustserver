"""The kind lab's SUT release helpers (lib/sut.sh), against a stand-in kubectl:
a namespace another deploy owns is refused, and the VIP and isolation smokes
run only when the release deploys the front proxy."""
import subprocess
import unittest

from charts import HELM_DIR

# Stand-ins for run.sh's log, die and smokes, and a kubectl answering from
# FAKE_* variables: the worker StatefulSet's owner and release, the proxy.
HARNESS = """
set -euo pipefail
log() { echo "log: $*"; }
die() { echo "die: $*"; exit 1; }
vip_smoke() { echo "vip_smoke"; }
isolation_smoke() { echo "isolation_smoke"; }
kubectl() {
  case "$*" in
    *"get statefulset b2bua-worker -o jsonpath="*managed-by*) printf '%s' "$FAKE_OWNER" ;;
    *"get statefulset b2bua-worker -o jsonpath="*release-name*) printf '%s' "$FAKE_RELEASE" ;;
    *"get statefulset b2bua-worker"*) [ -n "$FAKE_OWNER" ] ;;
    *"get deploy sip-front-proxy"*) [ "$FAKE_PROXY" = 1 ] ;;
    *) echo "unscripted kubectl $*" >&2; return 97 ;;
  esac
}
HELM_DIR="$1" CLUSTER=lab NS=ns ROLLOUT_TIMEOUT=1s
source "$HELM_DIR/lib/sut.sh"
"$2"
"""


def run(step: str, owner: str = "", release: str = "", proxy: str = "0", sut_release: str = "siprust") -> subprocess.CompletedProcess[str]:
    env = {"PATH": "/usr/bin:/bin", "FAKE_OWNER": owner, "FAKE_RELEASE": release, "FAKE_PROXY": proxy,
           "SUT_RELEASE": sut_release}
    return subprocess.run(["bash", "-c", HARNESS, "harness", str(HELM_DIR), step],
                          capture_output=True, text=True, env=env)


class Ownership(unittest.TestCase):
    def test_an_empty_namespace_and_the_release_s_own_objects_pass(self) -> None:
        self.assertEqual(run("require_no_raw_deploy").returncode, 0)
        self.assertEqual(run("require_no_raw_deploy", "Helm", "siprust").returncode, 0)

    def test_a_raw_deploy_is_refused(self) -> None:
        out = run("require_no_raw_deploy", "kubectl", "")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("holds a run.sh deploy", out.stdout)

    def test_another_release_is_refused_naming_it(self) -> None:
        out = run("require_no_raw_deploy", "Helm", "siprust", sut_release="product")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("holds release siprust; uninstall it first", out.stdout)


class Smokes(unittest.TestCase):
    def test_a_release_with_the_proxy_runs_both_smokes(self) -> None:
        out = run("sut_smokes", proxy="1")
        self.assertEqual((out.returncode, out.stdout.split()), (0, ["vip_smoke", "isolation_smoke"]))

    def test_a_release_without_the_proxy_skips_them(self) -> None:
        out = run("sut_smokes", proxy="0")
        self.assertEqual(out.returncode, 0)
        self.assertNotIn("vip_smoke", out.stdout)
        self.assertIn("smokes do not apply", out.stdout)


if __name__ == "__main__":
    unittest.main()
