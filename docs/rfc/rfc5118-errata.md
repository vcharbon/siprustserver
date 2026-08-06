# Errata for RFC 5118

Applies to [rfc5118.txt](rfc5118.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc5118>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |

---

## Erratum 1311 — Technical, Verified

Section 4.3, 1st par; reported by Alfred Hoenes on 2008-02-11.

**Original text:**

```
                                                  [...], the intended port
   number becomes the last octet of the reference.
```

**Corrected text:**

```
                                                  [...], the intended port
   number becomes the last octet pair of the reference.
```

**Notes:**

```
Each hexadecimal group in a literal IPv6 address encodes two octets
of the IPv6 address -- cf. RFC 4291 !
```

