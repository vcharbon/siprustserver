# Errata for RFC 3891

Applies to [rfc3891.txt](rfc3891.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3891>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |
| Held for Document Update | 1 |

---

## Erratum 7141 — Editorial, Verified

section-7.1; reported by Aaron Brumpton on 2022-09-28.

**Original text:**

```
SIP/2.0 180 Ringing
   To: <sip:bob@example.org>;tag=6472
   From: <sip:alice@example.org>;tag=7743
   Call-ID: 425928@phone.example.org
   CSeq: 1 INVITE
   Contact: <sip:bob@bobster.example.org>

   Message *3: Bob in lab -> Alice

   INVITE sip:alice@phone.example.org
   To: <sip:alice@example.org>
   From: <sip:bob@example.org>;tag=8983
   Call-ID: 09870@labpc.example.org
   CSeq: 1 INVITE
   Contact: <sip:bob@labpc.example.org>
   Replaces: 425928@phone.example.org
    ;to-tag=7743;from-tag=6472;early-only
```

**Corrected text:**

```
SIP/2.0 180 Ringing
   To: <sip:bob@example.org>;tag=6472
   From: <sip:alice@example.org>;tag=7743
   Call-ID: 425928@phone.example.org
   CSeq: 1 INVITE
   Contact: <sip:bob@bobster.example.org>

   Message *3: Bob in lab -> Alice

   INVITE sip:alice@phone.example.org
   To: <sip:alice@example.org>
   From: <sip:bob@example.org>;tag=8983
   Call-ID: 09870@labpc.example.org
   CSeq: 1 INVITE
   Contact: <sip:bob@labpc.example.org>
   Replaces: 425928@phone.example.org
    ;to-tag=6472;from-tag=7743;early-only
```

**Notes:**

```
The To and From tags are mismatched in the Replaces header
```

---

## Erratum 213 — Editorial, Held for Document Update

Section 1; reported by Rb Srikanth on 2005-07-26.

**Original text:**

```
  INVITE sip:bob@bobster.example.org
   To: <sip:bob@example.org>
   From: <sip:alice@phone2.example.org>;tag=8983
   Call-ID: 09870@phone2.example.org
   CSeq: 1 INVITE
   Contact: <sip:alice@phone2.example.org>
   Require: replaces
   Replaces: 425928@bobster.example.org;to-tag=7743;from-tag=6472
<mailto:425928@bobster.example.org;to-tag=7743;from-tag=6472>  
```

**Corrected text:**

```
  INVITE sip:bob@bobster.example.org
   To: <sip:bob@example.org>
   From: <sip:alice@phone2.example.org>;tag=8983
   Call-ID: 09870@phone2.example.org
   CSeq: 1 INVITE
   Contact: <sip:alice@phone2.example.org>
   Require: replaces
   Replaces: 425928@bobster.example.org;to-tag=8983;from-tag=7743
<mailto:425928@bobster.example.org;to-tag=8983;from-tag=7743>   
```

