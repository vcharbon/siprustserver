# What HA promises, and what it does not

The replicated worker pool behind the front proxy pair keeps established and
setting-up calls through the loss of one worker or one proxy. This page states
the limits of that promise. Each line names the decision that holds it; a
product built on this stack states its own additions beside it.

## Promised

- **A worker lost whole keeps its calls.** Every call has one backup on another
  worker (ADR-0011). When a primary dies, its backup takes over the calls with
  an INVITE in flight at once, the quiet ones on their next request
  (ADR-0047), and serves them to their end with one CDR (ADR-0055).
- **A planned exit hands its calls over.** A draining worker serves its calls
  until they clear, its backups hold them, or the grace ends (ADR-0031); the
  chart refuses a pod grace that would cut the drain.
- **A proxy lost keeps the calls it was routing.** In-dialog requests carry
  everything a proxy needs to route them (the stickiness cookie, ADR-0046), so
  the other proxy routes them without state.
- **Teardown is never refused** under overload (ADR-0037 item 8).
- **Every parameter set is checked whole.** A worker or proxy whose
  configuration breaks a relation between its values refuses to start, naming
  every value involved; the chart refuses to render a set whose pod spec and
  runner env disagree.

## Not promised

- **`applied` is a capped wait, not durability.** At sync level `applied` an
  effect waits for its backup at most the sync cap (100 ms), then leaves
  anyway (ADR-0055 §3). A worker lost within that window loses the effects its
  backup had not applied; the parties' retransmissions recover most of them.
- **Split brain is reconciled, not prevented.** Two copies of a call can serve
  at once across a partition; the versions are folded when the planes heal
  (ADR-0014, ADR-0031 D3). Until then each side answers what reaches it.
- **A backend request can be sent twice, or lost with an ending call.** A
  request in flight when its worker dies is sent again by the copy that takes
  an `Active` call over (ADR-0039 decision 5): a decision or limiter backend
  sees it twice and answers both. A request is made for its answer, never for
  its effect at the service, so an ending call sends none again.
- **The replica TTL is not a reboot budget.** `B2BUA_REBOOT_BUDGET_SEC` bounds
  how long a backup keeps a replica no write refreshed. A primary down longer
  than that loses the quiet calls whose replicas expired; the TTL is sized
  against the keepalive round and the INVITE bound, not against a node's
  repair time.
- **CDRs are not durable end to end.** The worker buffers them in memory and
  drops on overload; the broker queue is bounded (`x-max-length`, drop-head);
  a worker lost whole loses the CDRs it had not published. Counts are exposed
  at each stage (`b2bua_cdr_dropped_total`, the queue depth, the consumer's
  counters).
- **The call limiter fails open.** An admit with no usable answer within its
  budget admits the call (ADR-0040, ADR-0043): a limiter outage over-admits,
  it never refuses calls.
- **New calls are refused while a survivor is above its ceilings.** The
  copies a takeover turns live are never refused, but they count: until the
  survivor's live calls fall under its admission ceilings, new calls are
  refused, emergency calls included under the default emergency margin
  (TUNING.md, the worked example).
- **An unbacked call stays admissible.** Past a backup ceiling, or with no
  live backup, a new call is admitted without a backup and is lost with its
  worker; the shortfall is counted, never refused (ADR-0037 item 4).

## Transparent faults

Seen by neither party beyond retransmissions or a short setup delay:

| fault | what holds it |
|---|---|
| one worker killed, drained or restarted | its backup takes its calls over (ADR-0047, ADR-0031) |
| one proxy killed or restarted | the other proxy routes by cookie (ADR-0046) |
| a replication link lost and regained | the puller resumes from its watermark (ADR-0045) |
| a decision or limiter backend slow within its budget | the request's deadline (ADR-0039) |

Not transparent: both workers of a call lost together, both proxies lost
together, a total decision-backend outage (new calls are refused), and the
limits above.
