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

# ── clock discipline (the WSL2 monotonic-anchored-clock failover hazard) ──────
#
# Every b2bua/proxy pod anchors its wall clock ONCE at process start
# (`sip_clock::Clock::system()` = `SystemTime::now()` captured at boot, then
# `+ monotonic elapsed` forever after — immune to later wall steps BY DESIGN).
# All kind nodes are docker containers sharing the host (WSL2 VM) kernel clock,
# so any two pods AGREE on `now_ms()` only as long as the host's REALTIME clock
# never STEPS between their two start instants.
#
# When it does step — WSL2's classic post-sleep drift, "corrected" by
# systemd-timesyncd's SNTP *step* (timesyncd steps large offsets; it does NOT
# slew) — a pod anchored BEFORE the jump keeps the old base while a pod anchored
# AFTER it (a chaos-restarted worker!) takes the new one. The two clocks then
# stay PERMANENTLY offset by the jump. `TimerService::restore` rebuilds a
# failed-over call's timers as `(fire_at_from_dead_node - our now_ms).max(0)`, so
# that offset turns a healthy 300 s keepalive into a PAST-DUE one that fires the
# instant the backup takes over the dialog — an OPTIONS burst onto both legs,
# mis-triaged as a SUT failover bug (root cause of endurance-20260630
# reinvite/unexpected/clear: an a-leg keepalive OPTIONS raced the failed-over
# re-INVITE ~290 s early). The b2bua's restore tolerates only ms–seconds of skew
# by design; keeping the host clock that stable is infra's job, not the SUT's.
check_clock() {
  # The hazard is specific to a STEPPING wall clock. A real NTP-disciplined
  # server (chrony slewing at <=500 ppm) never steps mid-run — nothing to check.
  grep -qi microsoft /proc/version 2>/dev/null \
    || { log "clock: non-WSL host — slewing NTP assumed, skipping"; return; }

  local synced
  synced="$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo unknown)"
  if [ "$synced" != "yes" ]; then
    _pf_fail "host clock is NOT NTP-synchronized (timedatectl NTPSynchronized=$synced). A cold/drifted WSL2 clock makes each pod anchor to a DIFFERENT base at start → cross-node timer skew (a reclaimed keepalive fires early on takeover). Enable it: sudo timedatectl set-ntp true"
    return
  fi

  # synchronized == yes, but by WHAT? systemd-timesyncd STEPS large offsets;
  # chrony (makestep-limited) SLEWS them. On WSL2 the post-sleep catch-up IS a
  # large offset → timesyncd STEPS it → pods either side of the step diverge.
  if systemctl is-active chrony chronyd >/dev/null 2>&1; then
    log "clock: NTP-synchronized via chrony (slewing) — OK"
  elif systemctl is-active systemd-timesyncd >/dev/null 2>&1; then
    warn "clock: disciplined by systemd-timesyncd (SNTP — STEPS large offsets, does not slew). On WSL2 a post-sleep catch-up STEPS the wall clock mid-run; b2bua pods anchored either side of the step diverge PERMANENTLY, so a failed-over call's reclaimed keepalive can fire immediately (mis-triaged as a SUT bug). Mitigate BOTH:
     (1) keep the host AWAKE for the whole run (no sleep/hibernate — that is what makes WSL2 drift then jump); and
     (2) switch to slewing chrony so any mid-run correction stays sub-second:
         sudo apt-get install -y chrony && sudo systemctl disable --now systemd-timesyncd
         printf 'makestep 1.0 3\\nmaxslewrate 500\\n' | sudo tee -a /etc/chrony/chrony.conf && sudo systemctl restart chrony"
  else
    _pf_fail "host clock reports synchronized but no known daemon (chrony/systemd-timesyncd) is active — cannot vouch for slew-vs-step behaviour; a mid-run wall step diverges b2bua pod clocks (cross-node timer skew on failover)."
  fi
}

# Best-effort one-time wall-clock resync, run at bring-up BEFORE any pod starts,
# so the whole cluster anchors to one accurate base. Safe here (a single step
# while nothing is anchored yet); NEVER call this mid-run (a step then would be
# the very divergence we are guarding against). No-op / non-fatal without sudo.
sync_clock_at_bringup() {
  grep -qi microsoft /proc/version 2>/dev/null || return 0
  if sudo -n timedatectl set-ntp true >/dev/null 2>&1; then
    # Nudge timesyncd to resync now so the step (if any) lands before pods anchor.
    sudo -n systemctl restart systemd-timesyncd >/dev/null 2>&1 || true
    log "clock: forced a pre-run wall-clock resync (all pods now anchor to one base)"
  else
    log "clock: skipping pre-run resync (needs passwordless sudo) — relying on the running NTP daemon"
  fi
}

# Run all host checks. Called from run.sh `up` before the cluster is created.
check_host() {
  log "host preflight: cgroups + sysctls + clock (advisory; PREFLIGHT_STRICT=1 to enforce)"
  check_cgroups
  check_sysctls
  check_clock
}
