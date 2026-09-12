# Shared kubectl-context pinning for the kind-based SIP runner — sourced by
# run.sh, endurance.sh and chaos.sh so all three target the SAME cluster and can
# never silently act on another kube environment that happens to share this host.
#
# kind names its context "kind-<cluster>". We define a transparent `kubectl`
# wrapper function (not an alias — aliases don't expand in non-interactive
# scripts) so EVERY bare `kubectl ...` call site is pinned without edits. The
# context is recomputed from $CLUSTER on each call, so this is safe to source
# before CLUSTER is finalised; export KCTX to override the derived name.
#
# Source me AFTER lib/net-env.sh (same as the other libs). Safe to source twice.

kubectl() { command kubectl --context "${KCTX:-kind-${CLUSTER:-sip-e2e}}" "$@"; }

# kind_load <image> — side-load a host docker image into every node of $CLUSTER.
# Goes through a single-platform archive: under docker's containerd image store
# `docker save` of a multi-platform image writes an index whose foreign-platform
# blobs are not present, and kind's `ctr import --all-platforms` then fails on
# the missing digest. Saving the daemon's own platform keeps the archive
# complete. A docker whose `save` has no --platform gets the plain load.
kind_load() {
  local image="$1" platform tar
  if ! docker save --help 2>/dev/null | grep -q -- '--platform'; then
    kind load docker-image "$image" --name "$CLUSTER"
    return
  fi
  platform="$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')"
  tar="$(mktemp "${TMPDIR:-/tmp}/kind-load.XXXXXX.tar")"
  docker save --platform "$platform" -o "$tar" "$image" \
    && kind load image-archive "$tar" --name "$CLUSTER"
  local rc=$?
  rm -f "$tar"
  return "$rc"
}
