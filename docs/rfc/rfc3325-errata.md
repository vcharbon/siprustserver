# Errata for RFC 3325

Applies to [rfc3325.txt](rfc3325.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3325>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 4 |
| Rejected | 1 |

---

## Erratum 3744 — Technical, Verified

Section 9.1; reported by Brett Tate on 2013-10-08.

**Original text:**

```
A P-Asserted-Identity header field value MUST consist of exactly one
name-addr or addr-spec.
```

**Corrected text:**

```
A P-Asserted-Identity header field value MUST consist of exactly one
name-addr or addr-spec.  If the URI contains a comma, the URI MUST
be enclosed in angle brackets (< and >).
```

**Notes:**

```
While the P-Asserted-Identity and P-Preferred-Identity header fields have an ambiguity only for "," (not for ";" and "?"), we note that usage of ";" and "?" also must be enclosed in angle brackets to preserve consistency with the RFC 3261 section 20 bracket rule.
```

---

## Erratum 3894 — Technical, Verified

Section 9.2; reported by Richard Barnes on 2014-02-15.

**Original text:**

```
A P-Preferred-Identity header field value MUST consist of exactly one
name-addr or addr-spec.
```

**Corrected text:**

```
A P-Preferred-Identity header field value MUST consist of exactly one
name-addr or addr-spec.  If the URI contains a comma, the URI MUST
be enclosed in angle brackets (< and >).
```

**Notes:**

```
While the P-Asserted-Identity and P-Preferred-Identity header fields have an ambiguity only for "," (not for ";" and "?"), we note that usage of ";" and "?" also must be enclosed in angle brackets to preserve consistency with the RFC 3261 section 20 bracket rule.
```

---

## Erratum 4202 — Editorial, Verified

Section 10.2; reported by Giovanni Signoriello on 2014-12-17.

**Original text:**

```
P-Asserted-Identity: "Cullen Jennings" <sip:fluffy@vovida.org>
```

**Corrected text:**

```
P-Asserted-Identity: "Cullen Jennings" <sip:fluffy@cisco.com>
```

**Notes:**

```
May be an editorial error in the message F4, section 10.2.
In that message is added the P-Asserted-Identity with the SIP URI sip:fluffy@vovida.org
I suppose it should be cisco.com and not vovida.com.
Thank you.
```

---

## Erratum 5499 — Editorial, Verified

Section 10; reported by Richard Phernambucq on 2018-09-20.

**Original text:**

```
   * F4   proxy.cisco.com -> proxy.pstn.net (trusted)

   INVITE sip:+14085551212@proxy.pstn.net SIP/2.0
   Via: SIP/2.0/TCP useragent.cisco.com;branch=z9hG4bK-124
   Via: SIP/2.0/TCP proxy.cisco.com;branch=z9hG4bK-abc
```

**Corrected text:**

```
   * F4   proxy.cisco.com -> proxy.pstn.net (trusted)

   INVITE sip:+14085551212@proxy.pstn.net SIP/2.0
   Via: SIP/2.0/TCP proxy.cisco.com;branch=z9hG4bK-abc
   Via: SIP/2.0/TCP useragent.cisco.com;branch=z9hG4bK-124
```

**Notes:**

```
As per RFC 3261, chapter 16.6, step 8:
  The proxy MUST insert a Via header field value into the copy before the existing Via header field values.

The order of Via headers should be reversed. This applies to the following message examples:
chapter 10.1: F4, F5
chapter 10.2: F4, F5, F6

Text and examples in RFC3261 Section 20.42 supports the argument that the order is reversed.
```

---

## Erratum 7794 — Technical, Rejected

Section 10.2; reported by Christos Diamantis on 2024-02-02.

**Original text:**

```
The next proxy removes the P-Asserted-Identity 
header field and the request for Privacy before forwarding this 
request onward to the biloxi.com proxy server which it does not trust.
```

**Corrected text:**

```
The next proxy removes the P-Asserted-Identity 
header field but does not remove the request for Privacy before forwarding this 
request onward to the biloxi.com proxy server which it does not trust.
```

**Notes:**

```
As stated in ETSI TS 124 607 V17.0.0 (2022-04), 
section 4.3.3 Requirements on the terminating network side:
NOTE 1: The priv-value "id" in the Privacy header is not expected be removed when removing any P-Asserted-Identity header as described in 3GPP TS 24.229 subclauses 4.4.2 (The priv-value "id" shall not be removed from the Privacy header field when SIP signalling crosses the boundary of the trust domain) and 5.4.3.3.
and also,
section 4.5.2.9 Actions at the AS serving the terminating UE:
The priv-value "id" in the Privacy header will be used by the terminating UE to distinguish the request of OIR by the originating user.
 --VERIFIER NOTES-- 
This section was revised in https://www.rfc-editor.org/rfc/rfc5876.txt Section 3.2
If the errata remains relevant to the revision, a new errata should be filed per https://datatracker.ietf.org/doc/statement-iesg-iesg-processing-of-rfc-errata-for-the-ietf-stream-20210507/
```

