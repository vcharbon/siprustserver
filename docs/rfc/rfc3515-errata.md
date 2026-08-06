# Errata for RFC 3515

Applies to [rfc3515.txt](rfc3515.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3515>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |
| Held for Document Update | 1 |

---

## Erratum 4898 — Editorial, Verified

Section 2.1; reported by Marianne MOHALI on 2017-01-03.

**Original text:**

```
Refer-To: <sip:bob@biloxi.example.net?Accept-Contact=sip:bobsdesk.
       biloxi.example.net&Call-ID%3D55432%40alicepc.atlanta.example.com>
```

**Corrected text:**

```
Refer-To: <sip:bob@biloxi.example.net?Accept-Contact=sip:bobsdesk.
       biloxi.example.net&Call-ID=55432%40alicepc.atlanta.example.com>
```

**Notes:**

```
The "=" between the header name (hname) and the value (hvalue) in the headers component of the URI does not have to be in the percent-coded format as part of the ABNF of the headers component defined in RFC3261:
sip:user:password@host:port;uri-parameters?headers
headers         =  "?" header *( "&" header )
header          =  hname "=" hvalue
hname           =  1*( hnv-unreserved / unreserved / escaped )
hvalue          =  *( hnv-unreserved / unreserved / escaped )
hnv-unreserved  =  "[" / "]" / "/" / "?" / ":" / "+" / "$"
```

---

## Erratum 4652 — Technical, Held for Document Update

Section 2.1; reported by Brett Tate on 2016-03-30.

**Original text:**

```
The Refer-To header field MAY be encrypted as part of end-to-end
encryption.
```

**Corrected text:**

```
If the URI contains a comma, question mark or semicolon, the URI
MUST be enclosed in angle brackets (< and >).

The Refer-To header field MAY be encrypted as part of end-to-end
encryption.
```

**Notes:**

```
If addr-spec is used when there are parameters, it is ambiguous if the parameters are URI parameters or header parameters.  For consistency with RFC 3261 section 20, the same bracket rule is indicated even if comma and question mark do not cause an issue.
```

