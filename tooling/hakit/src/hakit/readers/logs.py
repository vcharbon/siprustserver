"""`hakit logs`: a window of pod logs from VictoriaLogs, every container incarnation
included.

The log shipper sends every container's output to VictoriaLogs as it is
written, so a force-deleted pod's lines are there after the cluster can no
longer reach them. The output file gets the lines of the pods whose name
matches a regex between two epochs, oldest first:

  <time> <pod> <container> c=<container id, 12> uid=<pod uid> | <message>

The container id comes from the log file the line was read from, so it names
the incarnation that wrote it; the pod uid is the shipper's metadata lookup by
pod name, which is cached and may name another incarnation's pod. The summary
has one line per incarnation.
"""

from __future__ import annotations

import json
import re
import urllib.parse

from ..executor import Executor
from ..jsonval import Json, as_obj

FILE_ID = re.compile(r"-([0-9a-f]{64})\.log$")


def container_id(rec: Json) -> str:
    """The 12-char id of the container that wrote `rec`, from its log file."""
    m = FILE_ID.search(str(rec.get("file", "")))
    return (m.group(1) if m else str(rec.get("kubernetes.docker_id", "?")))[:12]


def format_line(rec: Json) -> str:
    return (
        f"{rec.get('_time', '?')} {rec.get('kubernetes.pod_name', '?')} "
        f"{rec.get('kubernetes.container_name', '?')} c={container_id(rec)} "
        f"uid={rec.get('kubernetes.pod_id', '?')} | {rec.get('_msg', '')}"
    )


def incarnations(recs: list[Json]) -> list[str]:
    """One summary line per (pod, container, container id), in order of first line."""
    seen: dict[tuple[str, str, str], list[str]] = {}
    counts: dict[tuple[str, str, str], int] = {}
    for r in recs:
        k = (str(r.get("kubernetes.pod_name", "?")), str(r.get("kubernetes.container_name", "?")), container_id(r))
        t = str(r.get("_time", "?"))
        if k not in seen:
            seen[k] = [str(r.get("kubernetes.pod_id", "?")), t, t]
            counts[k] = 0
        counts[k] += 1
        seen[k][2] = t
    return [
        f"{pod} {cont} c={cid} uid={uid} lines={counts[(pod, cont, cid)]} first={first} last={last}"
        for (pod, cont, cid), (uid, first, last) in seen.items()
    ]


def query(executor: Executor, vl: str, namespace: str, t0: int, t1: int, pod_regex: str) -> list[Json]:
    """The lines; ValueError when VictoriaLogs did not answer."""
    q = (
        f'kubernetes.namespace_name:="{namespace}" kubernetes.pod_name:~"^({pod_regex})$" | sort by (_time) '
        "| fields _time, file, kubernetes.pod_name, kubernetes.pod_id, kubernetes.container_name, "
        "kubernetes.docker_id, _msg"
    )
    url = f"{vl}/select/logsql/query?{urllib.parse.urlencode({'query': q, 'start': t0, 'end': t1})}"
    res = executor.http("GET", url, timeout_s=60)
    if res.status != 200:
        raise ValueError(f"HTTP {res.status}: {res.body[:200]}")
    return [as_obj(json.loads(line)) for line in res.body.splitlines() if line.strip()]
