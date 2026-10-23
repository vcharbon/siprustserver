#!/usr/bin/env bash
# The host observability stack (VictoriaMetrics, VictoriaLogs, VictoriaTraces,
# Grafana): docker compose on stack/. The in-cluster collectors are the
# siprust-observability helm release (deploy/helm), not this script's.
#
# Modes:
#   --bootstrap : data dirs, the sipext network, `compose up --build`, then
#                 wait for every endpoint. The `kind` docker network must
#                 exist (kind-lab.sh up creates it).
#   --apply     : reload VictoriaMetrics' scrape config. IDEMPOTENT.
#   --down      : tear the host stack down (its data stays).
#   --status    : probe each endpoint and report what's up.
#
# Required tools: docker (uv when HAKIT_PATH names a product's content).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STACK_DIR="$HERE/stack"
HAKIT_DIR="$(cd "$HERE/../../tooling/hakit" && pwd)"

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

# The compose file attaches victoriametrics to the external `sipext` bridge so
# it can scrape the load-generator containers (sipp UAC exporters :9035 /
# loadgen :9300) that live there — an external network that does not exist
# would abort compose up. Normally kind-lab.sh up creates it; this guard
# covers a standalone --bootstrap before any cluster `up`, using the SAME
# parameters (no-NAT, pinned subnet) from deploy/k8s/lib/net-env.sh when the
# sibling checkout layout is present, else the identical hardcoded defaults.
ensure_sipext_net() {
  if [[ -f "$HERE/../k8s/lib/net-env.sh" ]]; then
    # shellcheck disable=SC1091
    source "$HERE/../k8s/lib/net-env.sh"
  fi
  SIPEXT_NET="${SIPEXT_NET:-sipext}"
  SIPEXT_SUBNET="${SIPEXT_SUBNET:-192.168.60.0/24}"
  SIPEXT_GATEWAY="${SIPEXT_GATEWAY:-192.168.60.1}"
  export SIPEXT_NET   # compose interpolates ${SIPEXT_NET:-sipext}
  if ! docker network inspect "$SIPEXT_NET" >/dev/null 2>&1; then
    c_cyan "==> creating missing sipext bridge $SIPEXT_NET ($SIPEXT_SUBNET, masquerade OFF)"
    docker network create --driver bridge \
      --subnet "$SIPEXT_SUBNET" --gateway "$SIPEXT_GATEWAY" \
      -o com.docker.network.bridge.enable_ip_masquerade=false \
      "$SIPEXT_NET" >/dev/null
  fi
}

# docker compose on the stack. Grafana serves the dashboards/ directory of
# every HA tooling search-path entry in place (ADR-0049 §7.5): the base content
# is in the compose file; when HAKIT_PATH names more entries, `hakit dashboards`
# validates the path and writes the override mounting theirs (gitignored data/).
stack_compose() {
  local files=(-f "$STACK_DIR/docker-compose.yml")
  if [[ -n "${HAKIT_PATH:-}" ]]; then
    require uv
    local override="$STACK_DIR/data/hakit-dashboards.compose.yml"
    uv run --quiet --project "$HAKIT_DIR" --locked hakit dashboards --compose-override "$override" >/dev/null
    files+=(-f "$override")
  fi
  (cd "$STACK_DIR" && docker compose "${files[@]}" "$@")
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
  # sipext must exist BEFORE compose up (external network in the compose file).
  ensure_sipext_net
  c_cyan "==> bringing up host stack (docker compose up -d --build)"
  # --build: the grafana service builds a custom image (bakes the VictoriaLogs
  # plugin in at build time, honouring the proxy build-args exported above). Without
  # it, compose would skip the build whenever the tag already exists and the plugin
  # layer would silently go missing.
  stack_compose up -d --build
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
  c_green "bootstrap done. Grafana: http://127.0.0.1:3333"
}

# Mode: --apply. Idempotent.
cmd_apply() {
  require docker
  c_cyan "==> reloading VM scrape config (POST /-/reload)"
  local_curl -fsS -X POST http://127.0.0.1:8428/-/reload \
    && c_green "VM reloaded" \
    || c_yellow "VM reload returned non-2xx (file watcher may handle it anyway)"
  # The search path's dashboards/ directories are mounted in place and
  # rescanned every 10 s: an edit lands without intervention. A new search-path
  # entry is a new mount, made by --bootstrap.
  c_cyan "==> Grafana dashboards: mounted in place, auto-reload (≤10s)"
  c_green "apply done"
}

cmd_down() {
  require docker
  c_cyan "==> docker compose down"
  stack_compose down
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

  if local_curl -fsS -m 2 http://127.0.0.1:8428/health >/dev/null 2>&1; then
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

  --bootstrap         bring the host stack up (needs the kind network)
  --apply             idempotent reload of the scrape config
  --down              tear the host stack down (its data stays)
  --status [-v]       probe endpoints and list the scrape targets;
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
