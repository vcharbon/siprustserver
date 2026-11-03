# shellcheck shell=bash
# MetalLB in BGP mode with its frr-k8s backend (the chart's default), pinned,
# in its own namespace: the speakers announce the proxy's VIPs to the lab
# router (lib/tor.sh) from the nodes whose proxy is Ready. The chart's
# objects (pools, advertisements, peers, BFD) come with the SUT release
# (proxy.announce.metallb). Sourced after lib/cluster.sh. frr-k8s coalesces
# configuration changes for METALLB_FRR_DEBOUNCE_MS before it reloads FRR (3000
# by default): every route change waits it out, so it is cut to 500, a few
# reload times (~150 ms), still one reload for a burst of changes.
#
# Env: METALLB_VERSION=0.16.1 METALLB_NS=metallb-system METALLB_FRR_DEBOUNCE_MS=500
METALLB_VERSION="${METALLB_VERSION:-0.16.1}"
METALLB_NS="${METALLB_NS:-metallb-system}"
METALLB_FRR_DEBOUNCE_MS="${METALLB_FRR_DEBOUNCE_MS:-500}"

metallb_release() {
  kubectl get namespace "$METALLB_NS" >/dev/null 2>&1 || kubectl create namespace "$METALLB_NS" >/dev/null
  # The speakers run privileged on the host network.
  kubectl label namespace "$METALLB_NS" --overwrite \
    pod-security.kubernetes.io/enforce=privileged pod-security.kubernetes.io/audit=privileged \
    pod-security.kubernetes.io/warn=privileged >/dev/null
  log "helm upgrade --install metallb $METALLB_VERSION ($METALLB_NS, frr-k8s)"
  helm upgrade --install metallb metallb --repo https://metallb.github.io/metallb \
    --version "$METALLB_VERSION" --kube-context "kind-$CLUSTER" -n "$METALLB_NS" \
    --set frrk8s.enabled=true --set frr-k8s.frrk8s.bgpDebounceTimeout="$METALLB_FRR_DEBOUNCE_MS" --wait --timeout "$ROLLOUT_TIMEOUT"
}
