"""A Kubernetes API server answering what `helm template --dry-run=server`
asks of a chart's render: the version, the discovery of every kind the charts
render (core, apps, rbac), and GETs of the objects it holds (what `lookup`
reads). Every other path is a 404 Status. Plain HTTP on 127.0.0.1, one test."""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

def resource(kind: str, namespaced: bool = True) -> dict[str, Any]:
    plural = kind.lower() + ("es" if kind.endswith("s") else "s")
    return {"name": plural, "singularName": kind.lower(), "namespaced": namespaced, "kind": kind,
            "verbs": ["get", "list", "create", "update", "patch", "delete"]}

GROUPS: dict[str, list[dict[str, Any]]] = {
    "/api/v1": [resource(k) for k in ("Secret", "ConfigMap", "Service", "ServiceAccount")]
    + [resource("Namespace", namespaced=False)],
    "/apis/apps/v1": [resource(k) for k in ("Deployment", "StatefulSet", "DaemonSet")],
    "/apis/rbac.authorization.k8s.io/v1": [resource("Role"), resource("RoleBinding"),
                                           resource("ClusterRole", namespaced=False),
                                           resource("ClusterRoleBinding", namespaced=False)],
}

class FakeKube:
    """`objects` maps an API path (`/api/v1/namespaces/<ns>/secrets/<name>`)
    to the object a GET returns."""

    def __init__(self, objects: dict[str, dict[str, Any]]) -> None:
        self.objects = objects
        self.requests: list[str] = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: Any) -> None:
                return

            def do_GET(self) -> None:
                path = self.path.split("?", 1)[0]
                fake.requests.append(path)
                body = fake.answer(path)
                code = 404 if body.get("kind") == "Status" else 200
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def answer(self, path: str) -> dict[str, Any]:
        if path == "/version":
            return {"major": "1", "minor": "33", "gitVersion": "v1.33.0"}
        if path == "/api":
            return {"kind": "APIVersions", "versions": ["v1"]}
        if path == "/apis":
            groups = [g.removeprefix("/apis/").rsplit("/", 1)[0] for g in GROUPS if g != "/api/v1"]
            return {"kind": "APIGroupList", "apiVersion": "v1", "groups": [
                {"name": g, "versions": [{"groupVersion": f"{g}/v1", "version": "v1"}],
                 "preferredVersion": {"groupVersion": f"{g}/v1", "version": "v1"}} for g in groups]}
        if path in GROUPS:
            gv = "v1" if path == "/api/v1" else path.removeprefix("/apis/")
            return {"kind": "APIResourceList", "groupVersion": gv, "resources": GROUPS[path]}
        if path in self.objects:
            return self.objects[path]
        return {"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound",
                "code": 404, "message": f"{path} not found"}

    def kubeconfig(self, directory: Path) -> Path:
        host, port = self.server.server_address[:2]
        path = directory / "kubeconfig"
        path.write_text(json.dumps({
            "apiVersion": "v1", "kind": "Config", "current-context": "fake",
            "clusters": [{"name": "fake", "cluster": {"server": f"http://{host}:{port}"}}],
            "users": [{"name": "fake", "user": {"token": "fake"}}],
            "contexts": [{"name": "fake", "context": {"cluster": "fake", "user": "fake"}}],
        }))
        return path

    def __enter__(self) -> "FakeKube":
        self.thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.server.shutdown()
        self.server.server_close()
