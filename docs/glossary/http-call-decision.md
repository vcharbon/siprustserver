# HTTP call-decision adaptation

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "HTTP call-decision adaptation".

The vocabulary for the external decision layer (the future HTTP adapter; today
the in-process scripted adapter) that tells the B2BUA *how to treat a call*,
the substrate for numbering-plan services. The decision request carries the
inbound call context (R-URI, From/To, all non-structural `X-*` headers, body);
the response is a **call treatment**. See [ADR-0017](../../docs/adr/0017-http-call-treatment-and-header-ownership.md).

**Call treatment**:
The single outcome the decision layer returns for a call, at *any* hop. The
initial decision and every failover hop draw from the **same** closed set:
**route** (bridge to a destination, with header/identity rewrites), **redirect**
(emit a 3xx to the caller with a Contact list), **reject** (a final failure
response the layer authors, with code, reason-phrase and extra headers), and **relay**
(pass the last attempted b-leg's actual failure response back to the caller
verbatim). One enum, reused by the new-call and failure callbacks, so "all the
info of how to treat the call" is one vocabulary.
_Avoid_: separate outcome sets for new-call vs failover (they are unified);
"terminate" (a fixed-486 path; say per-call **reject**/**relay**).

**Header ownership**:
For each SIP header on a B2BUA-authored message, exactly one of two parties is
its author: the **decision layer** (HTTP) or the **core engine** (B2BUA). The
matrix, for a normal service: the decision layer owns the **From** and **To**
*URIs* (the numbers) but **never the tags**; it owns **Contact** *only* on a
**redirect** (302) response (the redirect targets); it freely *adds*
non-structural headers (PAI, PANI, any `X-*`). The core engine always owns the
**tags**, **Via**, **CSeq**, **Call-ID**, **branch**, **Max-Forwards**, and
**Contact** on every non-302 message. From/To URI and 302-Contact are the only
fields that flip to HTTP ownership for a normal service.
_Avoid_: letting the flat header-update map rewrite a **structural** header
(From/To/Via/CSeq/Call-ID/Contact). The map only *appends*; structural rewrites go
through typed identity fields, and tags are never HTTP-settable.

**Reroute plan** (the carried failover list):
The ordered remainder of destinations (plus the **on-exhausted** behaviour) a
call still has to try, stashed by the decision layer inside the call's opaque
**callback context**, the field the platform treats as a token it round-trips
untouched (a real HTTP backend would hold this state itself). On a b-leg
failure the failure callback pops the head, returns a **route** to it, and
re-stashes the tail; on an empty list it returns the plan's **on-exhausted**
**call treatment** (a **reject**, **relay**, or **redirect**). Because the plan
rides the *existing* opaque callback-context string (and `ext`), it is
serialised and replicated with the call for free (no new wire field, no
positional-codec change, ADR-0008), so it survives failover by construction.
_Avoid_: a structured top-level "routes" field on the call (would be a new
replicated field); parsing the callback context anywhere but the decision layer
(the platform keeps it opaque).
