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

# Shared local-vs-non-local proxy split (the SINGLE source of truth). Sourcing it
# gives us `local_curl` (a curl that ALWAYS bypasses the proxy for 127.0.0.1 health
# checks) and a merged NO_PROXY. The Grafana image BUILD (compose `build:` →
# Dockerfile) needs the host proxy to fetch the plugin from grafana.com, and compose
# reads HTTP_PROXY/HTTPS_PROXY/NO_PROXY from THIS process env — so we export them for
# the build. Grafana's RUNTIME proxy env is blanked in docker-compose.yml, so this
# export never leaks into datasource traffic. proxy-env.sh is safe to source
# standalone under `set -u`. If it is somehow missing, fall back to a no-op
# local_curl so a non-proxied host still works.
if [[ -f "$HERE/../k8s/lib/proxy-env.sh" ]]; then
  # shellcheck disable=SC1091
  source "$HERE/../k8s/lib/proxy-env.sh"
  # Forward the host proxy + merged NO_PROXY to the compose BUILD (grafana plugin
  # fetch). NO_PROXY/no_proxy are already exported by proxy-env.sh.
  HTTP_PROXY="${HTTP_PROXY:-${http_proxy:-}}"
  HTTPS_PROXY="${HTTPS_PROXY:-${https_proxy:-}}"
  http_proxy="$HTTP_PROXY"; https_proxy="$HTTPS_PROXY"
  export HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
else
  local_curl() { curl --noproxy '*' "$@"; }
fi

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
  c_cyan "==> bringing up host stack (docker compose up -d --build)"
  # --build: the grafana service builds a custom image (bakes the VictoriaLogs
  # plugin in at build time, honouring the proxy build-args exported above). Without
  # it, compose would skip the build whenever the tag already exists and the plugin
  # layer would silently go missing.
  (cd "$STACK_DIR" && docker compose up -d --build)
  c_cyan "==> waiting for stack health..."
  # Local health checks MUST bypass the proxy: these are 127.0.0.1 services, but a
  # proxified shell would otherwise route them through squid (issue1) and report
  # bogus DOWN/UP. local_curl forces no-proxy.
  local healthy=0
  for i in {1..30}; do
    if local_curl -fsS http://127.0.0.1:8428/health >/dev/null 2>&1 \
       && local_curl -fsS http://127.0.0.1:9428/health >/dev/null 2>&1 \
       && local_curl -fsS http://127.0.0.1:10428/health >/dev/null 2>&1 \
       && local_curl -fsS http://127.0.0.1:3333/api/health >/dev/null 2>&1; then
      healthy=1
      c_green "stack healthy"
      break
    fi
    sleep 1
  done
  # HARD-FAIL on timeout: a silent fall-through here let --bootstrap "succeed" with a
  # crash-looped Grafana (no :3333 listener). Name the down endpoint(s) and exit 1.
  if [[ "$healthy" -ne 1 ]]; then
    c_red "stack did NOT become healthy within the wait window. Down endpoint(s):"
    for pair in \
        "VictoriaMetrics:http://127.0.0.1:8428/health" \
        "VictoriaLogs:http://127.0.0.1:9428/health" \
        "VictoriaTraces:http://127.0.0.1:10428/health" \
        "Grafana:http://127.0.0.1:3333/api/health"; do
      name="${pair%%:*}"; url="${pair#*:}"
      if ! local_curl -fsS -m 2 "$url" >/dev/null 2>&1; then
        c_red "  - $name ($url)"
      fi
    done
    c_yellow "container state:"
    (cd "$STACK_DIR" && docker compose ps) || true
    c_yellow "hint: check 'docker compose logs grafana' (a crash-loop here usually means a build/plugin or port issue)."
    exit 1
  fi
  apply_addons
  c_green "bootstrap done. Grafana: http://127.0.0.1:3333"
}

# Mode: --apply. Idempotent.
cmd_apply() {
  require docker
  c_cyan "==> reloading VM scrape config (POST /-/reload)"
  local_curl -fsS -X POST http://127.0.0.1:8428/-/reload \
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
  local verbose="${1:-}"
  printf '%-20s %s\n' 'service' 'status'
  printf '%-20s %s\n' '-------' '------'
  # Local probes bypass the proxy (see cmd_bootstrap note) — a proxified shell would
  # otherwise mis-report these 127.0.0.1 endpoints.
  for pair in \
      "VictoriaMetrics:http://127.0.0.1:8428/health" \
      "VictoriaLogs:http://127.0.0.1:9428/health" \
      "VictoriaTraces:http://127.0.0.1:10428/health" \
      "Grafana:http://127.0.0.1:3333/api/health"; do
    name="${pair%%:*}"; url="${pair#*:}"
    if local_curl -fsS -m 2 "$url" >/dev/null 2>&1; then
      printf '%-20s \033[32mUP\033[0m\n' "$name"
    else
      printf '%-20s \033[31mDOWN\033[0m\n' "$name"
    fi
  done

  # --verbose self-check: container state + whether Grafana carries proxy vars at
  # runtime (it MUST NOT) + a local-vs-proxied curl diff on the Grafana port (the
  # proxied probe failing while the local one passes is the expected, healthy shape
  # on a proxified host).
  if [[ "$verbose" == "--verbose" || "$verbose" == "-v" ]]; then
    echo
    c_cyan "containers (docker compose ps):"
    (cd "$STACK_DIR" && docker compose ps) 2>/dev/null || c_yellow "  (compose not available here)"
    echo
    c_cyan "Grafana runtime proxy vars (expect ALL blank):"
    local gname="${GRAFANA_CONTAINER_NAME:-grafanaSipRust}"
    if docker inspect "$gname" >/dev/null 2>&1; then
      # Match only NON-EMPTY values (`=.+`): the runtime blanks these to empty
      # strings, which `env` still prints as `http_proxy=` — without the `.+` we'd
      # report a correctly-blanked var as if the proxy were set.
      docker exec "$gname" env 2>/dev/null \
        | grep -Ei '^(http_proxy|https_proxy|no_proxy)=.+' \
        | sed 's/^/  /' \
        || c_green "  (none set — good)"
    else
      c_yellow "  ($gname not running)"
    fi
    echo
    c_cyan "Grafana local-vs-proxied curl (local should pass; proxied may fail behind a proxy):"
    if local_curl -fsS -m 2 http://127.0.0.1:3333/api/health >/dev/null 2>&1; then
      c_green "  local (no-proxy)  : OK"
    else
      c_red   "  local (no-proxy)  : FAIL"
    fi
    if curl -fsS -m 2 http://127.0.0.1:3333/api/health >/dev/null 2>&1; then
      c_green "  via shell proxy   : OK"
    else
      c_yellow "  via shell proxy   : FAIL (expected on a proxified host — local check is authoritative)"
    fi
  fi

  if kubectl_kind_present; then
    echo
    c_cyan "kind cluster — observability namespace:"
    kubectl -n "$NAMESPACE" get pods 2>/dev/null || c_yellow "namespace $NAMESPACE not found yet"
    echo
    c_cyan "VictoriaMetrics targets (top 20):"
    local_curl -fsS http://127.0.0.1:8428/api/v1/targets 2>/dev/null \
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

  --bootstrap         first install (may fail on rerun, run --down first)
  --apply             idempotent reload of dashboards / scrape / addons
  --down              tear down everything
  --status [-v]       probe endpoints and dump kind-addons pod state;
                      add --verbose / -v for container state + Grafana proxy-var
                      self-check + local-vs-proxied curl diagnostics
EOF
}

case "${1:-}" in
  --bootstrap) cmd_bootstrap ;;
  --apply)     cmd_apply ;;
  --down)      cmd_down ;;
  --status)    cmd_status "${2:-}" ;;
  ""|--help|-h) usage ;;
  *) usage; exit 1 ;;
esac
