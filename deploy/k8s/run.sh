#!/usr/bin/env bash
# Thin k8s endurance/load runner for the containerized Rust SIP SUT.
#
#   sipp UAC (docker, sipext bridge) -> sip-front-proxy (tier=edge, dual-faced,
#   LB+HMAC stickiness) -> b2bua-worker pool (tier=app) -> sipp UAS (docker,
#   sipext bridge). All generators are plain docker containers on the no-NAT
#   $SIPEXT_NET bridge (sipext dual-plane layout — tier=load kind nodes GONE);
#   they dial the EXTERNAL VIP ${SIPEXT_TARGET}:${SIP_PORT}.
#
# Deliberately minimal: deploy-all, run SIPp at a list of CAPS, sample per-pod
# CPU% + RSS from /proc, tear down. As of S11 the cluster topology
# (./cluster.yaml), the SIPp build context (./sipp/Dockerfile) and the SIPp
# scenarios (./sipp/scenarios/) are VENDORED COPIES (no longer symlinks into the
# sibling sipjsserver checkout) so the Rust SUT runner stands alone and the two
# can diverge — especially the endurance/chaos scenarios. Reuses the SAME kind
# cluster name (`sip-e2e`).
#
# >>> WSL ONE-CLUSTER CONSTRAINT <<<
# Only one kind cluster runs at a time on this host. `up` is NON-DESTRUCTIVE: if a
# `sip-e2e` cluster already exists it REFUSES rather than wiping it, so a cluster
# left behind by a failed run survives for analysis. Destruction is explicit —
# `down` — and `up` never auto-tears-down on failure either. To reclaim the host
# for the other SUT (the old one-shot "stop the other, run this" switch), run
# `./run.sh down` first, or `FORCE_RECREATE=1 ./run.sh up` to delete+recreate.
#
# Usage:
#   ./run.sh up                      # (re)create cluster + build/load images + observability
#   ./run.sh deploy                  # apply uas + workers + proxy, wait ready
#   ./run.sh obs                     # (re)deploy observability stack only (idempotent)
#   ./run.sh caps 200 30             # 200 cps for 30s, sample CPU/mem
#   ./run.sh sweep 30 50 100 200 400 # run a list of caps, 30s sampling each
#   ./run.sh all 30 50 100 200 400   # up + deploy + sweep (leaves cluster up)
#   ./run.sh heal-kindnet            # restart any kindnetd stuck in a watch hot-loop
#   ./run.sh down                    # delete the cluster (the ONLY destroy)
#   FORCE_RECREATE=1 ./run.sh up     # delete any existing cluster, then recreate
#
# >>> SOURCEABLE LIBRARY (issue 025) <<<
# This file doubles as a function library: all logic lives in functions and the
# subcommand dispatch is run_main(), executed ONLY when the script is run
# directly. A downstream overlay (living in a DIFFERENT directory) can
#     SUT_IMAGE=... EXTRA_IMAGE_BUILDS=... MANIFEST_DIR=...  # knobs BEFORE source
#     source /path/to/deploy/k8s/run.sh                      # executes nothing
# then call or override the functions (up, deploy, apply_manifest,
# subst_manifest, build_load_extra_images, ...) or dispatch via `run_main up`.
# Sourcing has NO side effects beyond variable defaults + function definitions
# (no cd, no docker/kubectl, no dispatch). Overlay knobs (all honoured on direct
# invocation too; defaults reproduce the historical behaviour exactly):
#   SUT_IMAGE           SUT image tag `up` builds + kind-loads (siprustserver:dev)
#   EXTRA_IMAGE_BUILDS  whitespace-separated EXTRA images `up` additionally
#                       builds + kind-loads, each entry
#                       name=dockerfile:context[:build-args] with build-args a
#                       comma-separated KEY=VAL list forwarded as --build-arg.
#                       Use absolute paths. Default: empty (no-op).
#   MANIFEST_DIR        directory the numbered manifests are applied from
#                       (default: this checkout's deploy/k8s/manifests)
#   CLUSTER_CONFIG      kind topology file (default: deploy/k8s/cluster.yaml)
#   SIPP_DIR            vendored SIPp build context (default: deploy/k8s/sipp)
#   SCENARIOS           SIPp scenario dir (default: $SIPP_DIR/scenarios)
#
# Observability (VictoriaMetrics + Grafana + vmagent/KSM/node-exporter/fluent-bit)
# is brought up automatically by `up` via deploy/observability/install.sh and
# survives across runs (host docker-compose); the in-cluster scrapers are
# re-applied on every `up` since recreating the cluster wipes them. Grafana:
# http://localhost:3333 (anonymous admin). Set OBS_ENABLE=0 to skip.
#
# Env: SUT_IMAGE=siprustserver:dev  WORKER_REPLICAS=2  CLUSTER=sip-e2e  NS=sip-test
#      OBS_ENABLE=1
set -euo pipefail
# Resolve our own directory WITHOUT a top-level `cd` (a source-time cd would leak
# into any script sourcing this library — issue 025); every path below that used
# to be cwd-relative is now anchored on $K8S_DIR instead.
K8S_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$K8S_DIR/../.." && pwd)"

# Network/port wiring (SIP_SUBNET/SIP_GATEWAY/PROXY_VIP/PROXY_TARGET/SIP_PORT) and
# host prerequisite checks (cgroups/sysctls) live in shared, env-overridable libs
# so run.sh, endurance.sh and chaos.sh stay in lock-step.
source "$K8S_DIR/lib/net-env.sh"
# Shared docker-on-sipext generator launcher (UAC stream + stat exporter as
# labelled, IP-pinned, resource-capped containers) — the single implementation
# caps()/sweep() here, endurance.sh and chaos.sh all compose. Sourced AFTER
# net-env.sh (needs SIPEXT_NET/SIPEXT_TARGET/SIPEXT_UAC_BASE_IP).
source "$K8S_DIR/lib/sipext-gen.sh"
# HTTP(S)-proxy wiring for PROXIFIED hosts: forwards the host proxy into docker
# builds (external mirrors/apt/git) while keeping cluster-local + 127 traffic OFF
# the proxy via a merged NO_PROXY. Sourced AFTER net-env.sh (needs SIP_SUBNET/
# SIP_GATEWAY/PROXY_VIP). See lib/proxy-env.sh for the local-vs-non-local split.
source "$K8S_DIR/lib/proxy-env.sh"
source "$K8S_DIR/lib/host-checks.sh"
# Pins every `kubectl` in this script to the kind cluster's own context (see lib)
# — must be sourced AFTER CLUSTER is known to be overridable, but the wrapper
# reads CLUSTER at call time so order is not load-bearing.
source "$K8S_DIR/lib/kube-env.sh"

CLUSTER="${CLUSTER:-sip-e2e}"
NS="${NS:-sip-test}"
# Bring-up waits, env-overridable. Defaults bumped from the historical 120s: on a
# loaded WSL2 host node-ready + image build/load + first rollout regularly need
# more, and a too-short wait used to abort the whole run.
KIND_WAIT="${KIND_WAIT:-300s}"
ROLLOUT_TIMEOUT="${ROLLOUT_TIMEOUT:-300s}"
SUT_IMAGE="${SUT_IMAGE:-siprustserver:dev}"
WORKER_REPLICAS="${WORKER_REPLICAS:-2}"
RESULTS="${RESULTS:-$K8S_DIR/results}"
SIPP_DIR="${SIPP_DIR:-$K8S_DIR/sipp}"       # vendored sipp build context + scenarios
SCENARIOS="${SCENARIOS:-$SIPP_DIR/scenarios}"
# kind cluster topology + the numbered manifests. Both overridable so a
# downstream overlay can point at its own copies; the manifests keep their
# UPSTREAM apply order/logic in deploy() below — an overlay with a different
# stack overrides deploy() and composes apply_manifest/subst_manifest itself.
CLUSTER_CONFIG="${CLUSTER_CONFIG:-$K8S_DIR/cluster.yaml}"
MANIFEST_DIR="${MANIFEST_DIR:-$K8S_DIR/manifests}"
# Extra images `up` builds + kind-loads AFTER the standard set — the downstream
# overlay hook (empty default = no-op). Format: see build_load_extra_images().
EXTRA_IMAGE_BUILDS="${EXTRA_IMAGE_BUILDS:-}"
# Replication: off by default for the plain load/endurance sweep (the chaos
# suite — chaos.sh — sets REPL_ENABLE=1). REPL_PORT is the cluster-wide repl TCP
# port (peer.host:REPL_PORT), templated into the worker manifest + headless svc.
REPL_ENABLE="${REPL_ENABLE:-0}"
REPL_PORT="${REPL_PORT:-9092}"
SCENARIO="${SCENARIO:-uac-basic.xml}"   # UAC scenario the load sweep drives
# Front-proxy HA (ADR-0012 D7): the proxy sits behind a keepalived VRRP VIP pair
# (PROXY_VIP internal + SIPEXT_VIP external) on SIP_PORT — all from lib/net-env.sh
# and stamped into the proxy/worker manifests by envsubst. Callers (the docker
# UAC streams) dial SIPEXT_TARGET (= the external VIP); PROXY_TARGET stays the
# INTERNAL VIP for in-cluster consumers (workers' outbound proxy). LIMITER_CAP
# feeds the -key xapi JSON in the UAC streams (default 20, lib/sipext-gen.sh).
LIMITER_CAP="${LIMITER_CAP:-20}"
KEEPALIVED_IMAGE="${KEEPALIVED_IMAGE:-siprustserver-keepalived:dev}"
# CDR transport: RabbitMQ broker (55-rabbitmq) + cdr-consumer (56-cdr-consumer).
# The broker is a public image pulled + side-loaded into kind so the run is
# offline-capable like every other image.
RABBITMQ_IMAGE="${RABBITMQ_IMAGE:-rabbitmq:3.13-management}"
# PROXY_VIP/PROXY_TARGET/SIP_PORT are exported by lib/net-env.sh.
export SUT_IMAGE WORKER_REPLICAS REPL_ENABLE REPL_PORT SCENARIO LIMITER_CAP RABBITMQ_IMAGE
# Observability: VictoriaMetrics + Grafana host stack + in-cluster vmagent/KSM/
# node-exporter/fluent-bit. Deployed automatically on `up` so the freshly
# (re)created cluster always has scraping wired and Grafana dashboards loaded.
# Set OBS_ENABLE=0 to skip (e.g. CI without docker compose).
OBS_ENABLE="${OBS_ENABLE:-1}"
OBS_DIR="${OBS_DIR:-$REPO_ROOT/deploy/observability}"

log() { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
die() { printf '\033[1;31m!! %s\033[0m\n' "$*" >&2; exit 1; }

# docker_build() — proxy-forwarding `docker build` wrapper — is defined in
# lib/proxy-env.sh (sourced above) so run.sh and endurance.sh share ONE definition.

# Overlay seams: the ONE definition of "apply a manifest" (plain vs envsubst-
# rendered), consumed by deploy()/caps() here and composable by a downstream
# overlay's own deploy. Take a full path — pass "$MANIFEST_DIR/<file>".
apply_manifest() { kubectl apply -f "$1"; }
subst_manifest() { envsubst < "$1" | kubectl apply -f -; }

# Build + kind-load every EXTRA_IMAGE_BUILDS entry (no-op when the list — the
# default — is empty). Whitespace-separated entries, each
#   name=dockerfile:context[:build-args]
# where build-args is an optional comma-separated KEY=VAL list forwarded as
# --build-arg, e.g.
#   EXTRA_IMAGE_BUILDS="mock:dev=/abs/Dockerfile.mock:/abs/ctx:FOO=1,BAR=2"
# Paths should be absolute (there is no cd — relative paths resolve against the
# caller's cwd). Parsed defensively: a malformed entry dies (fail fast) rather
# than silently skipping an image the overlay asked for. Called by up(); an
# overlay's own bring-up can call it directly too.
build_load_extra_images() {
  local entry name rest dockerfile context buildargs pair parts args
  for entry in $EXTRA_IMAGE_BUILDS; do
    case "$entry" in
      *=*:*) : ;;
      *) die "EXTRA_IMAGE_BUILDS entry '$entry' malformed (want name=dockerfile:context[:build-args])" ;;
    esac
    name="${entry%%=*}"; rest="${entry#*=}"
    dockerfile="${rest%%:*}"; rest="${rest#*:}"
    context="${rest%%:*}"
    buildargs=""
    if [ "$context" != "$rest" ]; then buildargs="${rest#*:}"; fi
    [ -n "$name" ]       || die "EXTRA_IMAGE_BUILDS entry '$entry': empty image name"
    [ -f "$dockerfile" ] || die "EXTRA_IMAGE_BUILDS entry '$entry': dockerfile '$dockerfile' not found"
    [ -d "$context" ]    || die "EXTRA_IMAGE_BUILDS entry '$entry': context dir '$context' not found"
    args=()
    if [ -n "$buildargs" ]; then
      IFS=',' read -ra parts <<< "$buildargs"
      for pair in "${parts[@]}"; do
        if [ -n "$pair" ]; then args+=(--build-arg "$pair"); fi
      done
    fi
    log "building extra image $name (EXTRA_IMAGE_BUILDS)"
    docker_build -f "$dockerfile" -t "$name" ${args[@]+"${args[@]}"} "$context"
    log "loading extra image $name into kind"
    kind load docker-image "$name" --name "$CLUSTER"
  done
}

preflight() {
  for t in kind kubectl docker envsubst; do command -v "$t" >/dev/null || die "missing tool: $t"; done
  [ -f "$CLUSTER_CONFIG" ]      || die "cluster.yaml missing"
  [ -f "$SIPP_DIR/Dockerfile" ] || die "sipp/Dockerfile missing (vendored sipp build context)"
  [ -d "$SCENARIOS" ]           || die "sipp/scenarios/ missing (vendored sipp scenarios)"
}

up() {
  preflight
  check_host   # cgroups + sysctls + clock (advisory; PREFLIGHT_STRICT=1 to enforce)
  sync_clock_at_bringup  # one accurate wall-clock base before any pod anchors (WSL2)

  # NON-DESTRUCTIVE by default. A cluster left over from a failed/aborted run must
  # SURVIVE so it can be analysed — destruction is explicit (`./run.sh down`). If
  # one already exists, refuse rather than silently wipe it. FORCE_RECREATE=1 opts
  # back into the old one-shot "stop the other, run this" switch.
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    if [ "${FORCE_RECREATE:-0}" = "1" ]; then
      log "FORCE_RECREATE=1 — deleting existing '$CLUSTER' cluster"
      kind delete cluster --name "$CLUSTER" 2>/dev/null || true
    else
      die "cluster '$CLUSTER' already exists — left intact for analysis. Destroy it with './run.sh down' (or re-run as 'FORCE_RECREATE=1 ./run.sh up')."
    fi
  fi

  # On ANY failure during bring-up, leave the partial cluster intact for analysis
  # — never auto-teardown. Cleared on success at the end of up().
  trap 'rc=$?; [ "$rc" -ne 0 ] && printf "\033[1;31m!! up failed (rc=%s) — cluster %s\047 left intact for analysis (NOT torn down).\n   Inspect: kubectl --context kind-%s -n %s get pods -o wide\n   Destroy: ./run.sh down\033[0m\n" "$rc" "$CLUSTER" "$CLUSTER" "$NS" >&2' EXIT

  # Pin the kind docker bridge to an IMMUTABLE $SIP_SUBNET so the keepalived VRRP
  # VIP ($PROXY_VIP, ADR-0012 D7) is ALWAYS on-subnet across recreates. Without
  # this, kind lets docker re-pick the first free /16 from its default pool on
  # each `up`; if a lower /16 is momentarily free the kind net lands there and the
  # VIP is off-subnet → whole cluster unreachable. We recreate the network here
  # (single-cluster host: it is empty right after the delete) and point kind at it
  # via KIND_EXPERIMENTAL_DOCKER_NETWORK; kind reuses a pre-existing network named
  # `kind` as-is and inherits the /16. The sibling docker stacks are pinned to
  # distinct /16s in their own compose files (victoria 172.18, callviewer 172.19)
  # so none can ever claim this one. Subnet/gateway/VIP are all configurable and
  # derived from a single SIP_SUBNET — see lib/net-env.sh.
  log "pinning kind docker bridge to immutable $SIP_SUBNET (keepalived VIP $PROXY_VIP on-subnet)"
  # If a `kind` network already exists with the correct /16, REUSE it — the obs
  # stack (grafana/victoria*) stays attached across cluster recreates, so
  # `docker network rm` would fail (active endpoints) and the subsequent
  # `create` would abort the whole `up`. Only (re)create when absent or wrong.
  if [ "$(docker network inspect kind --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null | tr ' ' '\n' | grep -cF "$SIP_SUBNET")" != "1" ]; then
    docker network rm kind >/dev/null 2>&1 || true
    docker network create --driver bridge \
      --subnet "$SIP_SUBNET" --gateway "$SIP_GATEWAY" kind >/dev/null
  fi
  export KIND_EXPERIMENTAL_DOCKER_NETWORK=kind

  log "creating cluster '$CLUSTER' from shared topology"
  kind create cluster --name "$CLUSTER" --config "$CLUSTER_CONFIG" --wait "$KIND_WAIT"

  # Cap each kind node's memory so the cluster can never starve the WSL2 host
  # (an uncapped node + a parallel cargo build OOM'd the host once). Pod limits
  # still apply inside; this is the node-container backstop.
  log "capping kind node memory (host-starvation backstop)"
  CLUSTER="$CLUSTER" "$REPO_ROOT/deploy/k8s/cap-kind-memory.sh" || true

  # External SIP plane: create the no-NAT sipext bridge and dual-home the two
  # tier=edge nodes (the ONLY components attached to both planes).
  sipext_up

  # Log the effective proxy env ONCE before any build so the exact forwarded
  # values (and the merged local NO_PROXY bypass) are transparent in the run log.
  log "proxy env (forwarded to docker build): $(proxy_env_summary)"
  log "building SUT image $SUT_IMAGE"
  docker_build -f "$REPO_ROOT/deploy/docker/Dockerfile" -t "$SUT_IMAGE" "$REPO_ROOT"
  log "building sipp:dev from vendored context ($SIPP_DIR)"
  docker_build -t sipp:dev "$SIPP_DIR"
  log "building keepalived sidecar image $KEEPALIVED_IMAGE (proxy VIP, ADR-0012 D7)"
  docker_build -f "$REPO_ROOT/deploy/docker/Dockerfile.keepalived" -t "$KEEPALIVED_IMAGE" "$REPO_ROOT"
  # RabbitMQ is a `docker pull` (not a build): pulls go through the docker daemon's
  # own proxy config (~/.docker/config.json / systemd), NOT build-args — nothing to
  # forward here. The merged NO_PROXY above keeps local lookups off the proxy.
  log "pulling RabbitMQ image $RABBITMQ_IMAGE (CDR transport)"
  docker image inspect "$RABBITMQ_IMAGE" >/dev/null 2>&1 || docker pull "$RABBITMQ_IMAGE"
  log "loading images into kind"
  kind load docker-image "$SUT_IMAGE" --name "$CLUSTER"
  # sipp:dev is NOT kind-loaded: all SIPp generators (UAC + UAS) run as plain
  # docker containers on the sipext bridge now, straight off the host image.
  kind load docker-image "$KEEPALIVED_IMAGE" --name "$CLUSTER"
  kind load docker-image "$RABBITMQ_IMAGE" --name "$CLUSTER"
  # Downstream overlay hook: extra images to build + side-load (empty default =
  # exact historical behaviour, nothing extra happens).
  build_load_extra_images

  obs   # bring up / refresh observability against the new cluster
  trap - EXIT   # success — disarm the "left intact for analysis" handler
}

# Deploy (or re-apply) the observability stack: host VM+Grafana via docker
# compose + the in-cluster scrapers (vmagent/KSM/node-exporter/fluent-bit).
# install.sh is idempotent; recreating the cluster wipes the in-cluster
# `observability` namespace, so this must re-run after every `up`.
obs() {
  [ "$OBS_ENABLE" = "1" ] || { log "OBS_ENABLE=0 — skipping observability"; return 0; }
  [ -x "$OBS_DIR/install.sh" ] || die "observability installer missing: $OBS_DIR/install.sh"
  # Grafana now binds 0.0.0.0, so it is reachable via the host IP as well as
  # loopback. Derive a best-effort host IP for the hint (don't hardcode one — WSL2
  # mirrored networking means it is not a fixed/known address); fall back quietly.
  local gport="${GRAFANA_PORT:-3333}" hostip
  hostip="$(hostname -I 2>/dev/null | awk '{print $1}')"
  log "deploying observability (Grafana http://127.0.0.1:${gport}${hostip:+ / http://$hostip:$gport}, VictoriaMetrics :8428)"
  "$OBS_DIR/install.sh" --bootstrap
}

deploy() {
  preflight
  apply_manifest "$MANIFEST_DIR/00-namespace.yaml"
  # NOTE: the sipp-scenarios / sipp-exporter ConfigMaps are GONE — scenarios
  # and the stat exporter are bind-mounted straight into the docker-on-sipext
  # generator containers (lib/sipext-gen.sh); nothing in-cluster consumes them.

  # KIND-MASQ-AGENT exemptions FIRST (manifests/05): the RETURN rules must be
  # in place before the first SIP flows or early calls hit the newkahneed-041
  # masquerade drop. Call-path load-bearing — gate on the rollout.
  log "deploying masq-exempt DaemonSet (KIND-MASQ-AGENT RETURN rules, newkahneed-041 contract)"
  subst_manifest "$MANIFEST_DIR/05-masq-exempt.yaml"
  kubectl -n "$NS" rollout status ds/masq-exempt --timeout="$ROLLOUT_TIMEOUT"

  # Downstream UAS: docker containers on the sipext plane (regenerates
  # uas-targets.csv with their pinned IPs for the UAC streams).
  sipp_uas_up

  log "deploying b2bua workers (repl=${REPL_ENABLE}, repl_port=${REPL_PORT})"
  # RBAC for the worker's EndpointSlice informer (needed when REPL_ENABLE=1; a
  # harmless ServiceAccount/Role otherwise).
  apply_manifest "$MANIFEST_DIR/15-worker-rbac.yaml"
  # Shared call limiter (single replica). Deployed before the workers so its
  # ClusterIP DNS resolves at worker startup. Inert until a decision returns
  # `call_limiter` entries, so it is a no-op for the scripted load runner.
  subst_manifest "$MANIFEST_DIR/50-call-limiter.yaml"
  # CDR transport: RabbitMQ broker + the dedicated CDR metrics consumer. Broker
  # first (before the workers) so its ClusterIP DNS resolves at worker startup;
  # workers fail-open (drop CDRs) if it is briefly unavailable.
  log "deploying RabbitMQ (CDR transport) + cdr-consumer"
  subst_manifest "$MANIFEST_DIR/55-rabbitmq.yaml"
  # CDR infra is best-effort telemetry, NOT on the call path: the workers
  # fail-open (drop CDRs) if the broker is slow/absent. So wait for it, but never
  # let an unhealthy broker/consumer abort the run (these gates are `|| true`,
  # unlike the call-path worker/proxy/uas gates below).
  kubectl -n "$NS" rollout status deploy/rabbitmq --timeout="$ROLLOUT_TIMEOUT" || true
  subst_manifest "$MANIFEST_DIR/56-cdr-consumer.yaml"
  subst_manifest "$MANIFEST_DIR/20-worker.yaml"
  kubectl -n "$NS" rollout status statefulset/b2bua-worker --timeout="$ROLLOUT_TIMEOUT"
  kubectl -n "$NS" rollout status deploy/cdr-consumer --timeout="$ROLLOUT_TIMEOUT" || true

  # The proxy now discovers the worker pool from k8s EndpointSlices (ADR-0012 D4)
  # — the SAME informer the b2bua replication engine uses (ADR-0011 X7) — so the
  # old "resolve worker pod IPs -> PROXY_WORKERS" bake is GONE (and with it the
  # chaos.sh proxy redeploy after a worker kill: a restarted worker's new IP flows
  # through the watch automatically).
  #
  # The worker *id* is still the pod name (EndpointSlice targetRef.name = POD_NAME
  # = B2BUA_ORDINAL = replication membership ordinal): the proxy signs it into the
  # w_pri/w_bak stickiness cookie, the b2bua echoes w_pri into the callRef primary
  # and w_bak into topology.bak (the changelog peer key). If those disagreed, the
  # changelog would bump under a peer no puller subscribes as → repl_pull_applied
  # stays 0 and the backup holds no replicas. EndpointSlice targetRef.name makes
  # callRef ownership, the cookie, and replication membership agree by construction
  # (see 20-worker.yaml / 25-proxy-rbac.yaml).
  log "deploying sip-front-proxy (k8s EndpointSlice worker discovery)"
  apply_manifest "$MANIFEST_DIR/25-proxy-rbac.yaml"
  subst_manifest "$MANIFEST_DIR/30-proxy.yaml"
  kubectl -n "$NS" rollout status deploy/sip-front-proxy --timeout="$ROLLOUT_TIMEOUT"
  # Proxy "Ready" means process-up + >=1 Alive worker — NOT "the VIP response
  # path works" (newkahneed-038: a fresh bring-up once passed Ready with ~99% of
  # responses never returning through the VIP until a manual proxy restart).
  # Gate the deploy on a real call-path round-trip before declaring the stack
  # ready. Skippable via VIP_SMOKE=0 for hosts without the sipext bridge.
  if [ "${VIP_SMOKE:-1}" = "1" ]; then
    vip_smoke || die "vip-smoke: external VIP ${SIPEXT_VIP}:${SIP_PORT} response path is DEAD after deploy (newkahneed-038). Check keepalived GARP convergence + track_interface: kubectl -n $NS logs deploy/sip-front-proxy -c keepalived; a 'kubectl -n $NS rollout restart deploy/sip-front-proxy' forces a clean re-election."
  fi
  # Isolation invariant gate (sipext layout): only the proxy bridges the two
  # planes; workers can neither be reached from sipext nor reach it, and the
  # external zone is NXDOMAIN in-cluster. Skippable via ISOLATION_SMOKE=0.
  if [ "${ISOLATION_SMOKE:-1}" = "1" ]; then
    isolation_smoke
  fi
  log "stack ready"
  kubectl -n "$NS" get pods -o wide
}

# ---------------------------------------------------------------------------
# External SIP plane (sipext) — the no-NAT dual-plane layout.
#
# A dedicated docker bridge with masquerade DISABLED: callers (loadgen / sipp
# containers / the WSL host itself via the bridge interface) reach the proxy's
# EXTERNAL face (SIPEXT_VIP) same-L2 — zero SNAT/DNAT/conntrack rewrite on the
# SIP path (the newkahneed-041 failure class is structurally impossible here).
# Only the two tier=edge kind nodes are dual-homed; app nodes/workers have no
# interface or route to this plane (isolation_smoke gates that).
sipext_up() {
  # Reuse-or-recreate like the kind net above: only (re)create when absent or
  # on the wrong subnet, so long-lived attachments survive a re-run.
  if [ "$(docker network inspect "$SIPEXT_NET" --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null | tr ' ' '\n' | grep -cF "$SIPEXT_SUBNET")" != "1" ]; then
    docker network rm "$SIPEXT_NET" >/dev/null 2>&1 || true
    docker network create --driver bridge \
      --subnet "$SIPEXT_SUBNET" --gateway "$SIPEXT_GATEWAY" \
      -o com.docker.network.bridge.enable_ip_masquerade=false \
      "$SIPEXT_NET" >/dev/null
    log "sipext: created no-NAT bridge $SIPEXT_NET ($SIPEXT_SUBNET, masquerade OFF)"
  fi
  # Dual-home the two tier=edge kind nodes (k8s node name == docker container
  # name). Sorted for a deterministic edge1/edge2 -> .11/.12 assignment. This
  # adds ${SIPEXT_IF} (eth1 on a fresh kind node — the kind net claimed eth0 at
  # creation) which keepalived tracks and the proxy's external face binds via
  # the SIPEXT_VIP that keepalived floats.
  local edges node ip n=0
  edges="$(kubectl get nodes -l tier=edge -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | sort)"
  [ -n "$edges" ] || die "sipext: no tier=edge nodes found (cluster not up?)"
  for node in $edges; do
    case "$n" in
      0) ip="$SIPEXT_EDGE1_IP" ;;
      1) ip="$SIPEXT_EDGE2_IP" ;;
      *) die "sipext: >2 tier=edge nodes — pin more SIPEXT_EDGE*_IPs" ;;
    esac
    if ! docker network inspect "$SIPEXT_NET" --format '{{range .Containers}}{{.Name}} {{end}}' | grep -qw "$node"; then
      docker network connect --ip "$ip" "$SIPEXT_NET" "$node"
      log "sipext: connected edge node $node as $ip"
    fi
    n=$((n+1))
  done
  [ "$n" -eq 2 ] || log "sipext: WARNING expected 2 edge nodes, connected $n"
}

# Remove every sipext-plane container this runner started (labelled at
# `docker run`) and the bridge itself. Called from down(); safe when absent.
sipext_down() {
  docker ps -aq --filter "label=sipext-run=$CLUSTER" | xargs -r docker rm -f >/dev/null 2>&1 || true
  docker network rm "$SIPEXT_NET" >/dev/null 2>&1 || true
}

# Downstream SIPp UAS as plain docker containers on sipext (replaces the
# 10-sipp-uas StatefulSet + headless Service — tier=load nodes are GONE).
# Pinned IPs from SIPEXT_UAS_IP upward; regenerates uas-targets.csv (consumed
# by the UAC streams' -inf per-call callee injection) with those IP LITERALS —
# workers must never resolve an external name (see 20-worker.yaml
# WORKER_ALLOWED_TARGET_SUFFIXES). Explicit --cpus/--memory caps: these now
# ride the host directly, and the WSL2 box freezes when overcommitted.
SIPP_UAS_REPLICAS="${SIPP_UAS_REPLICAS:-2}"
# Per-UAS-container docker caps (NEW KNOBS, endurance-sizeable): the endurance
# profile sizes the UAS pool for ~9k concurrent B-leg dialogs across
# SIPP_UAS_REPLICAS=5 shards (the SIPp single-process timer-wheel ceiling is
# ~2900 concurrent — see the retired manifests/10-sipp-uas.yaml history), and
# raises these caps per container. Defaults match the historical values.
SIPP_UAS_CPUS="${SIPP_UAS_CPUS:-4}"
SIPP_UAS_MEM="${SIPP_UAS_MEM:-2g}"
SIPEXT_GEN_DIR="${SIPEXT_GEN_DIR:-$K8S_DIR/.sipext-gen}"
sipp_uas_up() {
  docker network inspect "$SIPEXT_NET" >/dev/null 2>&1 || die "sipext network missing — './run.sh up' creates it"
  mkdir -p "$SIPEXT_GEN_DIR"
  local prefix="${SIPEXT_UAS_IP%.*}" base="${SIPEXT_UAS_IP##*.}" i ip
  printf 'SEQUENTIAL\n' > "$SIPEXT_GEN_DIR/uas-targets.csv"
  for i in $(seq 0 $(( SIPP_UAS_REPLICAS - 1 ))); do
    ip="${prefix}.$(( base + i ))"
    docker rm -f "sipp-uas-$i" >/dev/null 2>&1 || true
    docker run -d --name "sipp-uas-$i" --label "sipext-run=$CLUSTER" \
      --restart unless-stopped \
      --network "$SIPEXT_NET" --ip "$ip" \
      --cpus "$SIPP_UAS_CPUS" --memory "$SIPP_UAS_MEM" \
      -v "$SCENARIOS:/scenarios:ro" \
      sipp:dev sipp -sf /scenarios/uas-basic.xml -i "$ip" -p 5060 \
        -l 60000 -recv_timeout 600000 -trace_err >/dev/null
    printf '%s\n' "$ip" >> "$SIPEXT_GEN_DIR/uas-targets.csv"
    log "sipp-uas-$i up at $ip (docker, sipext)"
  done
}

# Functional VIP response-path gate (newkahneed-038). Sends a SIP OPTIONS to
# SIPEXT_VIP:SIP_PORT from a one-shot docker container on the sipext bridge —
# the same vantage as real callers, so the reply (worker 200 relayed back OUT
# of the external-face socket) exercises exactly the path that a lost
# gratuitous ARP kills: request in via the external VIP, response sourced from
# it back out. Retries (~3 s/attempt) until VIP_SMOKE_TIMEOUT (default 90 s):
# the keepalived garp_master_refresh (10 s) makes a stale-ARP window self-heal
# well inside that budget, so a healthy stack passes on an early attempt and a
# genuinely dead path fails loudly instead of surfacing as an all-timeout run.
# Reuses the keepalived image (alpine busybox nc; already built locally).
vip_smoke() {
  local timeout="${VIP_SMOKE_TIMEOUT:-90}" attempts
  attempts=$(( timeout / 3 ))
  log "vip-smoke: OPTIONS round-trip via ${SIPEXT_VIP}:${SIP_PORT} from sipext vantage (<=${attempts} attempts)"
  local probe
  probe="$(cat <<'EOS'
vip="$1"; port="$2"; attempts="$3"
myip="$(hostname -i 2>/dev/null | awk '{print $1}')"
i=0
while [ "$i" -lt "$attempts" ]; do
  printf 'OPTIONS sip:vip-smoke@%s:%s SIP/2.0\r\nVia: SIP/2.0/UDP %s:5060;branch=z9hG4bK-vipsmoke-%s;rport\r\nMax-Forwards: 10\r\nFrom: <sip:vip-smoke@vip-smoke.invalid>;tag=vipsmoke%s\r\nTo: <sip:vip-smoke@%s>\r\nCall-ID: vip-smoke-%s@vip-smoke\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n' \
    "$vip" "$port" "$myip" "$i" "$i" "$vip" "$i" > /tmp/req
  nc -u -w 2 "$vip" "$port" < /tmp/req > /tmp/resp 2>/dev/null || true
  if grep -q '^SIP/2.0' /tmp/resp; then
    echo "VIP-SMOKE-OK attempt=$i status=$(head -n1 /tmp/resp | tr -d '\r')"
    exit 0
  fi
  i=$((i+1))
  sleep 1
done
echo "VIP-SMOKE-FAIL: no SIP response from $vip:$port in $attempts attempts"
exit 1
EOS
)"
  docker rm -f vip-smoke >/dev/null 2>&1 || true
  docker run --rm --name vip-smoke --label "sipext-run=$CLUSTER" \
    --network "$SIPEXT_NET" "$KEEPALIVED_IMAGE" \
    /bin/sh -c "$probe" vip-smoke "$SIPEXT_VIP" "$SIP_PORT" "$attempts"
}

# Isolation gates (sipext dual-plane invariant: ONLY the proxy bridges the two
# planes; workers may resolve internal Services only). Three properties, each
# asserted from the vantage where a violation would originate:
#   1. sipext vantage -> internal plane: OPTIONS at a worker pod IP and at the
#      internal VIP must get NO reply (only SIPEXT_VIP answers externally).
#   2. pod-network vantage -> sipext: OPTIONS at SIPEXT_VIP must get NO reply.
#   3. pod-network vantage: the external test zone must be NXDOMAIN (external
#      names are resolvable ONLY by the proxy's own resolver, never CoreDNS).
# Negative probes retry 3x so a single lost packet can't mask a real reply.
isolation_smoke() {
  local neg
  neg="$(cat <<'EOS'
target="$1"; port="$2"; label="$3"
i=0
while [ "$i" -lt 3 ]; do
  printf 'OPTIONS sip:iso@%s:%s SIP/2.0\r\nVia: SIP/2.0/UDP 0.0.0.0:5060;branch=z9hG4bK-iso-%s;rport\r\nMax-Forwards: 10\r\nFrom: <sip:iso@iso.invalid>;tag=iso%s\r\nTo: <sip:iso@%s>\r\nCall-ID: iso-%s@iso\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n' \
    "$target" "$port" "$i" "$i" "$target" "$i" > /tmp/req
  nc -u -w 2 "$target" "$port" < /tmp/req > /tmp/resp 2>/dev/null || true
  if grep -q '^SIP/2.0' /tmp/resp; then
    echo "ISOLATION-FAIL: $label ($target:$port) ANSWERED from this vantage"
    exit 1
  fi
  i=$((i+1))
done
echo "ISOLATION-OK: $label ($target:$port) unreachable as required"
exit 0
EOS
)"
  local wip
  wip="$(kubectl -n "$NS" get pods -l app=b2bua-worker -o jsonpath='{.items[0].status.podIP}' 2>/dev/null)"
  [ -n "$wip" ] || die "isolation-smoke: no b2bua-worker pod IP (deploy first)"
  log "isolation-smoke 1/3: sipext vantage must NOT reach worker $wip / internal VIP $PROXY_VIP"
  docker run --rm --label "sipext-run=$CLUSTER" --network "$SIPEXT_NET" "$KEEPALIVED_IMAGE" \
    /bin/sh -c "$neg" iso "$wip" 5060 "worker-pod" \
    || die "isolation-smoke: worker pod reachable from sipext — the b2b is NOT protected"
  docker run --rm --label "sipext-run=$CLUSTER" --network "$SIPEXT_NET" "$KEEPALIVED_IMAGE" \
    /bin/sh -c "$neg" iso "$PROXY_VIP" "$SIP_PORT" "internal-VIP" \
    || die "isolation-smoke: internal VIP reachable from sipext — planes are bridged outside the proxy"
  log "isolation-smoke 2/3: pod-network vantage must NOT reach external VIP $SIPEXT_VIP"
  kubectl -n "$NS" delete pod iso-smoke --ignore-not-found --now >/dev/null 2>&1 || true
  kubectl -n "$NS" run iso-smoke --rm -i --restart=Never \
    --image="$KEEPALIVED_IMAGE" --image-pull-policy=IfNotPresent \
    --pod-running-timeout=2m \
    --overrides='{"spec":{"nodeSelector":{"tier":"app"}}}' \
    --command -- /bin/sh -c "$neg" iso "$SIPEXT_VIP" "$SIP_PORT" "external-VIP" \
    || die "isolation-smoke: external VIP reachable from the pod network — workers can dial the world"
  log "isolation-smoke 3/3: external zone must be NXDOMAIN in-cluster"
  kubectl -n "$NS" run iso-dns --rm -i --restart=Never \
    --image="$KEEPALIVED_IMAGE" --image-pull-policy=IfNotPresent \
    --pod-running-timeout=2m \
    --overrides='{"spec":{"nodeSelector":{"tier":"app"}}}' \
    --command -- /bin/sh -c 'if nslookup uas.sipext.test >/dev/null 2>&1; then echo "ISOLATION-FAIL: uas.sipext.test resolves in-cluster"; exit 1; fi; echo "ISOLATION-OK: external zone NXDOMAIN in-cluster"' \
    || die "isolation-smoke: external test zone resolvable from the pod network"
  log "isolation-smoke: all 3 properties hold (only the proxy bridges the planes)"
}

# sample_pod <label-selector> <window-seconds> -> prints "pod cpu% rss_mb" lines
sample_pods() {
  local sel="$1" window="$2"
  local tck pods
  pods="$(kubectl -n "$NS" get pods -l "$sel" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')"
  for pod in $pods; do
    tck="$(kubectl -n "$NS" exec "$pod" -- getconf CLK_TCK 2>/dev/null || echo 100)"
    local s0 u0 st0 s1 u1 st1 rss
    read -r u0 st0 <<< "$(kubectl -n "$NS" exec "$pod" -- cat /proc/1/stat 2>/dev/null | awk '{print $14, $15}')"
    sleep "$window"
    read -r u1 st1 <<< "$(kubectl -n "$NS" exec "$pod" -- cat /proc/1/stat 2>/dev/null | awk '{print $14, $15}')"
    rss="$(kubectl -n "$NS" exec "$pod" -- awk '/VmRSS/{print $2}' /proc/1/status 2>/dev/null)"
    local djiff cpu rss_mb
    djiff=$(( (u1+st1) - (u0+st0) ))
    cpu=$(awk -v d="$djiff" -v t="$tck" -v w="$window" 'BEGIN{printf "%.1f", (d/t)/w*100}')
    rss_mb=$(awk -v r="${rss:-0}" 'BEGIN{printf "%.1f", r/1024}')
    printf "%s %s %s\n" "$pod" "$cpu" "$rss_mb"
  done
}

# caps <cps> <seconds> — one measured cap step. The UAC is a docker container
# on the sipext bridge (lib/sipext-gen.sh; was the 40-sipp-uac-job kubectl Job):
# same sipp argv semantics, dialing the EXTERNAL VIP ${SIPEXT_TARGET}:${SIP_PORT},
# with a stat-exporter twin on :9035 at the stream's sipext IP (scraped
# statically by the host VictoriaMetrics). Slot 0 (.100) by convention — caps
# steps run sequentially, so one slot serves the whole sweep.
caps() {
  local cps="$1" secs="$2"
  mkdir -p "$RESULTS"
  local ramp=8 sample=$(( secs > 8 ? secs-8 : secs ))
  local name="sipp-uac-${cps}"
  local max_calls=$(( cps * (secs+20) ))
  local maxc="${MAX_CONCURRENT:-$(( cps * 600 ))}"
  log "cap=$cps: launching UAC stream $name (${secs}s), ramp ${ramp}s, sample ${sample}s"
  # Docker caps from the legacy k8s knobs: the old LIMITS map to --cpus/--memory
  # (requests were a k8s scheduler reservation — no docker analog). The per-run
  # stats dir under $RESULTS keeps the stat CSV after the container is reaped.
  UAC_CPU_LIM="${UAC_CPU_LIM:-8}" UAC_MEM_LIM="${UAC_MEM_LIM:-1536Mi}" \
  SIPEXT_STATS_ROOT="$RESULTS/cap${cps}-stats" \
    sipext_sipp_uac_up "$name" "$SCENARIO" "$cps" "${ROLE:-load}" "${UAC_SLOT:-0}" \
      "$max_calls" "$maxc"
  sleep "$ramp"
  echo "  --- worker/proxy CPU% + RSS(MB) over ${sample}s @ ${cps} cps ---"
  { sample_pods "app=b2bua-worker" "$sample"; sample_pods "app=sip-front-proxy" 2; } | tee "$RESULTS/cap${cps}.txt"
  # Capture UAC call outcome (same "Successful call"/"Failed call"/"Total Calls
  # created" screens, now from docker logs) before reaping the containers.
  # Guard the whole block: grep returns non-zero on no-match, which must not
  # abort the sweep.
  set +e
  local stats ok fail total
  stats="$(sipext_uac_logs "$name")"
  ok="$(printf '%s' "$stats"   | grep -aE 'Successful call'     | tail -1 | grep -oE '[0-9]+' | tail -1)"
  fail="$(printf '%s' "$stats" | grep -aE 'Failed call'         | tail -1 | grep -oE '[0-9]+' | tail -1)"
  total="$(printf '%s' "$stats"| grep -aE 'Total Calls created' | tail -1 | grep -oE '[0-9]+' | tail -1)"
  printf "  UAC: total=%s successful=%s failed=%s\n" "${total:-?}" "${ok:-?}" "${fail:-?}" | tee -a "$RESULTS/cap${cps}.txt"
  set -e
  sipext_sipp_uac_rm "$name"   # stop/rm UAC + exporter (both carry sipext-run=$CLUSTER)
  sleep 6   # let in-flight calls drain before the next cap
}

sweep() {
  local secs="$1"; shift
  printf "\n==== CAP SWEEP (sampling %ss each) ====\n" "$secs"
  for c in "$@"; do caps "$c" "$secs"; done
  printf "\nPer-cap CPU%%/RSS in %s/. Scrape app metrics with:\n  kubectl -n %s port-forward deploy/sip-front-proxy 9090 & curl localhost:9090/metrics\n" "$RESULTS" "$NS"
}

# Restart any kindnetd (CNI daemon) stuck in the client-go reflector hot-loop.
# After a chaos netcut, a node's kindnetd can lose its API-server watch and never
# cleanly re-establish it: the Pod/Namespace/NetworkPolicy reflectors fall into a
# tight relist/reconnect loop (DNS i/o timeout + TLS handshake timeout on every
# iteration) that burns ~1 core forever on WSL2. kindnetd never crashes, and its
# distroless image has no shell and serves no health port, so it cannot self-heal
# via a liveness probe — the only fix is to delete the pod so the DaemonSet
# recreates it with fresh watches. This detects the confirmed log signature and
# does exactly that. Idempotent and safe: routes/nft survive a kindnetd restart,
# so node connectivity is not torn down. Run it after a chaos/endurance run (or
# any time `top` shows kindnetd spinning).
heal-kindnet() {
  command -v kubectl >/dev/null || die "missing tool: kubectl"
  local pods p n thresh="${KINDNET_HEAL_THRESHOLD:-5}" win="${KINDNET_HEAL_WINDOW:-6m}" healed=0
  pods="$(kubectl -n kube-system get pods -l app=kindnet \
            -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null)"
  [ -n "$pods" ] || { log "no kindnet pods found (is the cluster up?)"; return 0; }
  for p in $pods; do
    # grep -c exits 1 on no match under set -e — guard it.
    n="$(kubectl -n kube-system logs --since="$win" "$p" 2>/dev/null | grep -c 'Failed to watch' || true)"
    if [ "${n:-0}" -gt "$thresh" ]; then
      log "kindnet $p: $n watch failures in last $win (> $thresh) — restarting"
      kubectl -n kube-system delete pod "$p" >/dev/null 2>&1 || true
      healed=$((healed+1))
    fi
  done
  [ "$healed" -eq 0 ] && log "all kindnet pods healthy (no stuck reflector loops)" \
                      || log "restarted $healed stuck kindnet pod(s); DaemonSet will recreate them"
}

down() {
  kind delete cluster --name "$CLUSTER"
  # Tear down the external plane too: the labelled sipext containers (UAS,
  # loadgen, probes) and the bridge itself.
  sipext_down
}

# Subcommand dispatch — the surface every existing caller (endurance.sh, docs,
# direct CLI use) depends on: names and behaviour are FROZEN (issue 025).
run_main() {
  local cmd="${1:-}"; shift || true
  case "$cmd" in
    up)     up ;;
    deploy) deploy ;;
    obs)    obs ;;
    caps)   caps "$@" ;;
    sweep)  sweep "$@" ;;
    all)    up; deploy; sweep "$@" ;;
    heal-kindnet) heal-kindnet ;;
    vip-smoke) vip_smoke ;;
    isolation-smoke) isolation_smoke ;;
    sipext-up) sipext_up ;;
    uas-up) sipp_uas_up ;;
    down)   down ;;
    *) die "usage: $0 {up|deploy|obs|caps <cps> <secs>|sweep <secs> <cps...>|all <secs> <cps...>|heal-kindnet|vip-smoke|isolation-smoke|sipext-up|uas-up|down}" ;;
  esac
}

# Dispatch ONLY when executed directly; `source run.sh` defines + defaults and
# runs nothing. (The if-form — not `[[ ... ]] && run_main` — so that sourcing
# returns 0 and cannot trip a `set -e` caller.)
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  run_main "$@"
fi
