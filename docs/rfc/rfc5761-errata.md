# Errata for RFC 5761

Applies to [rfc5761.txt](rfc5761.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc5761>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |

---

## Erratum 3380 — Technical, Verified

Section 4; reported by Martin Storsjö on 2012-10-16.

**Original text:**

```
   o  RTP payload type 80 conflicts with Receiver Summary Information
      (RSI) packets defined in "RTCP Extensions for Single-Source
      Multicast Sessions with Unicast Feedback" [6].
```

**Corrected text:**

```
   o  RTP payload type 81 conflicts with Receiver Summary Information
      (RSI) packets defined in "RTCP Extensions for Single-Source
      Multicast Sessions with Unicast Feedback" [6].
```

**Notes:**

```
Reference [6], RFC 5760 (IANA likewise), specifies that RTCP RSI has the RTCP packet type number 209, which means that it would conflict with RTP payload type 81, not 80 as stated.
```

