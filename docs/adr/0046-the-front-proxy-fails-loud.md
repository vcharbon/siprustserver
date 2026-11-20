# The front proxy fails loud

**Status:** accepted. D4's virtual address is superseded by ADR-0056, and a total backend outage
withdraws the routes (ADR-0056 §3).

**Owner:** `crates/sip-proxy-runner/src/config.rs` and `config/reader.rs` (the env grammar and its
ranges), `discovery.rs` (the worker pool), `readiness.rs`, `main.rs` (the SIGTERM handler),
`crates/topology/src/k8s.rs` and `k8s/watch_health.rs` (the kube client retry, the watch's own
health), `crates/sip-proxy/src/resolver.rs` (the bounded lookup, the address family),
`templates/proxy-hmac-secret.yaml` (the cookie key Secret).

The [appendix](appendix/0046-the-front-proxy-fails-loud.md) holds the defects behind the context,
every setting with its range and default, and the tests that pin the rules.

## Context

The front proxy must refuse where it would degrade silently, as the B2BUA worker does
(ADR-0045): no stand-in worker, no default for a malformed setting, no published key, no
unbounded name lookup.
Detail: [appendix, Context](appendix/0046-the-front-proxy-fails-loud.md#context).

## Decision

### 1. Discovery never fakes a worker

Without `PROXY_WORKERS` the pool is discovered from the EndpointSlices of the worker Service
(`K8sMembership::spawn_connecting`). It starts empty. A kube client that fails to build is built
again in the background, 1 s doubling to 30 s, for the life of the process. There is no stand-in
pool.

### 2. Readiness is "at least one worker `Alive`"

Readiness has no hard timer. The proxy is NotReady until its first worker view (ADR-0050 §11) and
while no worker it routes to is `Alive`, whatever its acceptance. It routes to the workers whose
endpoint reads ready. While none does, it routes to every worker present and not terminating, on
the probe's answers (ADR-0050 §11), so a fleet read not ready while it answers keeps the proxy
ready and a fleet booting silent does not. A broken watch never changes readiness: the last
listed pool stays, and the OPTIONS probe keeps its health current.

### 3. Discovery reports its own health

`sip_proxy_discovery_no_client` is 1 once a kube client build has failed and while none is built
(0 before the first attempt ends). `sip_proxy_discovery_watch_broken_since_seconds` is the Unix
time the watch broke, 0 while it delivers. A watch that restarts on a valid resource version
delivers no event until the next change, so a broken watch ends on a delivered event or after
90 s without an error (the watcher's 30 s backoff ceiling plus the 30 s connect timeout, with
margin). Each failure is an episode: one warning naming the cause when it starts, one line when
it ends.

### 4. The virtual address follows proxy health

Superseded by ADR-0056: a node announces the VIPs while its proxy's `/readyz` passes. `/healthz`
fails (`503 stalled`) while a recv shard has held one packet longer than `PROXY_SHARD_STALL_MS`
(2 s), as `/readyz` does: such a proxy black-holes the flows hashed to that shard while it still
answers every other probe, and the livenessProbe restarts the container. A shard waiting on an
empty socket is never stalled. A shard slowed but turning (CPU starved) restarts its clock on
every packet and trips only when one packet's handling alone exceeds the threshold.

### 5. Strict configuration

Every variable is read once, before the first bind. Unset takes the default. Set empty, malformed
or out of its range refuses the boot, naming every problem at once. Only `PROXY_UDP_SNDBUF` and
`PROXY_UDP_RCVBUF` give empty a meaning (the kernel default). The cross-component checks between
the probe timings and the `LB_*` set stay. `PROXY_SELF_GATE` takes only
`1|true|yes|on|0|false|no|off`.
Ranges and defaults: [appendix, Settings](appendix/0046-the-front-proxy-fails-loud.md#settings-and-their-ranges).

### 6. No fixed fallbacks for identity

The worker namespace is `PROXY_NAMESPACE`, else `POD_NAMESPACE`, else the pod's own (its service
account namespace file). None known refuses a discovery boot. An unspecified listen address
requires its advertise (`PROXY_ADVERTISE`, `PROXY_ADVERTISE_EXT`). Bind addresses
(`PROXY_LISTEN`, `PROXY_LISTEN_EXT`, `PROXY_METRICS`) are IP literals, so the boot looks no name
up. A SIGTERM handler that cannot be installed is fatal, since the proxy would be killed undrained.

### 7. The cookie signing key is required

`PROXY_HMAC_KEY` has no default. The lab deploy creates a Secret once per cluster when it is
missing (32 random bytes, base64 text, kid `k1`). Both proxies read it through `secretKeyRef`,
and every redeploy reuses it. Rotation through env is not supported: changing the key fails the
cookie check (403) for the in-dialog requests of every call in progress. The To-tags of the
finals a proxy answers itself are keyed by a key derived from it, so either proxy, and a
restarted one, absorbs the ACK to a final the other sent.

### 8. Named-target lookups are bounded

Each lookup is bounded at `PROXY_RESOLVER_LOOKUP_TIMEOUT_MS` (2 s). A packet whose name is being
looked up waits on that lookup (at most 32 per name) and leaves after the packets ahead of it, to
the address it finds. At the bound every packet waiting on the lookup is dropped and the negative
entry (5 s) written: a packet leaves within the bound of its lookup or never. A lookup past its
bound starts no further attempt (no second name form). Its in-flight slot stays taken until the
running attempt returns, so stuck lookup threads stay at most `max_in_flight` (64) per recv
shard, and a name is looked up again within about one resolver timeout after DNS recovers. Of a
name's addresses the first the egress socket of the packet's face can reach is used and kept for
the cache lifetime. An unspecified IPv6 bind is dual-stack and reaches both families.

### 9. NAPTR/SRV is a deliberate gap

Names resolve through A/AAAA only. RFC 3263 §4.1-4.2 server selection is not implemented. A
portless URI goes to port 5060.

## Consequences

- A proxy that cannot discover workers is NotReady and says why on `/metrics`. It never takes
  calls into a pool that does not exist.
- A deploy that forgets the cookie key, the namespace or an advertise address crash-loops with the
  variable named, instead of running on a guess.
- A name that resolves slower than the bound never resolves. The bound is the contract, and the
  operator raises it or fixes DNS.

Tests that pin each rule: [appendix](appendix/0046-the-front-proxy-fails-loud.md#tests-that-pin-it).
