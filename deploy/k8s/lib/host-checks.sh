# Host prerequisite checks for the kind-based SIP runner. Two things a stock
# (non-WSL) Linux box — e.g. a VMware VM — does NOT always satisfy and that
# silently degrade the run:
#
#   1. cgroup v2 (or v1 WITH swap accounting). cap-kind-memory.sh sets
#      `docker update --memory X --memory-swap X` — a true no-swap ceiling — but
#      on cgroup v1 WITHOUT swap accounting docker SILENTLY ignores --memory-swap,
#      defeating the host-starvation backstop. WSL2 ships cgroup v2; a stock
#      distro on cgroup v1 must boot with `cgroup_enable=memory swapaccount=1`.
#   2. inotify / fd sysctls. A 6-node kind cluster (cluster.yaml) exhausts the
#      default fs.inotify limits and nodes hang with "too many open files".
#
# All checks are ADVISORY by default (warn + exact remediation). Knobs:
#   PREFLIGHT_STRICT=1       a failed check aborts the run instead of warning
#   PREFLIGHT_FIX_SYSCTLS=1  raise any low sysctl automatically (needs root/sudo)
#   REQUIRED_SYSCTLS="k=min ..."  override the checked keys / thresholds
PREFLIGHT_STRICT="${PREFLIGHT_STRICT:-0}"
PREFLIGHT_FIX_SYSCTLS="${PREFLIGHT_FIX_SYSCTLS:-0}"
# Space-separated "key=min" pairs. A node is OK when its current value >= min.
REQUIRED_SYSCTLS="${REQUIRED_SYSCTLS:-fs.inotify.max_user_instances=512 fs.inotify.max_user_watches=524288 fs.file-max=2097152}"

# Reuse the caller's log/warn/die if present (run.sh defines log+die); otherwise
# provide minimal fallbacks so the lib is sourceable standalone.
command -v log  >/dev/null 2>&1 || log()  { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
command -v warn >/dev/null 2>&1 || warn() { printf '\033[1;33m>> %s\033[0m\n' "$*" >&2; }
command -v die  >/dev/null 2>&1 || die()  { printf '\033[1;31m!! %s\033[0m\n' "$*" >&2; exit 1; }

# Warn, or abort under PREFLIGHT_STRICT.
_pf_fail() { if [ "$PREFLIGHT_STRICT" = "1" ]; then die "preflight: $*"; else warn "preflight: $*"; fi; }

check_cgroups() {
  if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
    # cgroup v2 (unified): swap accounting is always available; the memory
    # controller just has to be delegated to the leaf where docker runs. Read the
    # tiny pseudo-file with a shell builtin + case match (no external grep, which
    # some shells shim into a wrapper that mishandles /sys files).
    local _controllers=""
    read -r _controllers < /sys/fs/cgroup/cgroup.controllers 2>/dev/null || true
    case " $_controllers " in
      *" memory "*) log "cgroup: v2 (unified), memory controller available — OK" ;;
      *) _pf_fail "cgroup v2 'memory' controller not delegated (see /sys/fs/cgroup/cgroup.subtree_control) — node memory caps will not apply" ;;
    esac
  elif [ -d /sys/fs/cgroup/memory ]; then
    # cgroup v1 (legacy): the --memory-swap == --memory ceiling is enforced ONLY
    # when swap accounting is compiled + booted in.
    if [ -e /sys/fs/cgroup/memory/memory.memsw.limit_in_bytes ]; then
      log "cgroup: v1 (legacy) with swap accounting — OK"
    else
      _pf_fail "cgroup v1 WITHOUT swap accounting: cap-kind-memory.sh's --memory-swap ceiling is SILENTLY IGNORED. Boot the kernel with 'cgroup_enable=memory swapaccount=1' (GRUB GRUB_CMDLINE_LINUX), or switch the host to cgroup v2 (systemd.unified_cgroup_hierarchy=1)."
    fi
  else
    _pf_fail "no recognizable cgroup memory hierarchy under /sys/fs/cgroup — cannot enforce node memory caps"
  fi
}

check_sysctls() {
  local pair key min cur
  for pair in $REQUIRED_SYSCTLS; do
    key="${pair%%=*}"; min="${pair#*=}"
    cur="$(sysctl -n "$key" 2>/dev/null || cat "/proc/sys/${key//.//}" 2>/dev/null || echo 0)"
    case "$cur" in ''|*[!0-9]*) cur=0 ;; esac
    if [ "$cur" -lt "$min" ]; then
      if [ "$PREFLIGHT_FIX_SYSCTLS" = "1" ]; then
        log "sysctl: raising $key ($cur -> $min)"
        sudo sysctl -w "$key=$min" >/dev/null 2>&1 || sysctl -w "$key=$min" >/dev/null 2>&1 \
          || _pf_fail "could not set $key=$min (need root / passwordless sudo)"
      else
        _pf_fail "$key=$cur is below the recommended $min — run: sudo sysctl -w $key=$min  (or set PREFLIGHT_FIX_SYSCTLS=1 to auto-apply)"
      fi
    else
      log "sysctl: $key=$cur (>= $min) — OK"
    fi
  done
}

# Run all host checks. Called from run.sh `up` before the cluster is created.
check_host() {
  log "host preflight: cgroups + sysctls (advisory; PREFLIGHT_STRICT=1 to enforce)"
  check_cgroups
  check_sysctls
}
