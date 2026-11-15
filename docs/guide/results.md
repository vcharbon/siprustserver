# Measured results

This page holds the results of an endurance run on the lab cluster. The run sends a constant
load of calls for two hours and injects incidents on a schedule. The runner charges each
failed call to the incident that caused it and compares the total with a budget.

> **Read this first.** The schedule of this run put the windows closer together than the
> longest call. A call can then meet two faults in a row. 521 calls were lost that way (see
> [Double failures](#double-failures)). Each fault alone stays within the behaviour that
> [ha.md](ha.md) states; no budget states two faults on one call. These losses come from the
> test design. The `endurance` profile now spaces its windows by the longest call, and
> `hakit validate` refuses a profile that does not.

## The run

| Item | Value |
|---|---|
| Date | 15 November 2026, 08:07:15 to 10:10:58 UTC |
| Commit | `cd1b37041` |
| Duration | 2 h 3 min of load, then the drain |
| Cluster | kind on one host (24 CPUs, 30 GiB): a control plane and five nodes. Two nodes for the proxies, two for the workers, one for the broker, the call limiter and the CDR consumer |
| Workers: count, CPU limit, memory limit | 2, no CPU limit, 3278 MiB |
| Proxies: count, CPU limit, memory limit | 2, no CPU limit, 3019 MiB |
| Sync level | `applied` |
| Chart values | `values-kind.yaml`, then `values-kind-endurance.yaml` |
| Run profile | The streams of `endurance`, with every base incident fired twice (51 windows). The windows were 33 s to 194 s apart (median 63 s). `endurance` fires each incident once, with each window at least 250 s after the previous one |

## Load

| Stream | Call flow | Calls a second | Hold time | Calls placed |
|---|---|---|---|---|
| `basic` | INVITE, 2 s of ring, answer, BYE | 180 | 20 s | 1,296,048 |
| `reinvite` | `basic`, with three re-INVITEs from the caller | 180 | 20 s | 1,295,984 |
| `emergency` | `basic` with `Resource-Priority` (RFC 4412) | 20 | 20 s | 144,007 |
| `callee_reinvite` | The callee sends two re-INVITEs and hangs up at 180 s | 20 | 180 s (the caller gives up at 240 s) | 143,999 |
| `burst` | `basic`, for the spike incidents only | 200 | 2 s | 53,858 |
| `flooding` | `basic` with an OPTIONS flood, for the abuse incident | 5 | 1 s | 599 |
| `orphaned` | `basic`, left without its BYE by the orphan incident | 10 | 60 s | 400 |

| Measure | Value |
|---|---|
| Calls placed | 2,934,895 |
| Calls that ended as the scenario expects | 2,932,295 (99.91 %) |
| Calls that failed | 2,144: 1,246 refused, 368 with no final, 530 cut. The 400 orphaned calls end by design without a record |
| Calls with no record | 56 `callee_reinvite` calls that the generator aborted inside two windows (see [Integrity](#integrity)) |
| Highest count of calls in progress | 17,400 on the two workers; 12,800 on one worker while the other was down |
| Setup time (INVITE to the first provisional): median, 95th percentile, 99th percentile | 5 ms, 6 ms, 154 ms. Outside the windows: 4 ms, 6 ms, 6 ms |

## Verdict

The runner computes `invalid`: 56 calls have no record (a `record_gap`; see
[Integrity](#integrity)). The other findings are:

- In the four CPU starvation windows, 313 callers got a provisional and then no final. The
  `cpu-overload` budget allows none. 71 emergency callers and 33 spike callers are among
  them. 2 emergency callers also got their final late.
- In the second forced pod delete, 3 callers got a provisional and then no final.
- 63 `reinvite` setups took more than 1 s outside every window.
- The 521 calls of the double failure below.

No other window has a finding. Each stream came back within its recovery bound after each
incident.

## Incidents

One row for each kind of incident. A window starts when the fault is injected and ends after
the recovery. "Setup failed" counts the refused setups and the setups with no final. "Calls
cut" and "Dialog requests failed" count the calls that the window may have changed.

| Incident | Times fired | Calls set up in the windows | Setup failed | Calls cut | Dialog requests failed | Budget | Findings | Longest recovery |
|---|---|---|---|---|---|---|---|---|
| Worker killed (`kill_worker`) | 2 | 14,635 | 0 | 1 | 1 | `worker-kill` | 0 | 2 s |
| Worker drained (`kill_worker_graceful`) | 2 | 16,758 | 0 | 0 | 0 | `worker-drain` | 0 | 6 s |
| Worker killed and kept down for 60 s (`kill_worker_held`) | 2 | 61,907 | 52 | 1 | 1 | `worker-kill` | 0 | 0 s |
| Proxy killed (`kill_proxy`) | 2 | 37,444 | 0 | 0 | 0 | `proxy-failover` | 0 | 0 s |
| Dependency pod killed (`pod_kill`) | 2 | 14,173 | 0 | 0 | 0 | `dependency-fault` | 0 | 2 s |
| Dependency pod restarted (`pod_restart`) | 2 | 15,563 | 0 | 0 | 0 | `dependency-fault` | 0 | 2 s |
| Dependency or broker frozen (`pod_freeze`) | 4 | 66,684 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| Network delay (`net_delay`) | 2 | 39,556 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| Network loss (`net_loss`) | 2 | 39,511 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| TCP reset (`tcp_reset`) | 2 | 40,758 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| Worker starved of CPU (`cpu_starve`) | 2 | 61,319 | 558 | 0 | 76 | `cpu-overload` | 5 | 0 s |
| Node stopped (`node_stop`) | 2 | 68,744 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| Replication link cut (`partition`) | 2 | 37,713 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| Packet loss on the SIP plane (`sip_loss`) | 2 | 40,075 | 0 | 1 | 0 | `lossy-plane` | 0 | 0 s |
| Broker store stalled (`io_stall`) | 2 | 37,479 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| Clock shifted (`clock_skew`) | 2 | 37,262 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| External plane cut (`cut_external_plane`) | 2 | 37,219 | 0 | 0 | 0 | `proxy-cut` | 0 | 0 s |
| Load spike (`spike`) | 7 | 199,108 | 0 | 0 | 0 | `spike` | 0 | 0 s |
| CPU starvation and a spike (`cpu_starve+spike`) | 2 | 57,664 | 1,001 | 0 | 4 | `cpu-overload` | 10 | 0 s |
| Pod force-deleted (`force_delete`) | 2 | 14,180 | 3 | 527 | 521 | `worker-kill` | 2 | 2 s |
| Orphaned dialogs (`orphan`) | 2 | 30,813 | 0 | 0 | 0 | `dependency-fault` | 0 | 0 s |
| In-dialog flood (`abuse`) | 2 | 93,632 | 0 | 0 | 0 | `abuse` | 0 | 0 s |

Impact outside all windows: 0 failed calls. Of the calls that met no window from INVITE to end
(1,298,000 calls of the four steady streams), none failed. The verdict lists 488 cuts as
outside every window: they are 487 calls of the double failure, whose BYE came after the
window closed, and one `reinvite` call.

## Double failures

1. 08:47:22 to 08:48:19: the CPU starvation and spike window. One worker runs at half a core,
   and 200 more calls a second arrive. The backlog to that worker passes its high mark. The
   other worker starts its new calls with no backup, and they keep none for their whole life
   ([ha.md](ha.md)). `b2bua_repl_calls_unbacked` on the other worker goes to 9,461, and 568 calls
   stay unbacked after the window closes.
2. The `basic`, `reinvite` and `emergency` calls of that time end normally within a minute. The
   `callee_reinvite` calls last 180 s.
3. 08:50:42: the next window force-deletes the worker that holds them. The unbacked gauge reads
   521 at that instant.
4. 08:51:43 to 08:52:09: each of the 521 callers sends its BYE and gets `481`. That is every
   `callee_reinvite` call set up from 08:47:37 to 08:48:03.

Each fault alone keeps its budget. The calls were lost because they met both. A call meets
one fault at most when each window opens at least the longest call life after the previous
window closes. The longest call life of this load is 250 s (10 s of setup and 240 s of hold).

## Replication

These numbers show how often the replication round trip of [ha.md](ha.md) completed before the
sync cap.

| Measure | Value |
|---|---|
| Effects that left after the backup applied the write | 31,670,770 (93.9 %) |
| Effects that left at the sync cap | 461,741 (1.4 %) |
| Effects with no live backup to wait for (a worker down or restarting, or calls with no backup) | 1,602,771 (4.8 %) |
| Effects that left because the link to the backup went down | 216 |
| Highest count of calls with no backup | 10,000 on one worker |
| Copies refused by the backup | 0 |
| Copies turned live by a takeover | 28,533 |

## Resources

The workers restart during the run, so "start" and "end" are different processes.

| Measure | Worker | Proxy |
|---|---|---|
| CPU at the nominal load: median, 95th percentile | 0.90 core, 1.14 core | 0.14 to 0.27 core, 0.22 to 0.34 core |
| Memory at the nominal load: median, highest | 715 to 740 MiB, 977 MiB | 424 to 462 MiB, 598 MiB |
| Memory for one call | Not measured in this run. The chart sizing uses 49,300 bytes for an answered call | Not applicable |
| Memory at rest after the run, against the start | 372 to 406 MiB, against 313 to 339 MiB | 394 to 396 MiB, against 113 to 186 MiB |

## CDRs

The profile does not join the CDRs with the call records, so this run does not count calls
with two CDRs or with none.

| Measure | Value |
|---|---|
| Calls placed | 2,934,895 |
| CDRs the workers wrote | 2,807,194 |
| CDRs the workers dropped | 113,254 |
| CDRs the consumer received | 2,818,284 |

The workers drop a CDR they cannot publish to the broker. 112,916 of the drops are in the
windows that stop the broker: frozen (26,018), its node stopped (59,105), its store stalled
(27,793). 338 drops are in the 35 s after the first node stop window closed, while the broker
came back.

## Integrity

The generator aborted 56 `callee_reinvite` calls and wrote no record for them. SIPp counts
them as `FailedCall`. For each one, SIPp logs "ACK CSeq value does NOT match value of related
INVITE CSeq -- aborting call": the ACK it received for a callee re-INVITE did not carry the
CSeq of that re-INVITE. 27 aborts are in the first CPU starvation and spike window and 29 in
the second. The calls had started 2 to 3 minutes before.

The run is `invalid` because of these calls. This run reads them as a `record_gap`: its
generator counters predate the count of SIPp's failed calls. A run of the current runner reads
them as a `generator_failure` that names each window a `callee_reinvite` call (up to 304 s)
can have met before an abort: the two CPU starvation and spike windows, and the windows just
before them. The profile's `record_shortfall_pct`
(0.1 %) tolerates only failures that no window can have touched. A failure inside a window can
be a call that the fault broke, and the window's budget cannot judge a call with no record.
Whether the system under test or the generator causes the mismatch is not yet known.

## How to run it again

```bash
deploy/helm/kind-lab.sh up
deploy/observability/install.sh --bootstrap
deploy/helm/kind-lab.sh deploy -f deploy/helm/siprust/values-kind-endurance.yaml
cd tooling/hakit
uv run --locked hakit run endurance
```

Run it on a quiet host: a generator stall from host memory or disk pressure makes the run
`invalid`. The runner writes `verdict.json` and `summary.md` in the run directory. The
profiles are in `tooling/hakit/content/profiles/`. [deploy/README.md](../../deploy/README.md)
gives the system requirements of the lab.
