# Shared HTTP(S)-proxy wiring for the kind-based SIP runner — the SINGLE source
# of truth for the local-vs-non-local proxy split, sourced by run.sh (and, later,
# by deploy/observability/install.sh) so a PROXIFIED host can still build images
# and reach external mirrors while NEVER funnelling cluster-internal traffic
# through the proxy.
#
# >>> LOCAL vs NON-LOCAL SPLIT <<<
# On a host that sits behind a corporate/dev HTTP proxy, the Docker BUILD steps
# (apt-get/apk/git clone inside the Dockerfiles) and the host's own registry /
# mirror / grafana-plugin fetches need the proxy to reach the outside world —
# deb.debian.org, github.com, grafana.com, registry-1.docker.io and friends.
# But EVERYTHING that lives on the kind cluster network or on loopback must
# BYPASS the proxy, or local health curls and intra-cluster calls would be
# (mis)routed through it and fail:
#
#   NON-LOCAL (through the proxy)  = anything on the public internet
#   LOCAL     (bypass via NO_PROXY) = 127.0.0.1/localhost/::1, the kind subnet +
#                                     gateway, the proxy VIP, the k8s service &
#                                     pod CIDRs, and the cluster DNS suffixes
#
# We read the host proxy vars (upper- AND lower-case), compute the canonical
# LOCAL no-proxy list, MERGE it with any host NO_PROXY, and export an effective
# NO_PROXY/no_proxy for the script's own use. `proxy_build_args` emits the
# matching `--build-arg` flags for `docker build`; `local_curl` runs a curl that
# always bypasses the proxy for 127.0.0.1 checks.
#
# Source me AFTER lib/net-env.sh (it relies on SIP_SUBNET/SIP_GATEWAY/PROXY_VIP
# being exported). Safe to source more than once; respects pre-set overrides.

# Host proxy vars, accepting either case (upper takes precedence, then lower).
PROXY_HTTP="${HTTP_PROXY:-${http_proxy:-}}"
PROXY_HTTPS="${HTTPS_PROXY:-${https_proxy:-}}"
PROXY_NO="${NO_PROXY:-${no_proxy:-}}"

# Kubernetes service + pod CIDRs are not derivable from net-env (they are the
# in-cluster overlay, not the docker bridge) — default to kind/kubeadm's stock
# ranges, overridable for bespoke clusters.
K8S_SERVICE_CIDR="${K8S_SERVICE_CIDR:-10.96.0.0/12}"
K8S_POD_CIDR="${K8S_POD_CIDR:-10.244.0.0/16}"

# Canonical LOCAL no-proxy list (see header). PROXY_VIP/SIP_SUBNET/SIP_GATEWAY
# normally come from net-env.sh; we repeat net-env's stock defaults via `:-` so
# this lib is ALSO safe to source standalone under `set -u` (the documented
# install.sh reuse path) without net-env.sh having run first. The .svc* suffixes
# cover cluster-DNS names.
_proxy_local_list="127.0.0.1,localhost,::1,\
${SIP_SUBNET:-172.20.0.0/16},${SIP_GATEWAY:-172.20.0.1},${PROXY_VIP:-172.20.255.250},\
${K8S_SERVICE_CIDR},${K8S_POD_CIDR},\
.svc,.svc.cluster.local,.cluster.local"

# Merge the host NO_PROXY in front of the local list (host entries first so any
# host-specific bypass survives), de-duplicating empties. The merged value is
# the effective NO_PROXY the script — and docker build — must use.
if [ -n "$PROXY_NO" ]; then
  NO_PROXY="${PROXY_NO},${_proxy_local_list}"
else
  NO_PROXY="$_proxy_local_list"
fi
no_proxy="$NO_PROXY"
export NO_PROXY no_proxy K8S_SERVICE_CIDR K8S_POD_CIDR

# proxy_build_args — emit the `docker build` --build-arg flags for the proxy env,
# one flag per line on stdout (caller splits with `mapfile`/word-split). A flag
# is emitted only when its host var is non-empty, EXCEPT NO_PROXY which is ALWAYS
# emitted (the merged local list is harmless and keeps in-build local fetches off
# the proxy). With NO host proxy set, only the two NO_PROXY flags are emitted, so
# a non-proxied host builds exactly like plain `docker build`.
proxy_build_args() {
  if [ -n "$PROXY_HTTP" ]; then
    printf -- '--build-arg\nHTTP_PROXY=%s\n--build-arg\nhttp_proxy=%s\n' "$PROXY_HTTP" "$PROXY_HTTP"
  fi
  if [ -n "$PROXY_HTTPS" ]; then
    printf -- '--build-arg\nHTTPS_PROXY=%s\n--build-arg\nhttps_proxy=%s\n' "$PROXY_HTTPS" "$PROXY_HTTPS"
  fi
  printf -- '--build-arg\nNO_PROXY=%s\n--build-arg\nno_proxy=%s\n' "$NO_PROXY" "$NO_PROXY"
}

# proxy_env_summary — one-line human summary of the effective proxy env, for the
# run log (so the exact forwarded values are transparent before any build).
proxy_env_summary() {
  printf 'HTTP_PROXY=%s HTTPS_PROXY=%s NO_PROXY=%s' \
    "${PROXY_HTTP:-<unset>}" "${PROXY_HTTPS:-<unset>}" "$NO_PROXY"
}

# local_curl — curl that ALWAYS bypasses the proxy (for 127.0.0.1/localhost
# health checks). Forces the merged no_proxy and clears any inherited *_proxy so
# the request can never be funnelled through the proxy.
local_curl() {
  http_proxy='' https_proxy='' HTTP_PROXY='' HTTPS_PROXY='' \
    no_proxy="$NO_PROXY" NO_PROXY="$NO_PROXY" curl "$@"
}

# docker_build — `docker build` wrapper that injects the proxy build-args
# (proxy_build_args) so apt-get/apk/git clone inside the Dockerfiles can reach
# external mirrors on a proxified host, while the merged NO_PROXY keeps
# cluster-local/127 fetches off the proxy. SHARED by run.sh (cluster `up`) and
# endurance.sh (wireup rebuild) so BOTH build sites forward the proxy identically.
# proxy_build_args emits one token per line — read into an array so values stay
# intact. Echoes the EXACT command it runs (via the caller's `log` if defined,
# else stderr) so the forwarded args are transparent in the run log. With no host
# proxy set it degrades to plain `docker build` (only the harmless NO_PROXY arg).
# Caller-side redirections (e.g. >>"$RUNLOG" 2>&1) apply to the build output.
docker_build() {
  local args=() line
  while IFS= read -r line; do [ -n "$line" ] && args+=("$line"); done < <(proxy_build_args)
  if declare -F log >/dev/null 2>&1; then
    log "docker build ${args[*]} $*"
  else
    printf '\033[1;36m>> docker build %s %s\033[0m\n' "${args[*]}" "$*" >&2
  fi
  docker build "${args[@]}" "$@"
}
