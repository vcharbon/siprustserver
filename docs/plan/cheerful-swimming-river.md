# Endurance + Chaos suite enhancement & 2h run

## Context

The k8s chaos suite (`deploy/k8s/chaos.sh`) today proves exactly one thing: a
single worker-kill mid-hold, asserting call survival via the P2P replica. The
load runner (`run.sh`) does a CPU/RSS sweep. Neither exercises a realistic
endurance profile (mixed long + short traffic + abuse), neither kills the
**proxy**, and SIPp outcomes are only read post-hoc by grepping the final job
log — there is **no live signal** in Grafana for concurrent calls, error
classes, or chaos events.

The user wants:
1. A baseline long-call SIPp stream, plus three new chaos primitives — **kill
   primary proxy**, **always-on abuse traffic @1cps**, **200cps traffic peaks**.
2. A **SIPp metrics exporter** turning SIPp's stat CSV (errors, concurrent
   calls, every failure class) into Prometheus metrics, scraped the same way the
   rest of the stack is, with a **dedicated Grafana dashboard**.
3. A **2-hour endurance run**: long calls @5cps + short calls @100cps + abuse
   @1cps, chaos event every 15 min, with chaos/metric monitoring delegated to a
   subagent; small fixes (<100 LOC) applied and the run relaunched.

Confirmed decisions: **smoke-validate (~10 min) first, then auto-launch the full
2h**; long-call hold **300s (~1500 concurrent at 5cps)**; on a <100 LOC fix,
**restart a clean 2h clock**.

Environment is already favorable: the `sip-e2e` kind cluster (k8s **v1.34** →
native sidecars GA) is up with 2 workers + proxy + UAS, and the host
observability stack (VictoriaMetrics `:8428`, Grafana `:3333`, datasource
uid=`vm`) is healthy. vmagent already scrapes any pod carrying
`prometheus.io/scrape: "true"` (`deploy/observability/kind-addons/vmagent.yaml`).

## Approach

### 1. SIPp stat → Prometheus exporter (new)

- **`deploy/k8s/sipp/exporter/sipp_stat_exporter.py`** (new, stdlib only:
  `http.server` + threads). Tails SIPp's `-trace_stat` CSV (semicolon-separated;
  parse the header row → column-name→index map so it's robust to SIPp column
  drift). On each `/metrics` scrape it reads the **last** data row and emits,
  labeled by `scenario`/`role`/`job` (from env):
  - `sipp_current_calls` (concurrent — the headline gauge)
  - `sipp_calls_created_total`, `sipp_successful_calls_total`, `sipp_failed_calls_total`
  - `sipp_failed_total{cause=…}` — one series per SIPp failure column
    (`max_udp_retrans`, `timeout_recv`, `timeout_send`, `call_rejected`,
    `unexpected_msg`, `cannot_send`, `tcp_connect`, `tcp_closed`,
    `regexp`, `congestion`, `dead_call_reused`)
  - `sipp_retransmissions_total`, `sipp_out_of_call_msgs_total`, `sipp_dead_call_msgs_total`
  - `sipp_call_rate`, `sipp_response_time_ms`, `sipp_up`
  Mounted from a **`sipp-exporter` ConfigMap** so the script can be edited
  without rebuilding the image.
- **`deploy/k8s/sipp/Dockerfile`** — add `python3` to the final runtime stage
  (one apt line; no pip). This is the only image change; the exporter runs from
  the same `sipp:dev` image as SIPp itself.

### 2. Wire the exporter into every SIPp job — "reporting wired like cluster start"

- **`deploy/k8s/manifests/40-sipp-uac-job.yaml`** — three changes:
  - SIPp writes stats to a shared `emptyDir`: change `-stf /dev/stdout` →
    `-stf /stats/stat.csv` and add `-fd 1` (1s flush). Final summary still hits
    stdout, so existing `grep 'Successful call'` parsing keeps working.
  - Add a **native sidecar** (`initContainers:` entry with
    `restartPolicy: Always`, GA on v1.34 → excluded from Job-completion
    accounting, so `wait --for=condition=complete` still works) named
    `stat-exporter`: same `sipp:dev` image, `python3 /exporter/...`, mounts the
    `stats` emptyDir + `sipp-exporter` ConfigMap, `containerPort: 9035`.
  - Pod template gets `prometheus.io/scrape:"true"`, `…/port:"9035"`,
    `…/path:"/metrics"` annotations + a templated `${ROLE}`/`${SCENARIO}` so
    long/short/abuse/peak jobs self-label. vmagent picks them up automatically —
    identical scrape path to proxy/worker pods.
- **`deploy/k8s/run.sh` `deploy()`** — also build the `sipp-exporter` ConfigMap
  from `sipp/exporter/` (next to the existing `sipp-scenarios` CM) so any job —
  load sweep, chaos, or endurance — emits metrics by construction. This is what
  makes the "reporting SIPp wired the same way as cluster start" guarantee hold:
  it goes through the same `deploy` path, not an ad-hoc manifest.

### 3. New baseline scenario

- **`deploy/k8s/sipp/scenarios/uac-endurance-long.xml`** (new) — clone of
  `uac-endurance-short.xml` with `<pause milliseconds="300000"/>` (300s hold) →
  deterministic ~1500 concurrent dialogs at 5cps. (The existing
  `uac-long-options.xml` is keepalive-driven and timing-coupled to the SUT — too
  indirect for a clean baseline.)

### 4. New chaos primitives

- **`deploy/k8s/chaos.sh`** — add, alongside the existing worker-kill:
  - `kill_proxy()` — `kubectl delete pod -l app=sip-front-proxy --force`
    (single-replica proxy → new-call outage until the Deployment restarts it;
    in-dialog traffic via Record-Route pins back once it's back).
  - `peak()` — launch a `uac-endurance-short.xml` burst job at `PEAK_CAPS=200`
    for `PEAK_SECS=30`, then delete it (the 200cps traffic-peak chaos).
  - `abuse_up()` / `abuse_down()` — long-lived job at `ABUSE_CAPS=1` cycling an
    abuse scenario (`uac-abuse-options-flood.xml` → `…-reinvite-flood.xml` →
    `…-ghost-after-ack.xml`). Started for the whole endurance window.
  - `push_metric()` helper — POST chaos outcomes to VM
    `http://127.0.0.1:8428/api/v1/import/prometheus` so each event lands in
    Grafana: `sip_chaos_event_total{type,result}` and `sip_chaos_active{type}`.
  - New subcommands: `proxykill`, `peak`, `abuse {up|down}`.

### 5. Endurance orchestrator (new)

- **`deploy/k8s/endurance.sh`** (new) — the 2h driver:
  - **Wire-up (same path as cluster start):** rebuild `sipp:dev` (now with
    python3) + `kind load`, then `run.sh deploy` (rebuilds both ConfigMaps,
    re-resolves proxy worker IPs). REPL on, 2 workers.
  - **Baseline streams** (long-lived jobs): long `uac-endurance-long.xml`
    @`LONG_CPS=5`, short `uac-endurance-short.xml` @`SHORT_CPS=100`, abuse
    @`ABUSE_CAPS=1`.
  - **Chaos loop:** every `CHAOS_INTERVAL=900s` for `DURATION=7200s`, cycle
    `kill_worker → kill_proxy → peak(200) → …`. Each event: timestamp, run,
    measure success-rate from live exporter metrics, `push_metric`, append a row
    to `results/endurance-<ts>/events.jsonl`.
  - **`SMOKE=1`** mode: `DURATION=600`, `CHAOS_INTERVAL=180`, one of each event —
    the ~10-min validation gate.
  - Designed to run under `run_in_background`; structured JSONL + VM metrics are
    the monitoring surface.

### 6. Dedicated Grafana dashboard (new)

- **`deploy/observability/stack/grafana/dashboards/sipp-endurance-chaos.json`**
  (new, `schemaVersion: 39`, datasource uid `vm`, auto-loaded by bind-mount +
  `install.sh --apply`). Rows:
  - **Traffic:** `sipp_call_rate` and `sipp_current_calls` stacked by
    `role`/`scenario`; calls-created rate.
  - **Outcomes:** successful vs failed rate; success-ratio %; total failed.
  - **Failure causes:** `sipp_failed_total` by `cause` (timeseries + table);
    retransmissions; timeouts.
  - **Chaos:** `sip_chaos_event_total` by type/result (state-timeline + table),
    `sip_chaos_active`, worker/proxy restarts
    (`kube_pod_container_status_restarts_total`), pod up/down. Chaos events also
    wired as dashboard annotations.

### 7. Run, monitor, fix loop

1. Apply files; rebuild+load `sipp:dev`; `run.sh deploy`; `install.sh --apply`
   to load the dashboard.
2. **Smoke run** (`SMOKE=1`) in background; verify exporter series appear in VM
   (`/api/v1/query`) and the dashboard renders, and each chaos type emits an
   event metric.
3. **Launch the full 2h run** in background.
4. **Monitor** via periodic wake-ups: read `events.jsonl` + query VM for
   failure-class spikes / chaos `result="fail"`.
5. On a failure, **delegate a thorough investigation to a subagent**. If the fix
   is <100 LOC, apply it and **relaunch a clean 2h run**; otherwise stop and
   report findings.

## Files

| File | Change |
|------|--------|
| `deploy/k8s/sipp/exporter/sipp_stat_exporter.py` | new — stat-CSV→Prometheus exporter |
| `deploy/k8s/sipp/Dockerfile` | add `python3` to runtime stage |
| `deploy/k8s/sipp/scenarios/uac-endurance-long.xml` | new — 300s-hold long baseline |
| `deploy/k8s/manifests/40-sipp-uac-job.yaml` | stats volume + native sidecar + scrape annotations + role/scenario labels |
| `deploy/k8s/run.sh` | build `sipp-exporter` ConfigMap in `deploy()` |
| `deploy/k8s/chaos.sh` | `kill_proxy`/`peak`/`abuse`/`push_metric` + subcommands |
| `deploy/k8s/endurance.sh` | new — 2h orchestrator (baseline streams + chaos loop + SMOKE) |
| `deploy/observability/stack/grafana/dashboards/sipp-endurance-chaos.json` | new — dedicated dashboard |

## Verification

- **Exporter unit-ish:** feed a captured SIPp stat CSV to the exporter locally,
  `curl localhost:9035/metrics`, confirm all series + correct values.
- **Wiring:** after `run.sh deploy` + a short UAC job, query VM
  `http://127.0.0.1:8428/api/v1/query?query=sipp_current_calls` → non-empty;
  target shows healthy in `install.sh --status`.
- **Chaos primitives:** `chaos.sh proxykill`, `chaos.sh peak`, `chaos.sh abuse up`
  each succeed and push a `sip_chaos_event_total` sample (verify via VM query).
- **Dashboard:** loads in Grafana (`:3333`), all panels populate during the
  smoke run.
- **Smoke gate:** `SMOKE=1 endurance.sh` completes ~10 min with one of each
  chaos event recorded in `events.jsonl` and visible on the dashboard.
- **Full run:** `events.jsonl` accrues ~8 chaos events over 2h; success-rate and
  failure-class panels stay within thresholds; any `result="fail"` triggers the
  subagent investigation path.
