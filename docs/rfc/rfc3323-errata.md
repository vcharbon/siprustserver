# Errata for RFC 3323

Applies to [rfc3323.txt](rfc3323.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3323>) as of 2026-11-07.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |

---

## Erratum 5184 — Technical, Verified

Section 4.2 Expre...; reported by DONG O YI on 2017-11-16.

**Original text:**

```
Privacy-hdr  =  "Privacy" HCOLON priv-value *(";" priv-value)
```

**Corrected text:**

```
Privacy-hdr  =  "Privacy" HCOLON priv-value *("," priv-value)
```

**Notes:**

```
RFC3261 says, 

Specifically, any SIP header whose grammar is of the form

      header  =  "header-name" HCOLON header-value *(COMMA header-value)

   allows for combining header fields of the same name into a comma-
   separated list.

So, RFC3323 conflicts RFC3261.
```
