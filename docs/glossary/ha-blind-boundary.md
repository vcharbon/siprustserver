# The HA-blind boundary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "The HA-blind boundary".

[ADR-0055](../../docs/adr/0055-services-are-ha-blind.md) §1, §11.

**Blind crate**:
A crate that holds single-node logic only: no HA type, term or branch. Its
manifest states `[package.metadata.ha] side = "blind"`; its normal dependencies
are blind crates and its files pass the **word net** (`scripts/ha_side.py`, run by
`scripts/purity.py`). The call record, the decision core, the SDK, the decision
seam (`b2bua-decision`) and the services are blind.
_Avoid_: "HA-free" for a crate whose side no manifest states.

**Aware crate**:
A crate that may know HA (`side = "aware"`): the shell (`b2bua`), replication,
the proxy, the runners, the harnesses and every test crate.

**Word net**:
The words a blind crate never holds, in any spelling: `takeover`, `reclaim`,
`tombstone`, `materialise`/`materialize`, `stand-down`, and `replica` as a word of
its own. No allowlist: a hit moves to an aware crate or states the single-node
fact it makes.

**Store envelope**:
What the store keeps and replicates beside a call record (`b2bua::store::envelope`):
the worker pair the call was placed on, the version vector and the state stamp.
A replicated body is the envelope, then the call body. The core reads none of it;
the shell hands each turn the call's **owners** as a plain input
(`b2bua_core::owners::Owners`).

**Off-turn request**:
An HTTP request a call sends off its turn and awaits the answer of (ADR-0039):
a release or failure consult, a REFER authorization, a service's request, a
limiter admit. It is made for its answer alone: its side effects at the
service, and load balancing or rate limiting over HTTP, are no concern. A node
serving an `Active` call from its record sends it again; an ending call needs
no answer and sends none again.
