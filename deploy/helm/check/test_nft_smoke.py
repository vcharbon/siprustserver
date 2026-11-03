"""The kind lab's nft smoke (lib/smokes.sh), against a stand-in kubectl and
docker: every edge node gets the route guard's check write and the network
tools' write, in the node's network namespace, and a failing write stops the
deploy naming the image and the node."""
import subprocess
import unittest

from charts import HELM_DIR

HARNESS = """
set -euo pipefail
log() { echo "log: $*"; }
die() { echo "die: $*"; exit 1; }
kubectl() {
  case "$*" in
    "get nodes -l tier=edge -o jsonpath={.items[*].metadata.name}") echo "e1 e2" ;;
    *) echo "unscripted kubectl $*" >&2; return 97 ;;
  esac
}
docker() {
  echo "docker $*" >&2
  case "$*" in *"$FAKE_FAIL"*) [ -z "$FAKE_FAIL" ] ;; esac
}
SUT_IMAGE=sut:t NETTOOLS_IMAGE=tools:t
source "$1/lib/smokes.sh"
nft_smoke
"""


def run(fail: str = "") -> subprocess.CompletedProcess[str]:
    return subprocess.run(["bash", "-c", HARNESS, "harness", str(HELM_DIR)],
                          capture_output=True, text=True,
                          env={"PATH": "/usr/bin:/bin", "FAKE_FAIL": fail})


class NftSmoke(unittest.TestCase):
    def test_every_edge_node_takes_both_images_writes(self) -> None:
        out = run()
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        runs = [line for line in out.stderr.splitlines() if line.startswith("docker run")]
        self.assertEqual(len(runs), 4, out.stdout)
        for node in ("e1", "e2"):
            guard = f"docker run --rm --net container:{node} --cap-add NET_ADMIN -e ROUTE_GUARD_PEERS=192.0.2.1 --entrypoint /usr/local/bin/route-guard sut:t --check"
            self.assertIn(guard, runs)
            self.assertTrue(any(f"container:{node}" in r and "tools:t" in r and "nft -f -" in r for r in runs))

    def test_a_failing_write_stops_the_deploy_naming_the_image_and_node(self) -> None:
        out = run(fail="container:e2 --cap-add NET_ADMIN -e")
        self.assertEqual(out.returncode, 1)
        self.assertIn("die: nft-smoke: sut:t's route guard cannot write nftables on e2", out.stdout)


if __name__ == "__main__":
    unittest.main()
