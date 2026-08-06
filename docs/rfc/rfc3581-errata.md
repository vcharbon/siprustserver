# Errata for RFC 3581

Applies to [rfc3581.txt](rfc3581.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3581>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Rejected | 1 |

---

## Erratum 8351 — Editorial, Rejected

Section 3; reported by Roman on 2025-03-27.

**Original text:**

```
 The client behavior specified here affects the transport processing
   defined in Section 18.1 of SIP (RFC 3261) [1].

   A client, compliant to this specification (clients include UACs and
   proxies), MAY include an "rport" parameter in the top Via header
   field value of requests it generates.  This parameter MUST have no
   value; it serves as a flag to indicate to the server that this
   extension is supported and requested for the transaction.

   When the client sends the request, if the request is sent using UDP,
   the client MUST be prepared to receive the response on the same IP
   address and port it used to populate the source IP address and source
   port of the request.  For backwards compatibility, the client MUST
   still be prepared to receive a response on the port indicated in the
   sent-by field of the topmost Via header field value, as specified in
   Section 18.1.1 of SIP [1].
```

**Corrected text:**

```
 The client behavior specified here affects the transport processing
   defined in Section 18.1 of SIP (RFC 3261) [1].

   A client, compliant to this specification (clients include UACs and
   proxies), MAY include an "rport" parameter in the top Via header
   field value of requests it generates.  This parameter MUST have no
   value; it serves as a flag to indicate to the server that this
   extension is supported and requested for the transaction.

   When the client sends the request, if the request is sent using UDP,
   the client MUST be prepared to receive the response on the same IP
   address and port it used to populate the source IP address and source
   port of the request.  For backwards compatibility, the client MUST
   still be prepared to receive a response on the port indicated in the
   sent-by field of the topmost Via header field value, as specified in
   Section 18.1.1 of SIP [1].
```

**Notes:**

```
would like to report an error in RFC 3581, "An Extension to the Session Initiation Protocol (SIP) for Symmetric Response Routing".

In Section 3, "Client Behavior", the following sentence contains an incorrect references:
"processing defined in Section 18.1 of SIP"
"As specified in Section 18.1.1 of SIP [1]."

The reference points to RFC 3581, but RFC 3581 does not contain a Section 18.1.1. The correct reference should point to Section 18.1.1 of RFC 3261, which is the core SIP specification.

Correct reference:
https://datatracker.ietf.org/doc/html/rfc3261#section-18.1.1

Please consider updating the RFC to correct this error.
 --VERIFIER NOTES-- 
This is regarding the link generated in the rfc2html output, not the RFC itself (https://www.rfc-editor.org/rfc/rfc3581.txt). Please add an issue here: https://github.com/ietf-tools/rfc2html.
```

