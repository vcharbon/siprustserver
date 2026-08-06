# Errata for RFC 4868

Applies to [rfc4868.txt](rfc4868.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc4868>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |
| Held for Document Update | 1 |

---

## Erratum 1785 — Technical, Verified

Section 2.7.2.3; reported by Sheila Frankel on 2009-05-19.

**Original text:**

```
Test Case AUTH512-4:
   Key =          0a0b0c0d0e0f10111213141516171819
                  0102030405060708090a0b0c0d0e0f10
                  1112131415161718191a1b1c1d1e1f20
                  2122232425262728292a2b2c2d2e2f30
                  3132333435363738393a3b3c3d3e3f40  (64 bytes)
```

**Corrected text:**

```
Test Case AUTH512-4:
   Key =          0102030405060708090a0b0c0d0e0f10
                  1112131415161718191a1b1c1d1e1f20
                  2122232425262728292a2b2c2d2e2f30
                  3132333435363738393a3b3c3d3e3f40  (64 bytes)
```

**Notes:**

```
Originally noted by Tero Kivinen
```

---

## Erratum 5507 — Technical, Held for Document Update

Section 2.7.2.2.; reported by f. Le Pouliquen on 2018-09-27.

**Original text:**

```
Test Case AUTH384-4:
   Key =          0102030405060708090a0b0c0d0e0f10
                  1112131415161718191a1b1c1d1e1f20
                  0a0b0c0d0e0f10111213141516171819
```

**Corrected text:**

```
Test Case AUTH384-4:
   Key =          0102030405060708090a0b0c0d0e0f10
                  1112131415161718191a1b1c1d1e1f20
                  2122232425262728292a2b2c2d2e2f30
```

**Notes:**

```
as the keys increase from case 256 to 384 and to 512, they have the same base.
```

