# Errata for RFC 6442

Applies to [rfc6442.txt](rfc6442.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc6442>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 2 |

---

## Erratum 4236 — Technical, Verified

Section 5.1, 5.2; reported by Richard Appleton on 2015-01-19.

**Original text:**

```
              <gbp:retransmission-allowed>false
              </gbp:retransmission-allowed>
```

**Corrected text:**

```
              <gbp:retransmission-allowed>no
              </gbp:retransmission-allowed>
```

**Notes:**

```
as per section 4.4

This location error is specific to having the PIDF-LO [RFC4119]
   <retransmission-allowed> element set to "no".  This location error is
   stating it requires permission (i.e., PIDF-LO <retransmission-
   allowed> element set to "yes")

and RFC4119 section 2.2.2
```

---

## Erratum 5027 — Technical, Verified

Section 5.1; reported by Larry Reeder on 2017-05-31.

**Original text:**

```
 --boundary1

   Content-Type: application/pidf+xml
   Content-ID: <target123@atlanta.example.com>
   <?xml version="1.0" encoding="UTF-8"?>
       <presence
```

**Corrected text:**

```
 --boundary1

   Content-Type: application/pidf+xml
   Content-ID: <target123@atlanta.example.com>

   <?xml version="1.0" encoding="UTF-8"?>
       <presence
```

**Notes:**

```
The PIDF-LO examples in RFC 6442 don't have an empty line between the message headers and the message body in the pidf+xml bodies.

RFC 2046, section 5.1 says this about multipart MIME body parts:  " After its boundary delimiter line, each body part then consists of a header area, a blank line, and a body area".

This errata also applies to the example in section 5.2
```

