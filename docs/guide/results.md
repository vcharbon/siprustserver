# Measured results

This page holds the results of an endurance run on the lab cluster. The run sends a constant
load of calls for several hours and injects incidents on a schedule. The runner charges each
failed call to the incident that caused it and compares the total with a budget.

> The tables on this page are empty. A run of 3 hours is in progress, and its numbers fill them.

## The run

| Item | Value |
|---|---|
| Date | |
| Commit | |
| Duration | |
| Cluster | |
| Workers: count, CPU limit, memory limit | |
| Proxies: count, CPU limit, memory limit | |
| Sync level | `applied` |
| Run profile | |

## Load

| Stream | Call flow | Calls a second | Hold time | Calls placed |
|---|---|---|---|---|
| | | | | |

| Measure | Value |
|---|---|
| Calls placed | |
| Calls that ended as the scenario expects | |
| Calls that failed | |
| Highest count of calls in progress | |
| Setup time: median, 95th percentile, 99th percentile | |

## Incidents

One row for each kind of incident. The impact is the count of calls that did not end as
expected in the window of the incident. A window starts when the fault is injected and ends
after the recovery.

| Incident | Times fired | Calls in the windows | Setup failed | Calls cut | Dialog requests failed | Budget | Verdict | Recovery time |
|---|---|---|---|---|---|---|---|---|
| Worker killed | | | | | | | | |
| Worker drained | | | | | | | | |
| Proxy killed | | | | | | | | |
| Node stopped | | | | | | | | |
| Replication link cut | | | | | | | | |
| Packet loss on the SIP plane | | | | | | | | |
| Load spike | | | | | | | | |

Impact outside all windows: . It must be zero. A failed call that no incident explains is a
defect.

## Replication

These numbers show how often the replication round trip of [ha.md](ha.md) completed before the
sync cap.

| Measure | Value |
|---|---|
| Effects that left after the backup applied the write | |
| Effects that left at the sync cap | |
| Highest count of calls with no backup | |
| Copies turned live by a takeover | |

## Resources

| Measure | Worker | Proxy |
|---|---|---|
| CPU at the nominal load | | |
| Memory at the nominal load | | |
| Memory for one call | | Not applicable |
| Memory at the end of the run, against the start | | |

## CDRs

| Measure | Value |
|---|---|
| Calls placed | |
| CDRs received | |
| Calls with two CDRs | |
| Calls with no CDR | |

## How to run it again

```bash
deploy/helm/kind-lab.sh up
deploy/observability/install.sh --bootstrap
deploy/helm/kind-lab.sh deploy
cd tooling/hakit
uv run --locked hakit run <profile>
```

The runner writes `verdict.json` and `summary.md` in the run directory. The profiles are in
`tooling/hakit/content/profiles/`. [deploy/README.md](../../deploy/README.md) gives the system
requirements of the lab.
