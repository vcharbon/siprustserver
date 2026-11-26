"""The kind lab's SUT release helpers (lib/sut.sh), against a stand-in kubectl:
a namespace another deploy owns is refused, and the route, VIP and isolation
smokes and the proxy's series run only when the release deploys the front
proxy."""
import subprocess
import unittest

from charts import HELM_DIR

# Stand-ins for lib/cluster.sh's log and die, lib/smokes.sh's smokes, and a
# kubectl answering from FAKE_* variables: the worker StatefulSet's owner and
# release, the release of the kept worker Service and cookie key Secret (empty:
# absent), the proxy.
HARNESS = """
set -euo pipefail
log() { echo "log: $*"; }
die() { echo "die: $*"; exit 1; }
route_smoke() { echo "route_smoke"; }
vip_smoke() { echo "vip_smoke"; }
isolation_smoke() { echo "isolation_smoke"; }
policy_smoke() { echo "policy_smoke"; }
kubectl() {
  case "$*" in
    *"get statefulset b2bua-worker -o jsonpath="*managed-by*) printf '%s' "$FAKE_OWNER" ;;
    *"get statefulset b2bua-worker -o jsonpath="*release-name*) printf '%s' "$FAKE_RELEASE" ;;
    *"get statefulset b2bua-worker"*) [ -n "$FAKE_OWNER" ] ;;
    *"get service b2bua-worker -o jsonpath="*release-name*) printf '%s' "$FAKE_SERVICE" ;;
    *"get service b2bua-worker"*) [ -n "$FAKE_SERVICE" ] ;;
    *"get secret sip-front-proxy-hmac -o jsonpath="*release-name*) printf '%s' "$FAKE_SECRET" ;;
    *"get secret sip-front-proxy-hmac"*) [ -n "$FAKE_SECRET" ] ;;
    *"get deploy sip-front-proxy"*) [ "$FAKE_PROXY" = 1 ] ;;
    *) echo "unscripted kubectl $*" >&2; return 97 ;;
  esac
}
HELM_DIR="$1" CLUSTER=lab NS=ns ROLLOUT_TIMEOUT=1s
source "$HELM_DIR/lib/sut.sh"
"$2"
"""


def run(step: str, owner: str = "", release: str = "", proxy: str = "0", sut_release: str = "siprust",
        service: str = "", secret: str = "") -> subprocess.CompletedProcess[str]:
    env = {"PATH": "/usr/bin:/bin", "FAKE_OWNER": owner, "FAKE_RELEASE": release, "FAKE_PROXY": proxy,
           "FAKE_SERVICE": service, "FAKE_SECRET": secret, "SUT_RELEASE": sut_release}
    return subprocess.run(["bash", "-c", HARNESS, "harness", str(HELM_DIR), step],
                          capture_output=True, text=True, env=env)


class Ownership(unittest.TestCase):
    def test_an_empty_namespace_and_the_release_s_own_objects_pass(self) -> None:
        self.assertEqual(run("require_own_release").returncode, 0)
        self.assertEqual(run("require_own_release", "Helm", "siprust").returncode, 0)

    def test_objects_helm_does_not_own_are_refused(self) -> None:
        out = run("require_own_release", "kubectl", "")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("holds a b2bua-worker helm does not own", out.stdout)

    def test_another_release_is_refused_naming_it(self) -> None:
        out = run("require_own_release", "Helm", "siprust", sut_release="product")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("holds release siprust; uninstall it first", out.stdout)

    def test_the_release_s_own_kept_objects_pass(self) -> None:
        out = run("require_own_release", service="siprust", secret="siprust")
        self.assertEqual(out.returncode, 0, out.stdout)

    def test_a_kept_service_of_an_uninstalled_release_is_refused_naming_its_delete(self) -> None:
        out = run("require_own_release", service="siprust", sut_release="product")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("keeps service b2bua-worker of release siprust; delete it first: "
                      "kubectl --context kind-lab -n ns delete service b2bua-worker", out.stdout)

    def test_a_kept_secret_of_an_uninstalled_release_is_refused_naming_its_delete(self) -> None:
        out = run("require_own_release", service="product", secret="siprust", sut_release="product")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("keeps secret sip-front-proxy-hmac of release siprust; delete it first: "
                      "kubectl --context kind-lab -n ns delete secret sip-front-proxy-hmac", out.stdout)


class Smokes(unittest.TestCase):
    def test_a_release_with_the_proxy_runs_every_smoke(self) -> None:
        out = run("sut_smokes", proxy="1")
        self.assertEqual((out.returncode, out.stdout.split()),
                         (0, ["route_smoke", "vip_smoke", "isolation_smoke", "policy_smoke"]))

    def test_a_release_without_the_proxy_skips_them(self) -> None:
        out = run("sut_smokes", proxy="0")
        self.assertEqual(out.returncode, 0)
        self.assertNotIn("vip_smoke", out.stdout)
        self.assertIn("smokes do not apply", out.stdout)


class ObsScope(unittest.TestCase):
    def test_the_series_gate_names_the_proxy_only_when_the_release_runs_one(self) -> None:
        self.assertEqual(run("sut_obs_scope", proxy="1").stdout.strip(), "proxy")
        self.assertEqual(run("sut_obs_scope", proxy="0").stdout.strip(), "sut")


if __name__ == "__main__":
    unittest.main()
