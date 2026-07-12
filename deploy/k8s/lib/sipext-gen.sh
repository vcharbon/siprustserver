# Shared "launch a load generator as a docker container on the sipext bridge"
# helpers — the SINGLE implementation behind run.sh caps()/sweep(), every
# endurance.sh baseline stream, and the chaos.sh transient streams (failover/
# peak/abuse/orphan), mirroring how lib/net-env.sh is shared so the three
# runners never drift. Replaces the retired manifests/40-sipp-uac-job.yaml
# (tier=load kind nodes are GONE — sipext dual-plane layout).
#
# Source AFTER lib/net-env.sh (needs SIPEXT_NET/SIPEXT_TARGET/SIP_PORT/
# SIPEXT_UAC_BASE_IP). Safe to source more than once; respects pre-set
# overrides; sourcing has no side effects beyond defaults + function defs.
#
# The stream contract (one UAC stream = TWO containers sharing one netns/IP):
#   sipext_sipp_uac_up <name> <scenario> <cps> <role> <slot> <max_calls> <max_concurrent>
#     - <name>       docker container name (was the k8s Job name), e.g.
#                    sipp-uac-short-em. The exporter twin is "<name>-exporter".
#     - <scenario>   file under $SCENARIOS (mounted ro at /scenarios)
#     - <role>       stream role label (long/short_em/.../abuse/peak/orphan/
#                    failover/load) — becomes the exporter's SIPP_ROLE, i.e.
#                    the role= label every dashboard/endurance gate keys on,
#                    AND the container label sipext-role.
#     - <slot>       deterministic IP slot: IP = SIPEXT_UAC_BASE_IP + slot
#                    (.100+; see the slot plan in lib/net-env.sh). Callers own
#                    the slot assignment so parallel streams never collide.
#     - <max_calls>/<max_concurrent>  sipp -m / -l (same semantics as before).
#   Env knobs (defaults reproduce the old job template):
#     UAC_CPU_LIM (k8s qty, default 8)     -> docker --cpus   (hard cap; docker
#     UAC_MEM_LIM (k8s qty, default 1536Mi)-> docker --memory  has no "request")
#     UAC_RESTART (default on-failure:4)   -> docker --restart (the old Job
#                    backoffLimit:4 analog: a SIPp exit-255 timer-wheel abort
#                    is recreated in place; docker kill/rm never triggers it)
#     LIMITER_CAP (default 20)             -> the -key xapi limiter JSON
#     SIPEXT_STATS_ROOT (default $SIPEXT_GEN_DIR/stats) -> host dir; the
#                    stream's stat CSV lands in $SIPEXT_STATS_ROOT/<name>/ and
#                    SURVIVES the container (bind mount at /stats).
#     SCENARIOS / SIPP_EXPORTER_DIR        -> scenario + exporter bind mounts
#   Argv semantics preserved from the retired job: target ${SIPEXT_TARGET}:
#   ${SIP_PORT} (callers dial the EXTERNAL VIP — never PROXY_TARGET, which
#   stays the internal, in-cluster face), -sf, -key xapi limiter JSON,
#   -inf uas-targets.csv (the run-GENERATED sipext IP-literal CSV shadows the
#   stale in-repo FQDN file — workers must never resolve external names),
#   -s service, -i <its sipext IP> -p 5060, -r/-rp/-l/-m/-recv_timeout
#   600000/-trace_err/-trace_stat -stf /stats/stat.csv -fd 1.
#
# Observability: the exporter twin serves :9035 on the stream's sipext IP; the
# host-side VictoriaMetrics scrapes the .100-.135 range statically (see
# deploy/observability/stack/victoriametrics/prometheus.yml) — no per-stream
# registration, and a missing/absent target can never fail a run.
#
# Reaping: every container carries the label sipext-run=$CLUSTER, so run.sh
# sipext_down (and `./run.sh down`) removes them all.

# Resolve the k8s deploy dir (this file lives in <k8s>/lib/) without cd.
_SIPEXT_GEN_K8S_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CLUSTER="${CLUSTER:-sip-e2e}"
SIPEXT_GEN_DIR="${SIPEXT_GEN_DIR:-$_SIPEXT_GEN_K8S_DIR/.sipext-gen}"
SIPEXT_STATS_ROOT="${SIPEXT_STATS_ROOT:-$SIPEXT_GEN_DIR/stats}"
SCENARIOS="${SCENARIOS:-$_SIPEXT_GEN_K8S_DIR/sipp/scenarios}"
SIPP_EXPORTER_DIR="${SIPP_EXPORTER_DIR:-$_SIPEXT_GEN_K8S_DIR/sipp/exporter}"
SIPP_IMAGE="${SIPP_IMAGE:-sipp:dev}"
LIMITER_CAP="${LIMITER_CAP:-20}"

_sipext_gen_log() { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
_sipext_gen_die() { printf '\033[1;31m!! %s\033[0m\n' "$*" >&2; return 1; }

# k8s resource quantity -> docker flag value. Docker has no request/limit
# split, so callers map the old k8s LIMIT onto --cpus/--memory (the request
# was a scheduler reservation — meaningless on plain docker).
k8s_cpu_to_docker() {  # "2" -> 2, "500m" -> 0.500
  local q="$1"
  case "$q" in
    *m) awk -v v="${q%m}" 'BEGIN{printf "%.3f", v/1000}' ;;
    *)  printf '%s' "$q" ;;
  esac
}
k8s_mem_to_docker() {  # "384Mi" -> 384m, "2Gi" -> 2g (docker wants b/k/m/g)
  local q="$1"
  case "$q" in
    *Gi) printf '%sg' "${q%Gi}" ;;
    *Mi) printf '%sm' "${q%Mi}" ;;
    *Ki) printf '%sk' "${q%Ki}" ;;
    *G)  printf '%sg' "${q%G}" ;;
    *M)  printf '%sm' "${q%M}" ;;
    *K)  printf '%sk' "${q%K}" ;;
    *)   printf '%s' "$q" ;;
  esac
}

# Deterministic per-stream IP: SIPEXT_UAC_BASE_IP + slot. Guarded so a bad
# slot can never land on a pinned IP (<.100) or the VIP (.250).
sipext_uac_ip() {  # $1 = slot (0..149)
  local slot="$1"
  case "$slot" in
    ''|*[!0-9]*) _sipext_gen_die "sipext_uac_ip: slot '$slot' is not a number"; return 1 ;;
  esac
  local prefix="${SIPEXT_UAC_BASE_IP%.*}" base="${SIPEXT_UAC_BASE_IP##*.}" last
  last=$(( base + slot ))
  if [ "$last" -ge 250 ]; then
    _sipext_gen_die "sipext_uac_ip: slot $slot -> .$last collides with the VIP range (max slot $(( 249 - base )))"
    return 1
  fi
  echo "${prefix}.${last}"
}

# Launch (or replace) one SIPp UAC stream + its stat exporter. See the
# contract comment at the top of this file.
sipext_sipp_uac_up() {  # name scenario cps role slot max_calls max_concurrent
  local name="$1" scenario="$2" cps="$3" role="$4" slot="$5" max_calls="$6" maxc="$7"
  local ip sdir cpus mem restart
  docker network inspect "$SIPEXT_NET" >/dev/null 2>&1 \
    || { _sipext_gen_die "sipext network '$SIPEXT_NET' missing — './run.sh up' creates it"; return 1; }
  # The GENERATED IP-literal CSV (sipp_uas_up) — never fall back to the stale
  # in-repo FQDN file: workers must not resolve external names
  # (WORKER_ALLOWED_TARGET_SUFFIXES, 20-worker.yaml).
  [ -s "$SIPEXT_GEN_DIR/uas-targets.csv" ] \
    || { _sipext_gen_die "generated uas-targets.csv missing ($SIPEXT_GEN_DIR) — './run.sh deploy' (sipp_uas_up) regenerates it"; return 1; }
  ip="$(sipext_uac_ip "$slot")" || return 1
  sdir="$SIPEXT_STATS_ROOT/$name"
  mkdir -p "$sdir"
  rm -f "$sdir/stat.csv"   # a stale CSV from a previous run would feed the exporter old counters
  cpus="$(k8s_cpu_to_docker "${UAC_CPU_LIM:-8}")"
  mem="$(k8s_mem_to_docker "${UAC_MEM_LIM:-1536Mi}")"
  restart="${UAC_RESTART:-on-failure:4}"
  sipext_sipp_uac_rm "$name"
  docker run -d --name "$name" \
    --label "sipext-run=$CLUSTER" --label "sipext-kind=sipp-uac" \
    --label "sipext-role=$role" --label "sipext-stream=$name" \
    --network "$SIPEXT_NET" --ip "$ip" \
    --restart "$restart" \
    --cpus "$cpus" --memory "$mem" \
    -v "$SCENARIOS:/scenarios:ro" \
    -v "$SIPEXT_GEN_DIR/uas-targets.csv:/scenarios/uas-targets.csv:ro" \
    -v "$sdir:/stats" \
    "$SIPP_IMAGE" sipp "${SIPEXT_TARGET}:${SIP_PORT}" \
      -sf "/scenarios/$scenario" \
      -key xapi "{\"action\":\"route\",\"call_limiter\":[{\"id\":\"endurance-limiter\",\"limit\":${LIMITER_CAP}}]}" \
      -inf /scenarios/uas-targets.csv \
      -s service \
      -i "$ip" -p 5060 \
      -r "$cps" -rp 1000 \
      -l "$maxc" -m "$max_calls" \
      -recv_timeout 600000 \
      -trace_err -trace_stat -stf /stats/stat.csv -fd 1 \
    >/dev/null || { _sipext_gen_die "docker run failed for UAC stream $name"; return 1; }
  # Stat->Prometheus exporter twin: SAME netns as the UAC (shares its IP; 9035
  # TCP cannot clash with sipp's 5060 UDP), same env contract as the old
  # native-sidecar (SIPP_STAT_FILE/SIPP_SCENARIO/SIPP_ROLE/SIPP_JOB). Failure
  # to start it degrades reporting only — never the stream.
  docker run -d --name "${name}-exporter" \
    --label "sipext-run=$CLUSTER" --label "sipext-kind=sipp-exporter" \
    --network "container:$name" \
    --cpus 0.5 --memory 128m \
    -v "$sdir:/stats:ro" \
    -v "$SIPP_EXPORTER_DIR:/exporter:ro" \
    -e SIPP_STAT_FILE=/stats/stat.csv \
    -e "SIPP_SCENARIO=$scenario" \
    -e "SIPP_ROLE=$role" \
    -e "SIPP_JOB=$name" \
    "$SIPP_IMAGE" python3 /exporter/sipp_stat_exporter.py \
    >/dev/null 2>&1 \
    || _sipext_gen_log "WARN: exporter for $name failed to start (metrics missing; stream unaffected)"
  _sipext_gen_log "UAC stream $name up at $ip (docker/sipext, role=$role, ${cps}cps, --cpus $cpus --memory $mem)"
}

# Remove one stream (UAC + exporter). Safe when absent.
sipext_sipp_uac_rm() {  # $1 = name
  docker rm -f "$1" "$1-exporter" >/dev/null 2>&1 || true
}

# Is the stream's UAC container running? Echoes true/false (missing -> false).
sipext_uac_running() {  # $1 = name
  local st
  st="$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || echo false)"
  [ "$st" = "true" ] && echo true || echo false
}

# Wait for a stream to run to completion (sipp exits when -m is reached).
# Returns 0 once the container is no longer running (or never existed),
# 1 on timeout. Replaces `kubectl wait --for=condition=complete job/...`.
sipext_uac_wait_exit() {  # $1 = name, $2 = timeout secs (default 120)
  local name="$1" timeout="${2:-120}" waited=0
  while [ "$waited" -lt "$timeout" ]; do
    [ "$(sipext_uac_running "$name")" = "true" ] || return 0
    sleep 3; waited=$(( waited + 3 ))
  done
  return 1
}

# Print the stream's SIPp stdout/stderr (the "Successful call"/"Failed call"/
# "Total Calls created" screens). Replaces `kubectl logs`.
sipext_uac_logs() {  # $1 = name [$2 = tail lines]
  local name="$1" tail_n="${2:-}"
  if [ -n "$tail_n" ]; then
    docker logs --tail "$tail_n" "$name" 2>&1 || true
  else
    docker logs "$name" 2>&1 || true
  fi
}
