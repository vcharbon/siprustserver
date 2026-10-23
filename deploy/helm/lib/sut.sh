# shellcheck shell=bash
# The SUT release of the kind lab (kind-lab.sh): the siprust chart, or a
# product chart that depends on it. Sourced after lib/cluster.sh and
# lib/smokes.sh (log, die, kubectl on the lab's context, vip_smoke,
# isolation_smoke); reads HELM_DIR, CLUSTER, NS, ROLLOUT_TIMEOUT.
#
# SUT_CHART (default the siprust chart), SUT_RELEASE (default siprust) and
# SUT_SUBCHART (the name a product chart gives its siprust dependency): a
# product chart gets the siprust kind values nested under that name, then its
# own values-kind.yaml when it has one, then the helm args.

SUT_CHART="${SUT_CHART:-$HELM_DIR/siprust}"
SUT_RELEASE="${SUT_RELEASE:-siprust}"
SUT_SUBCHART="${SUT_SUBCHART:-}"

# The siprust kind values nested under SUT_SUBCHART, as a values file of the
# product chart; its path on stdout. Nothing when the SUT is the siprust chart.
sut_kind_values() {
  local out
  [ -n "$SUT_SUBCHART" ] || return 0
  out="$(mktemp "${TMPDIR:-/tmp}/sut-kind-values.XXXXXX.yaml")"
  { printf '%s:\n' "$SUT_SUBCHART"; sed 's/^/  /' "$HELM_DIR/siprust/values-kind.yaml"; } > "$out"
  printf '%s\n' "$out"
}

# A product chart's dependencies, built from their file:// sources.
sut_dependencies() {
  [ -z "$SUT_SUBCHART" ] || helm dependency update "$SUT_CHART" >/dev/null \
    || die "helm dependency update $SUT_CHART failed"
}

# `helm <verb> <release> <chart>` of the SUT with its whole value set: the
# nested siprust kind values, the chart's values-kind.yaml when it has one,
# then the args.
sut_helm() { # <verb...> -- [helm args]
  local verb=() nested own=() rc=0
  while [ "$1" != -- ]; do verb+=("$1"); shift; done
  shift
  sut_dependencies
  nested="$(sut_kind_values)"
  [ ! -f "$SUT_CHART/values-kind.yaml" ] || own=(-f "$SUT_CHART/values-kind.yaml")
  helm "${verb[@]}" "$SUT_RELEASE" "$SUT_CHART" --kube-context "kind-$CLUSTER" -n "$NS" \
    ${nested:+-f "$nested"} "${own[@]}" "$@" || rc=$?
  [ -z "$nested" ] || rm -f "$nested"
  return "$rc"
}

sut_release() { # [helm args]
  log "helm upgrade --install $SUT_RELEASE ($NS, $SUT_CHART)"
  sut_helm upgrade --install -- --create-namespace "$@" --wait --timeout "$ROLLOUT_TIMEOUT"
}

sut_template() { # [helm args]
  sut_helm template -- "$@"
}

# Objects of the same names helm does not own, or another release owns, are
# refused before helm refuses to adopt them.
require_own_release() {
  local owner release
  kubectl -n "$NS" get statefulset b2bua-worker >/dev/null 2>&1 || return 0
  owner="$(kubectl -n "$NS" get statefulset b2bua-worker \
    -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}')"
  [ "$owner" = "Helm" ] \
    || die "namespace $NS holds a b2bua-worker helm does not own; delete it first: kubectl --context kind-$CLUSTER delete namespace $NS"
  release="$(kubectl -n "$NS" get statefulset b2bua-worker \
    -o jsonpath='{.metadata.annotations.meta\.helm\.sh/release-name}')"
  [ "$release" = "$SUT_RELEASE" ] \
    || die "namespace $NS holds release $release; uninstall it first: helm --kube-context kind-$CLUSTER -n $NS uninstall $release"
}

sut_has_proxy() { kubectl -n "$NS" get deploy sip-front-proxy >/dev/null 2>&1; }

# The `hakit obs-check` scope of the release: `proxy` when it runs the front proxy.
sut_obs_scope() { if sut_has_proxy; then echo proxy; else echo sut; fi; }

# The VIP and isolation smokes, when the release deploys the front proxy.
sut_smokes() {
  if sut_has_proxy; then
    vip_smoke || die "vip-smoke: the external VIP ${SIPEXT_VIP}:${SIP_PORT} answers nothing; kubectl -n $NS logs deploy/sip-front-proxy -c keepalived"
    isolation_smoke
  else
    log "no front proxy in $NS: the VIP and isolation smokes do not apply"
  fi
}
