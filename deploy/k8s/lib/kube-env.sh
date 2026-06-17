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
