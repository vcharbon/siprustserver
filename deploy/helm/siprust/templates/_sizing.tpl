{{/*
The capacity sizing of `sizing` (values.yaml): the workers' and the proxies'
computed env and memory, as JSON. Takes the chart's values (`.Values` here, a
parent's `.Values.<subchart>`); an empty object when sizing is off.

Per worker, W workers, a profile of `cps` new calls a second held `hold`
seconds (ringing included), ringing `ring`:
- calls: the mean share cps × hold / W, + callMarginPct, up to a thousand;
  emergency + emergencyPct; backups = emergency (every call a peer admits).
- a live call's bytes: answered + (ringing − answered) × ring / hold.
- transactions per call a second: txnSecondsPerCall + 2 × ring (a ringing
  call's two INVITE transactions). The ceiling covers the larger of a lone,
  empty worker taking the whole rate (cps × that) and a worker at its call
  ceiling's rate (calls / hold × that, + its keepalive probes: two legs held
  Timer F each B2BUA_KEEPALIVE_SEC per call), + txnMarginPct, up to a
  thousand; emergency + emergencyPct.
- RSS ceilings: base + calls × call + backups × replica + transactions × txn.
- memory: the larger of a full takeover (both call ceilings live, the
  emergency transactions, the reserve, every outbound connection) and the
  emergency RSS ceiling plus what a takeover adds (ADR-0037 item 7), over
  1 − slackPct. The reserve: two teardown transactions and the keepalive
  probes of every call held, and cps × Timer F of refusals.
Per proxy, which takes the whole profile alone:
- self-gate: cps + gateMarginPct, its burst a second of it.
- memo: the self-gate's calls of `cancel_lru::entry_ms_per_call` at the mean
  ring, the INVITEs per call and the INVITE entry cap (inviteTxnTimeoutSec +
  Timer H), with the proxy start check's quarter of headroom, up to the 16
  stripes; its bytes at the heaviest entry, 2 560 B.
- memory: the memo's bytes, each entry's overhead and the base, over
  1 − slackPct.
*/}}
{{- define "siprust.sizing" -}}
{{- $v := . }}
{{- $s := $v.sizing }}
{{- if not $s.enabled }}
{{- dict | toJson }}
{{- else }}
{{- $p := $s.profile }}
{{- $m := $s.memory }}
{{- range $k := list "cps" "holdSeconds" "ringSeconds" "invitesPerCall" }}
{{- if kindIs "invalid" (index $p $k) }}{{- fail (printf "sizing.profile.%s is required with sizing.enabled" $k) }}{{- end }}
{{- end }}
{{- range $k := list "callMarginPct" "emergencyPct" "txnMarginPct" "gateMarginPct" "txnSecondsPerCall" }}
{{- if kindIs "invalid" (index $s $k) }}{{- fail (printf "sizing.%s is required with sizing.enabled" $k) }}{{- end }}
{{- end }}
{{- range $k := list "baseBytes" "answeredCallBytes" "ringingCallBytes" "replicaBytes" "txnBytes" "outboundConnections" "connectionBytes" "slackPct" "proxyBaseBytes" "memoEntryOverheadBytes" }}
{{- if kindIs "invalid" (index $m $k) }}{{- fail (printf "sizing.memory.%s is required with sizing.enabled" $k) }}{{- end }}
{{- end }}
{{- if kindIs "invalid" $v.inviteTxnTimeoutSec }}{{- fail "inviteTxnTimeoutSec is required with sizing.enabled: the memo's INVITE entry cap derives from it" }}{{- end }}
{{- $mib := 1048576 }}
{{- $cps := int64 $p.cps }}
{{- $hold := int64 $p.holdSeconds }}
{{- $ring := int64 $p.ringSeconds }}
{{- if gt $ring $hold }}{{- fail (printf "sizing.profile.ringSeconds=%d exceeds holdSeconds=%d, which includes the ring" $ring $hold) }}{{- end }}
{{- $workers := int64 $v.worker.replicas }}
{{- $keepalive := include "siprust.envInt" (list ($v.worker.env | default dict) "B2BUA_KEEPALIVE_SEC" 300) | int64 }}
{{- $slack := int64 $m.slackPct }}
{{- /* Worker calls. */ -}}
{{- $share := div (mul $cps $hold) $workers }}
{{- $calls := mul (div (add (div (mul $share (add 100 (int64 $s.callMarginPct))) 100) 999) 1000) 1000 }}
{{- $callsE := mul (div (add (div (mul $calls (add 100 (int64 $s.emergencyPct))) 100) 999) 1000) 1000 }}
{{- $backups := $callsE }}
{{- $answered := int64 $m.answeredCallBytes }}
{{- $call := add $answered (div (mul (sub (int64 $m.ringingCallBytes) $answered) $ring) $hold) }}
{{- $replica := int64 $m.replicaBytes }}
{{- $txn := int64 $m.txnBytes }}
{{- $base := int64 $m.baseBytes }}
{{- /* Worker transactions. */ -}}
{{- $perCps := add (int64 $s.txnSecondsPerCall) (mul 2 $ring) }}
{{- $lone := mul $cps $perCps }}
{{- $survivor := add (div (mul $calls $perCps) $hold) (div (mul $calls 64) $keepalive) }}
{{- $peak := max $lone $survivor }}
{{- $txns := mul (div (add (div (mul $peak (add 100 (int64 $s.txnMarginPct))) 100) 999) 1000) 1000 }}
{{- $txnsE := mul (div (add (div (mul $txns (add 100 (int64 $s.emergencyPct))) 100) 999) 1000) 1000 }}
{{- /* Worker RSS ceilings and memory. */ -}}
{{- $rss := add $base (mul $calls $call) (mul $backups $replica) (mul $txns $txn) }}
{{- $rssE := add $base (mul $callsE $call) (mul $backups $replica) (mul $txnsE $txn) }}
{{- $rssMi := div (add $rss (sub $mib 1)) $mib }}
{{- $rssEMi := div (add $rssE (sub $mib 1)) $mib }}
{{- $held := add $callsE $backups }}
{{- $reserve := add (mul $held 2) (div (mul $held 64) $keepalive) (mul $cps 32) }}
{{- $conns := mul (int64 $m.outboundConnections) (int64 $m.connectionBytes) }}
{{- $full := add $base (mul $held $call) (mul (add $txnsE $reserve) $txn) $conns }}
{{- $grow := add (mul $rssEMi $mib) (mul $backups (sub $call $replica)) (mul $reserve $txn) }}
{{- $workerMi := div (add (div (mul (max $full $grow) 100) (sub 100 $slack)) (sub $mib 1)) $mib }}
{{- /* Proxy. */ -}}
{{- $gate := div (add (mul $cps (add 100 (int64 $s.gateMarginPct))) 99) 100 }}
{{- $invites := int64 (mulf (float64 $p.invitesPerCall) 1000) }}
{{- $further := sub $invites 2000 }}
{{- $ttl := add (mul (int64 $v.inviteTxnTimeoutSec) 1000) 32000 }}
{{- $inviteMs := add $ttl (mul $ring 1000) (div (mul $further $ttl) 1000) }}
{{- $entryMs := add $inviteMs 64000 288000 (div (mul $further 96000) 1000) }}
{{- $callSeconds := div $entryMs 1000 }}
{{- $entries := mul (div (add (div (add (mul $gate $callSeconds 5) 3) 4) 15) 16) 16 }}
{{- $memoBytes := mul $entries 2560 }}
{{- $proxyNeed := add $memoBytes (mul $entries (int64 $m.memoEntryOverheadBytes)) (int64 $m.proxyBaseBytes) }}
{{- $proxyMi := div (add (div (mul $proxyNeed 100) (sub 100 $slack)) (sub $mib 1)) $mib }}
{{- $out := dict
  "workerEnv" (dict
    "B2BUA_MAX_CALLS" (toString $calls)
    "B2BUA_MAX_CALLS_EMERGENCY" (toString $callsE)
    "B2BUA_REPL_BACKUP_MAX_CALLS" (toString $backups)
    "B2BUA_MAX_TXNS" (toString $txns)
    "B2BUA_MAX_TXNS_EMERGENCY" (toString $txnsE)
    "B2BUA_MAX_RSS" (printf "%dMi" $rssMi)
    "B2BUA_MAX_RSS_EMERGENCY" (printf "%dMi" $rssEMi)
    "B2BUA_REPL_BACKUP_MAX_RSS" (printf "%dMi" $rssEMi))
  "workerMemory" (printf "%dMi" $workerMi)
  "proxyEnv" (dict
    "PROXY_SELF_GATE_CPS_RATE" (toString $gate)
    "PROXY_SELF_GATE_CPS_SIZE" (toString $gate)
    "PROXY_BRANCH_MEMO_MAX_ENTRIES" (toString $entries)
    "PROXY_BRANCH_MEMO_MAX_BYTES" (toString $memoBytes)
    "PROXY_BRANCH_MEMO_MEAN_RING_MS" (toString (mul $ring 1000))
    "PROXY_BRANCH_MEMO_INVITES_PER_CALL" (toString $p.invitesPerCall))
  "proxyMemory" (printf "%dMi" $proxyMi)
  "figures" (dict "callBytes" $call "txnsPerCps" $perCps "lone" $lone "survivor" $survivor "reserve" $reserve "full" $full "grow" $grow "entrySeconds" $callSeconds "proxyNeed" $proxyNeed) }}
{{- $out | toJson }}
{{- end }}
{{- end }}
