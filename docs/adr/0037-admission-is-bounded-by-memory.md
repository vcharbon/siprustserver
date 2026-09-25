# Admission is bounded by memory; emergency calls keep a higher ceiling

**Status:** accepted (2026-09-25)

## Context

Admission sheds on rate (the CPS bucket), on the worker's own loop (the
panic-ELU backstop) and on ingress depth (the queue brake). None of them
bounds what the worker holds. Long calls at an admitted rate keep growing it,
emergency calls bypass the bucket, and a takeover adds a peer's calls all at
once. Each live call costs a roughly fixed amount, but the cost depends on the
call's traffic, and the transaction table and the backup replicas held for
peers grow beside it. A worker with no bound of its own grows until the kernel
kills it, and a kill drops every call it serves.

## Decision

1. **Three quantities bound new calls:** live calls (takeover copies
   included), live SIP transactions, and process RSS. The counts cover the
   costs known ahead; RSS covers everything the counts do not.
2. **Each bound has two ceilings.** At the `normal` ceiling a new
   non-emergency call is refused. At the `emergency` ceiling every new call is
   refused. Emergency calls keep priority between the two, and the higher
   ceiling still protects the process. Every ceiling is an explicit setting;
   none is derived from the host or the container, and all are off by default.
3. **The reject is a 503 with `Retry-After` and no `Reason`.** It is sent
   from the ingress brake while the last sample refuses the call's class, with
   no transaction created, and from the admission gate from exact counts,
   ahead of the CPS bucket so it spends no token. In-dialog requests are never
   refused.
4. **Backup replicas have their own ceilings:** a count and an RSS ceiling,
   set lower than the admission ones. At either, a replica of a call this node
   does not hold yet is not stored. Updates and deletes of held replicas, and a
   node's own calls coming back from a peer, always apply. A replica left out
   is stored by the call's next write that finds room, since every write
   carries the whole body.
5. **RSS is sampled, counts are exact.** RSS is read every 100 ms through an
   injectable `SystemProbe`; tests drive it through a simulated one.

## Consequences

- A call whose replica was left out has no backup until its next write lands;
  a primary failure in that window loses it. The shed is counted
  (`b2bua_repl_backup_shed_total`), never silent.
- RSS lags up to one sample and stays high after a drain while the allocator
  keeps freed pages, so the RSS ceilings must leave room below the process
  limit; the counts react at once.
- A transaction ceiling also counts the transactions of rejected and finished
  INVITEs that linger for their timers, so it is sized from the offered rate,
  not from live calls alone.
