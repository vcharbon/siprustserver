#!/usr/bin/env bash
# Observability install script for sipjsserver.
#
# Modes:
#   --bootstrap : bring up host stack + apply kind-addons. NOT idempotent.
#                 Re-runs may fail loudly on existing objects; rerun
#                 --down first if you want a clean slate.
#   --apply     : reload dashboards + scrape config in the live stack
#                 + reapply kind-addons. IDEMPOTENT — safe to rerun any
#                 number of times while iterating on dashboard JSON or
#                 scrape rules.
#   --down      : tear down host stack + delete the observability ns
#                 in kind.
#   --status    : probe each endpoint and report what's up.
#
# Required tools: docker, kubectl. kind cluster must exist for the
# addons step; if not, --bootstrap/--apply skip kind-addons and warn.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STACK_DIR="$HERE/stack"
ADDONS_DIR="$HERE/kind-addons"
KIND_NET="${KIND_NET:-kind}"
NAMESPACE="${OBS_NAMESPACE:-observability}"

c_red()    { printf '\033[31m%s\033[0m\n' "$*"; }
c_green()  { printf '\033[32m%s\033[0m\n' "$*"; }
c_yellow() { printf '\033[33m%s\033[0m\n' "$*"; }
c_cyan()   { printf '\033[36m%s\033[0m\n' "$*"; }

require() {
  command -v "$1" >/dev/null 2>&1 || { c_red "missing required tool: $1"; exit 1; }
}

kind_net_gateway_ip() {
  # IPv4 gateway of the kind docker network — used as the address the
  # in-cluster vmagent/fluent-bit reach the host stack on.
  docker network inspect "$KIND_NET" \
    --format '{{range .IPAM.Config}}{{if (.Gateway | printf "%s" | len) }}{{if not (eq (slice .Gateway 0 4) "fc00")}}{{.Gateway}}{{end}}{{end}}{{end}}' 2>/dev/null \
    | head -1
}

kind_cluster_present() {
  docker network ls --format '{{.Name}}' | grep -qx "$KIND_NET"
}

kubectl_kind_present() {
  command -v kubectl >/dev/null 2>&1 && kubectl get nodes >/dev/null 2>&1
}

# Render the {vmagent,fluent-bit}.yaml manifests with the host IP
# substituted for __HOST_IP__, then apply.
apply_addons() {
  if ! kubectl_kind_present; then
    c_yellow "[skip] kubectl/kind cluster not available — kind-addons not applied"
    return 0
  fi
  local host_ip
  host_ip="$(kind_net_gateway_ip)"
  if [[ -z "$host_ip" ]]; then
    c_red "could not determine kind network gateway IPv4. Is the 'kind' docker network present?"
    return 1
  fi
  c_cyan "applying kind-addons (host IP from cluster's POV: $host_ip)"
  for f in "$ADDONS_DIR"/*.yaml; do
    sed "s|__HOST_IP__|$host_ip|g" "$f" | kubectl apply -f -
  done
}

# Mode: --bootstrap. May fail loudly; that's OK.
cmd_bootstrap() {
  require docker
  # Grafana runs as UID 472 inside the container and writes to its
  # bind-mounted /var/lib/grafana. Pre-create + chown so the very first
  # `docker compose up` doesn't crash-loop. Idempotent.
  mkdir -p "$STACK_DIR/data/grafana" "$STACK_DIR/data/victoriametrics" \
           "$STACK_DIR/data/victorialogs" "$STACK_DIR/data/victoria-traces"
  docker run --rm -v "$STACK_DIR/data/grafana:/data" alpine \
    chown -R 472:472 /data >/dev/null
  c_cyan "==> bringing up host stack (docker compose up -d)"
  (cd "$STACK_DIR" && docker compose up -d)
  c_cyan "==> waiting for stack health..."
  for i in {1..30}; do
    if curl -fsS http://127.0.0.1:8428/health >/dev/null 2>&1 \
       && curl -fsS http://127.0.0.1:9428/health >/dev/null 2>&1 \
       && curl -fsS http://127.0.0.1:10428/health >/dev/null 2>&1 \
       && curl -fsS http://127.0.0.1:3333/api/health >/dev/null 2>&1; then
      c_green "stack healthy"
      break
    fi
    sleep 1
  done
  apply_addons
  c_green "bootstrap done. Grafana: http://127.0.0.1:3333"
}

# Mode: --apply. Idempotent.
cmd_apply() {
  require docker
  c_cyan "==> reloading VM scrape config (POST /-/reload)"
  curl -fsS -X POST http://127.0.0.1:8428/-/reload \
    && c_green "VM reloaded" \
    || c_yellow "VM reload returned non-2xx (file watcher may handle it anyway)"
  # Grafana auto-watches /var/lib/grafana/dashboards every 10s — files
  # are bind-mounted from the repo so edits land without intervention.
  c_cyan "==> Grafana dashboards: bind-mounted, auto-reload (≤10s)"
  apply_addons
  c_green "apply done"
}

cmd_down() {
  require docker
  c_cyan "==> docker compose down"
  (cd "$STACK_DIR" && docker compose down)
  if kubectl_kind_present; then
    c_cyan "==> deleting kind namespace $NAMESPACE"
    kubectl delete namespace "$NAMESPACE" --ignore-not-found --wait=false
  fi
  c_green "down done"
}

cmd_status() {
  printf '%-20s %s\n' 'service' 'status'
  printf '%-20s %s\n' '-------' '------'
  for pair in \
      "VictoriaMetrics:http://127.0.0.1:8428/health" \
      "VictoriaLogs:http://127.0.0.1:9428/health" \
      "VictoriaTraces:http://127.0.0.1:10428/health" \
      "Grafana:http://127.0.0.1:3333/api/health"; do
    name="${pair%%:*}"; url="${pair#*:}"
    if curl -fsS -m 2 "$url" >/dev/null 2>&1; then
      printf '%-20s \033[32mUP\033[0m\n' "$name"
    else
      printf '%-20s \033[31mDOWN\033[0m\n' "$name"
    fi
  done
  if kubectl_kind_present; then
    echo
    c_cyan "kind cluster — observability namespace:"
    kubectl -n "$NAMESPACE" get pods 2>/dev/null || c_yellow "namespace $NAMESPACE not found yet"
    echo
    c_cyan "VictoriaMetrics targets (top 20):"
    curl -fsS http://127.0.0.1:8428/api/v1/targets 2>/dev/null \
      | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    print('  (could not parse /api/v1/targets)'); sys.exit(0)
for t in d.get('data', {}).get('activeTargets', [])[:20]:
    print(f\"  {t['labels'].get('job','?'):<28} {t['scrapeUrl']:<50} {t['health']}\")
" 2>/dev/null || true
  fi
}

usage() {
  cat <<EOF
Usage: $0 [--bootstrap|--apply|--down|--status]

  --bootstrap   first install (may fail on rerun, run --down first)
  --apply       idempotent reload of dashboards / scrape / addons
  --down        tear down everything
  --status      probe endpoints and dump kind-addons pod state
EOF
}

case "${1:-}" in
  --bootstrap) cmd_bootstrap ;;
  --apply)     cmd_apply ;;
  --down)      cmd_down ;;
  --status)    cmd_status ;;
  ""|--help|-h) usage ;;
  *) usage; exit 1 ;;
esac
