"""A loadgen stream placed in the cluster: one pod in the lab's namespace, for
a system under test the generator network cannot reach (ClusterIP Services).

It keeps the contract of the docker stream (`loadgen.py`): the same binary
and flags, one call-outcome record per call shipped to VictoriaLogs (as pods
reach it), the drain by `POST /drain` and the rate by `POST /rate`, both
through the API server's pod proxy, and the counters of the `metrics.prom`
the drained binary leaves. The pod's `gen` container is the binary, bound to
the pod's own address; its `keeper` container (a POSIX shell of the same
image) outlives it, so the stream directory the two share (`/stats`) is read
after the binary exited. Before the pod is removed every file of `/stats` and
the binary's log are copied into the stream directory on the host. The pod is
annotated for the in-cluster scrape, so its series reach VictoriaMetrics with
its `stream` and `shape` labels.
"""

from __future__ import annotations

import json
from pathlib import Path

from .executor import Executor, ProcResult
from .generator import Tally
from .jsonval import Json, as_list, as_obj
from .lab import Lab, resolve_target
from .loadgen import FINAL_METRICS, METRICS_PORT, LoadgenStream, parse_metrics
from .profile import Stream

STATS = "/stats"
# The pod's own address, set by the kubelet; `$(POD_IP)` in an argument expands to it.
POD_IP = "$(POD_IP)"
KEEPER = "trap 'exit 0' TERM; while :; do sleep 3600 & wait $!; done"
READY_TIMEOUT_S = 180


def pod_name(stream: str, tag: str) -> str:
    """A DNS-1123 name: `hakit-<stream><tag>`, `_` spelled `-`, at most 63."""
    return f"hakit-{stream}{tag}".replace("_", "-").lower()[:63].rstrip("-")


class PodLoadgenStream:
    def __init__(
        self,
        run: str,
        stream: Stream,
        slot: int,
        stream_dir: Path,
        lab: Lab,
        executor: Executor,
        call_id_tag: str = "",
    ) -> None:
        self.inner = LoadgenStream(run, stream, slot, stream_dir, lab, executor, call_id_tag)
        self.run = run
        self.stream = stream
        self.call_id_tag = call_id_tag
        self.dir = stream_dir
        self.lab = lab
        self.executor = executor
        self.name = pod_name(stream.name, call_id_tag)
        # Set once `/stats` was copied out: the files are read from the host then.
        self.copied = False

    @property
    def records_path(self) -> Path:
        return self.dir / "records.jsonl"

    def max_concurrent(self) -> int:
        return self.inner.max_concurrent()

    def _kubectl(self, *args: str, timeout_s: float = 60) -> ProcResult:
        return self.executor.run(
            ["kubectl", "--context", self.lab.kube_context, "-n", self.lab.namespace, *args], timeout_s=timeout_s
        )

    def manifest(self, target: str) -> Json:
        argv = self.inner.argv(target=target, bind_ip=POD_IP, records_url=self.lab.vlogs_url_from_pods)
        image = str(self.stream.params["image"])
        mount = [{"name": "stats", "mountPath": STATS}]
        selector = dict(kv.split("=", 1) for kv in self.lab.pod_node_selector.split(",") if "=" in kv)
        spec: Json = {
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 5,
            "containers": [
                {
                    "name": "gen",
                    "image": image,
                    "imagePullPolicy": "IfNotPresent",
                    "command": argv[:1],
                    "args": argv[1:],
                    "env": [{"name": "POD_IP", "valueFrom": {"fieldRef": {"fieldPath": "status.podIP"}}}],
                    # The scrape keeps the declared port the annotation names.
                    "ports": [{"name": "metrics", "containerPort": METRICS_PORT}],
                    "volumeMounts": mount,
                    "resources": {"limits": {"cpu": "4", "memory": "1536Mi"}},
                },
                {
                    "name": "keeper",
                    "image": image,
                    "imagePullPolicy": "IfNotPresent",
                    "command": ["sh", "-c", KEEPER],
                    "volumeMounts": mount,
                    "resources": {"requests": {"cpu": "10m", "memory": "8Mi"}},
                },
            ],
            # In memory, charged to the pod: no write waits on the node's disk.
            "volumes": [{"name": "stats", "emptyDir": {"medium": "Memory", "sizeLimit": "512Mi"}}],
        }
        if selector:
            spec["nodeSelector"] = selector
        return {
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": self.name,
                "namespace": self.lab.namespace,
                "labels": {"hakit-run": self.run, "hakit-stream": self.stream.name},
                "annotations": {"prometheus.io/scrape": "true", "prometheus.io/port": str(METRICS_PORT)},
            },
            "spec": spec,
        }

    def start(self) -> str | None:
        target, err = resolve_target(self.lab, self.executor, self.stream.target)
        if target is None:
            return f"loadgen pod of {self.stream.name} did not start: {err}"
        self._kubectl("delete", "pod", self.name, "--grace-period=0", "--force", "--ignore-not-found", timeout_s=120)
        path = self.dir / "pod.json"
        path.write_text(json.dumps(self.manifest(target), indent=1) + "\n")
        res = self._kubectl("create", "-f", str(path))
        if res.rc != 0:
            return f"loadgen pod of {self.stream.name} was refused: {res.stderr.strip()[:300]}"
        ready = self._kubectl(
            "wait", "--for=condition=Ready", f"pod/{self.name}", f"--timeout={READY_TIMEOUT_S}s", timeout_s=READY_TIMEOUT_S + 30
        )
        if ready.rc != 0:
            return f"loadgen pod of {self.stream.name} not ready in {READY_TIMEOUT_S} s: {ready.stderr.strip()[:300]}"
        return None

    def _gen_state(self) -> Json | None:
        """The `gen` container's state; `{"gone": {}}` when the pod no longer
        exists; None when it could not be read."""
        res = self._kubectl("get", "pod", self.name, "-o", "json", timeout_s=30)
        if res.rc != 0:
            return {"gone": {}} if "NotFound" in res.stderr else None
        try:
            status = as_obj(as_obj(json.loads(res.stdout)).get("status", {}))
        except ValueError:
            return None
        for c in as_list(status.get("containerStatuses", [])):
            cs = as_obj(c)
            if cs.get("name") == "gen":
                return as_obj(cs.get("state", {}))
        return None

    def running(self) -> bool:
        """False only once the binary is known to have stopped (terminated, or
        its pod gone): a state the API server did not answer is read again at
        the next tick, never taken for a dead generator."""
        state = self._gen_state()
        return state is None or not ("terminated" in state or "gone" in state)

    def exit_code(self) -> int | None:
        state = self._gen_state()
        done = None if state is None else state.get("terminated")
        code = None if done is None else as_obj(done).get("exitCode")
        return code if isinstance(code, int) else None

    def clean_exit(self, code: int | None) -> bool:
        """A drained loadgen exits 0 once its records are sealed."""
        return code == 0

    def _post(self, path: str) -> bool:
        """POST to the binary's HTTP endpoint through the API server's pod proxy."""
        empty = self.dir / "empty.body"
        empty.write_text("")
        raw = f"/api/v1/namespaces/{self.lab.namespace}/pods/{self.name}:{METRICS_PORT}/proxy/{path}"
        return self._kubectl("create", "--raw", raw, "-f", str(empty), timeout_s=30).rc == 0

    def drain(self) -> bool:
        return self._post("drain")

    def set_rate(self, cps: float) -> bool:
        return self._post(f"rate?cps={cps:g}")

    def _read(self, name: str) -> str | None:
        if self.copied:
            return self.executor.read_text(self.dir / name)
        res = self._kubectl("exec", self.name, "-c", "keeper", "--", "cat", f"{STATS}/{name}", timeout_s=120)
        return res.stdout if res.rc == 0 else None

    def tally(self) -> Tally:
        """The counters of the last `/metrics` body, left in `metrics.prom` as
        loadgen exits; the live endpoint through the pod proxy while it runs."""
        text = self._read(FINAL_METRICS)
        if text is None and not self.copied:
            raw = f"/api/v1/namespaces/{self.lab.namespace}/pods/{self.name}:{METRICS_PORT}/proxy/metrics"
            res = self._kubectl("get", "--raw", raw, timeout_s=30)
            text = res.stdout if res.rc == 0 else ""
        return parse_metrics(text or "", self.stream.name)

    def local_records(self) -> int:
        text = self._read("records.jsonl") or ""
        return sum(1 for ln in text.splitlines() if ln.strip())

    def copy_out(self) -> None:
        """Every file of `/stats` and the binary's log into the stream directory."""
        if self.copied:
            return
        listing = self._kubectl("exec", self.name, "-c", "keeper", "--", "ls", "-1", STATS, timeout_s=30)
        if listing.rc != 0:
            return
        for name in listing.stdout.split():
            body = self._kubectl("exec", self.name, "-c", "keeper", "--", "cat", f"{STATS}/{name}", timeout_s=120)
            if body.rc == 0:
                (self.dir / name).write_text(body.stdout)
        log = self._kubectl("logs", self.name, "-c", "gen", timeout_s=120)
        if log.rc == 0:
            (self.dir / "gen.log").write_text(log.stdout)
        self.copied = True

    def kill(self) -> bool:
        """The binary's calls are abandoned: its files are copied out, then the pod goes."""
        self.copy_out()
        res = self._kubectl("delete", "pod", self.name, "--grace-period=0", "--force", "--wait=false", timeout_s=60)
        return res.rc == 0

    def remove(self) -> None:
        self.copy_out()
        self._kubectl("delete", "pod", self.name, "--grace-period=0", "--force", "--ignore-not-found", timeout_s=120)
