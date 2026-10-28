"""The proxy's cookie signing key: drawn once, read back by every upgrade."""
import base64
import shutil
import tempfile
import unittest
from pathlib import Path

from charts import SIPRUST, container, find, kind_values, names, render, secret_key_bytes
from fake_kube import FakeKube

KIND = kind_values(SIPRUST)
SECRET_PATH = "/api/v1/namespaces/sip-test/secrets/sip-front-proxy-hmac"


def b64(raw: bytes) -> str:
    return base64.b64encode(raw).decode()


class CookieKey(unittest.TestCase):
    def test_a_first_install_draws_a_32_byte_key_kept_across_uninstall(self) -> None:
        secret = find(render(SIPRUST, *KIND), "Secret", "sip-front-proxy-hmac")
        self.assertEqual(secret["metadata"]["annotations"]["helm.sh/resource-policy"], "keep")
        self.assertEqual(len(secret_key_bytes(secret)), 32)
        self.assertEqual(base64.b64decode(secret["data"]["kid"]), b"k1")

    def test_an_upgrade_reads_the_live_key_and_kid_back(self) -> None:
        """What `lookup` returns against a cluster that holds the release's
        Secret; a changed kid value is ignored once the Secret exists."""
        live_key = b64(b64(bytes(range(32))).encode())
        live_kid = b64(b"live-kid")
        secret = {"apiVersion": "v1", "kind": "Secret", "type": "Opaque",
                  "metadata": {"name": "sip-front-proxy-hmac", "namespace": "sip-test",
                               "labels": {"app.kubernetes.io/managed-by": "Helm"},
                               "annotations": {"meta.helm.sh/release-name": "rel",
                                               "meta.helm.sh/release-namespace": "sip-test"}},
                  "data": {"kid": live_kid, "key": live_key}}
        directory = Path(tempfile.mkdtemp(prefix="helm-lookup-"))
        self.addCleanup(shutil.rmtree, directory)
        with FakeKube({SECRET_PATH: secret}) as kube:
            docs = render(SIPRUST, *KIND, "--set", "proxy.hmac.kid=k2", "--dry-run=server",
                          "--disable-openapi-validation", "--kubeconfig",
                          str(kube.kubeconfig(directory)))
        self.assertIn(SECRET_PATH, kube.requests)
        rendered = find(docs, "Secret", "sip-front-proxy-hmac")
        self.assertEqual(rendered["data"], {"kid": live_kid, "key": live_key})

    def test_an_existing_secret_is_read_and_none_is_made(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "proxy.hmac.existingSecret=edge-cookie")
        self.assertNotIn(("Secret", "sip-front-proxy-hmac"), names(docs))
        c = container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy")
        refs = {e["name"]: e["valueFrom"]["secretKeyRef"]["name"]
                for e in c["env"] if "valueFrom" in e and "secretKeyRef" in e["valueFrom"]}
        refs.pop("PROXY_CLUSTER_KEY", None)
        self.assertEqual(refs, {"PROXY_HMAC_KID": "edge-cookie", "PROXY_HMAC_KEY": "edge-cookie"})

    def test_the_proxy_reads_the_workers_cluster_key(self) -> None:
        docs = render(SIPRUST, *KIND)
        c = container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy")
        w = find(docs, "StatefulSet", "b2bua-worker")
        worker = container(w, w["spec"]["template"]["spec"]["containers"][0]["name"])
        key = {e["name"]: e["valueFrom"]["secretKeyRef"]
               for e in c["env"] + worker["env"]
               if "valueFrom" in e and "secretKeyRef" in e["valueFrom"]}
        self.assertEqual(key["PROXY_CLUSTER_KEY"], key["B2BUA_CLUSTER_KEY"])


if __name__ == "__main__":
    unittest.main()
