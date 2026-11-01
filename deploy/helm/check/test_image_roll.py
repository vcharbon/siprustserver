"""The kind lab's image roll (lib/image-roll.sh): after the SUT release, a
workload whose live pod runs an image other than the one its tag names on the
pod's node is restarted and waited for; nothing else is. A real kubectl reads
the pods and the workloads from a fake API server; `docker exec <node> crictl
inspecti` and `kubectl rollout` are stand-ins."""
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any

from charts import HELM_DIR
from fake_kube import FakeKube

NS = "lab"
OLD = "sha256:" + "a" * 64
NEW = "sha256:" + "b" * 64
OTHER = "sha256:" + "c" * 64

# Stand-ins for lib/cluster.sh's log and die, `kubectl rollout` (recorded) on
# top of the real kubectl, and a `docker exec <node> crictl inspecti` printing
# what the crictl template (the image id, then each repo digest) prints, from
# FAKE_NODES lines "<node> <image> <image id> [repo digest...]"; an unlisted
# pair is an image the node does not hold, another template is unscripted.
HARNESS = """
set -euo pipefail
log() { echo "log: $*"; }
die() { echo "die: $*"; exit 1; }
kubectl() {
  case "$*" in
    *" rollout status "*) echo "kubectl $*" >> "$CALLS"; [ "$FAKE_STATUS_RC" = 0 ] ;;
    *" rollout "*) echo "kubectl $*" >> "$CALLS" ;;
    *) command kubectl --kubeconfig "$FAKE_KUBECONFIG" "$@" ;;
  esac
}
docker() {
  local node="$2" image="${!#}" n i rest
  if [ "$*" != "exec $node crictl inspecti -o go-template --template $CRICTL_TEMPLATE $image" ]; then
    echo "unscripted docker $*" >&2
    return 97
  fi
  while read -r n i rest; do
    if [ "$n $i" = "$node $image" ]; then echo "$rest"; return 0; fi
  done <<< "$FAKE_NODES"
  echo "no such image $image" >&2
  return 1
}
HELM_DIR="$1" NS=lab ROLLOUT_TIMEOUT=7s
CRICTL_TEMPLATE='{{.status.id}}{{range .status.repoDigests}} {{.}}{{end}}'
source "$HELM_DIR/lib/image-roll.sh"
roll_stale_images
"""


def status(name: str, image_id: str) -> dict[str, Any]:
    return {"name": name, "image": "ignored:status", "imageID": image_id, "ready": True}


def pod(name: str, owner: tuple[str, str], node: str, containers: list[tuple[str, str, str]],
        init: list[tuple[str, str, str]] | None = None, deleting: bool = False) -> dict[str, Any]:
    """A pod of `owner` (kind, name) on `node`; each container is
    (name, spec image, status imageID)."""
    meta: dict[str, Any] = {"name": name, "namespace": NS, "ownerReferences": [
        {"apiVersion": "apps/v1", "kind": owner[0], "name": owner[1], "controller": True,
         "uid": "u"}]}
    if deleting:
        meta["deletionTimestamp"] = "2026-01-01T00:00:00Z"
    spec: dict[str, Any] = {"nodeName": node,
                            "containers": [{"name": c, "image": i} for c, i, _ in containers]}
    st: dict[str, Any] = {"phase": "Running",
                          "containerStatuses": [status(c, d) for c, _, d in containers]}
    if init:
        spec["initContainers"] = [{"name": c, "image": i} for c, i, _ in init]
        st["initContainerStatuses"] = [status(c, d) for c, _, d in init]
    return {"apiVersion": "v1", "kind": "Pod", "metadata": meta, "spec": spec, "status": st}


def workload(kind: str, name: str, strategy: dict[str, Any] | None = None,
             owner: tuple[str, str] | None = None) -> dict[str, Any]:
    meta: dict[str, Any] = {"name": name, "namespace": NS}
    if owner:
        meta["ownerReferences"] = [{"apiVersion": "apps/v1", "kind": owner[0], "name": owner[1],
                                    "controller": True, "uid": "u"}]
    spec: dict[str, Any] = {}
    if strategy is not None:
        spec["updateStrategy"] = strategy
    return {"apiVersion": "apps/v1", "kind": kind, "metadata": meta, "spec": spec}


def listing(kind: str, api: str, items: list[dict[str, Any]]) -> dict[str, Any]:
    return {"apiVersion": api, "kind": f"{kind}List", "metadata": {}, "items": items}


ROLLING = {"type": "RollingUpdate", "rollingUpdate": {"partition": 0}}


class Lab:
    """The namespace: pods, ReplicaSets, StatefulSets and DaemonSets."""

    def __init__(self) -> None:
        self.pods: list[dict[str, Any]] = []
        self.replicasets = [workload("ReplicaSet", "proxy-5f", owner=("Deployment", "proxy"))]
        self.statefulsets = [workload("StatefulSet", "worker", ROLLING)]
        self.daemonsets = [workload("DaemonSet", "agent", {"type": "RollingUpdate"})]

    def objects(self) -> dict[str, dict[str, Any]]:
        apps = f"/apis/apps/v1/namespaces/{NS}"
        return {f"/api/v1/namespaces/{NS}/pods": listing("Pod", "v1", self.pods),
                f"{apps}/replicasets": listing("ReplicaSet", "apps/v1", self.replicasets),
                f"{apps}/statefulsets": listing("StatefulSet", "apps/v1", self.statefulsets),
                f"{apps}/daemonsets": listing("DaemonSet", "apps/v1", self.daemonsets)}


class Run:
    def __init__(self, out: subprocess.CompletedProcess[str], calls: list[str]) -> None:
        self.code = out.returncode
        self.out = out.stdout + out.stderr
        self.calls = calls

    def rolled(self) -> list[str]:
        return [c.split()[-1] for c in self.calls if " rollout restart " in c]

    def waited(self) -> list[str]:
        return [c.split(" rollout status ")[1].split()[0] for c in self.calls if " rollout status " in c]


def run(lab: Lab, nodes: str, status_rc: int = 0) -> Run:
    directory = Path(tempfile.mkdtemp(prefix="image-roll-"))
    try:
        calls = directory / "calls"
        calls.touch()
        with FakeKube(lab.objects()) as kube:
            env = {"PATH": os.environ["PATH"], "HOME": str(directory),
                   "FAKE_KUBECONFIG": str(kube.kubeconfig(directory)), "FAKE_NODES": nodes,
                   "FAKE_STATUS_RC": str(status_rc), "CALLS": str(calls)}
            out = subprocess.run(["bash", "-c", HARNESS, "harness", str(HELM_DIR)],
                                 capture_output=True, text=True, env=env, timeout=60)
        return Run(out, calls.read_text().splitlines())
    finally:
        shutil.rmtree(directory)


# Both nodes tag the reloaded app image at NEW (its repo digest names the
# archive it was imported from) and the sidecar image at OTHER.
NODES = "\n".join(f"{n} {i}" for n in ("n1", "n2") for i in (
    f"app:dev {OTHER} docker.io/library/import-2@{NEW}",
    f"side:dev {OTHER} docker.io/library/import-1@{OTHER}"))


def imported(digest: str) -> str:
    return f"docker.io/library/import-1@{digest}"


class ImageRoll(unittest.TestCase):
    def setUp(self) -> None:
        if not shutil.which("kubectl"):
            self.fail("the image roll check needs kubectl on the PATH")
        self.lab = Lab()
        self.lab.pods = [
            pod("proxy-5f-x", ("ReplicaSet", "proxy-5f"), "n1",
                [("side", "side:dev", imported(OTHER)), ("proxy", "app:dev", imported(NEW))]),
            pod("agent-k", ("DaemonSet", "agent"), "n2", [("agent", "side:dev", imported(OTHER))]),
            pod("worker-0", ("StatefulSet", "worker"), "n1", [("worker", "app:dev", imported(NEW))]),
            pod("worker-1", ("StatefulSet", "worker"), "n2", [("worker", "app:dev", imported(NEW))]),
        ]

    def test_every_pod_on_its_tagged_image_rolls_nothing(self) -> None:
        r = run(self.lab, NODES)
        self.assertEqual((r.code, r.calls), (0, []), r.out)
        self.assertIn("nothing rolled", r.out)

    def test_a_reloaded_image_rolls_exactly_the_workloads_whose_pods_run_the_old_one(self) -> None:
        self.lab.pods[2] = pod("worker-0", ("StatefulSet", "worker"), "n1",
                               [("worker", "app:dev", imported(OLD))])
        self.lab.pods[0] = pod("proxy-5f-x", ("ReplicaSet", "proxy-5f"), "n1",
                               [("side", "side:dev", imported(OTHER)),
                                ("proxy", "app:dev", imported(OLD))])
        r = run(self.lab, NODES)
        self.assertEqual(r.code, 0, r.out)
        self.assertEqual(r.rolled(), ["deployment/proxy", "statefulset/worker"])
        self.assertEqual(r.waited(), ["deployment/proxy", "statefulset/worker"])
        self.assertTrue(all("--timeout 7s" in c for c in r.calls if " rollout status " in c), r.calls)
        self.assertIn("rolled deployment/proxy statefulset/worker", r.out)
        self.assertIn("worker-0", r.out)

    def test_restarts_all_come_before_the_first_wait(self) -> None:
        for i, (name, owner) in enumerate([("proxy-5f-x", ("ReplicaSet", "proxy-5f")),
                                           ("agent-k", ("DaemonSet", "agent"))]):
            self.lab.pods[i] = pod(name, owner, "n1", [("c", "app:dev", imported(OLD))])
        r = run(self.lab, NODES)
        self.assertEqual([c.split(" rollout ")[1].split()[0] for c in r.calls], ["restart", "restart", "status", "status"])

    def test_an_init_container_on_the_old_image_rolls_its_workload(self) -> None:
        self.lab.pods[1] = pod("agent-k", ("DaemonSet", "agent"), "n2",
                               [("agent", "side:dev", imported(OTHER))],
                               init=[("prep", "app:dev", imported(OLD))])
        self.assertEqual(run(self.lab, NODES).rolled(), ["daemonset/agent"])

    def test_an_image_without_a_repo_digest_compares_by_its_id(self) -> None:
        nodes = "n1 bare:dev " + NEW
        self.lab.pods = [pod("agent-k", ("DaemonSet", "agent"), "n1", [("a", "bare:dev", NEW)])]
        self.assertEqual(run(self.lab, nodes).calls, [])
        self.lab.pods = [pod("agent-k", ("DaemonSet", "agent"), "n1", [("a", "bare:dev", OLD)])]
        self.assertEqual(run(self.lab, nodes).rolled(), ["daemonset/agent"])

    def test_a_partitioned_statefulset_rolls_only_from_its_partition(self) -> None:
        self.lab.statefulsets = [workload("StatefulSet", "worker",
                                          {"type": "RollingUpdate", "rollingUpdate": {"partition": 1}})]
        self.lab.pods[2] = pod("worker-0", ("StatefulSet", "worker"), "n1",
                               [("worker", "app:dev", imported(OLD))])
        self.assertEqual(run(self.lab, NODES).calls, [])
        self.lab.pods[3] = pod("worker-1", ("StatefulSet", "worker"), "n2",
                               [("worker", "app:dev", imported(OLD))])
        self.assertEqual(run(self.lab, NODES).rolled(), ["statefulset/worker"])

    def test_an_on_delete_workload_is_named_and_left(self) -> None:
        self.lab.statefulsets = [workload("StatefulSet", "worker", {"type": "OnDelete"})]
        self.lab.pods[2] = pod("worker-0", ("StatefulSet", "worker"), "n1",
                               [("worker", "app:dev", imported(OLD))])
        r = run(self.lab, NODES)
        self.assertEqual((r.code, r.calls), (0, []), r.out)
        self.assertIn("statefulset/worker", r.out)
        self.assertIn("OnDelete", r.out)

    def test_terminating_and_unstarted_pods_are_not_read(self) -> None:
        self.lab.pods.append(pod("proxy-4e-y", ("ReplicaSet", "proxy-5f"), "n1",
                                 [("proxy", "app:dev", imported(OLD))], deleting=True))
        self.lab.pods.append(pod("worker-2", ("StatefulSet", "worker"), "n1",
                                 [("worker", "app:dev", "")]))
        self.assertEqual(run(self.lab, NODES).calls, [])

    def test_a_node_that_answers_no_image_for_a_live_pod_fails_the_deploy(self) -> None:
        self.lab.pods[2] = pod("worker-0", ("StatefulSet", "worker"), "n3",
                               [("worker", "app:dev", imported(OLD))])
        r = run(self.lab, NODES)
        self.assertNotEqual(r.code, 0, r.out)
        self.assertEqual(r.calls, [])
        self.assertIn("die: image roll: node n3 answers no app:dev for pod worker-0", r.out)
        self.assertIn("no such image app:dev", r.out)

    def test_a_rollout_that_does_not_finish_fails_the_deploy(self) -> None:
        self.lab.pods[2] = pod("worker-0", ("StatefulSet", "worker"), "n1",
                               [("worker", "app:dev", imported(OLD))])
        r = run(self.lab, NODES, status_rc=1)
        self.assertNotEqual(r.code, 0)
        self.assertIn("die: ", r.out)
        self.assertIn("statefulset/worker", r.out)


if __name__ == "__main__":
    unittest.main()
