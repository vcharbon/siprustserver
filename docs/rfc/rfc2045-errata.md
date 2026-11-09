# Errata for RFC 2045

Applies to [rfc2045.txt](rfc2045.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc2045>) as of 2026-11-07.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 3 |
| Held for Document Update | 1 |
| Rejected | 1 |

---

## Erratum 2586 — Technical, Verified

Section 1.; reported by Christopher Yeleighton on 2010-10-28.

**Original text:**

```
   All of the header fields defined in this document are subject to the
   general syntactic rules for header fields specified in RFC 822.  In
   particular, all of these header fields except for Content-Disposition
   can include RFC 822 comments, which have no semantic content and
   should be ignored during MIME processing.
```

**Corrected text:**

```
   All of the header fields defined in this document are subject to the
   general syntactic rules for header fields specified in RFC 822.  In
   particular, all of these header fields
   can include RFC 822 comments, which have no semantic content and
   should be ignored during MIME processing.
```

**Notes:**

```
A header field Content-Disposition is not defined in this document.  Therefore, the header fields defined in this document do not contain a field Content-Disposition and the exception is not necessary.  It is also misleading because it looks as if the exception affected the Content-Disposition field defined in RFC2813 while it does not.
```

## Erratum 512 — Editorial, Verified

Section 5.1; reported by Ned Freed on 2005-02-24.

**Original text:**

```
     tspecials :=  "(" / ")" / "<" / ">" / "@" /
                   "," / ";" / ":" / "\" / <">
                   "/" / "[" / "]" / "?" / "="
```

**Corrected text:**

```
     tspecials :=  "(" / ")" / "<" / ">" / "@" /
                   "," / ";" / ":" / "\" / <"> /
                   "/" / "[" / "]" / "?" / "="
```

**Notes:**

```
Missing alternative separator on the second line.
```

## Erratum 7120 — Editorial, Verified

Section 2.4; reported by Maciej Szumski on 2022-09-07.

**Original text:**

```
Note that this does NOT imply thay they have no meaning at all
```

**Corrected text:**

```
Note that this does NOT imply that they have no meaning at all
```

**Notes:**

```
Fixing a typo: 'thay' instead of 'that'.
```

## Erratum 3442 — Editorial, Held for Document Update

Section 8.; reported by Kwadronaut on 2013-01-03.

**Original text:**

```
   body is often desirable.  For example, it may be useful to mark an
   "image" body as "a picture of the Space Shuttle Endeavor."
```

**Corrected text:**

```
   body is often desirable.  For example, it may be useful to mark an
   "image" body as "a picture of the Space Shuttle Endeavour."
```

**Notes:**

```
That orbiter was called the Endeavour, little typo.

---
Verifier notes:
Little typo, indeed, and perhaps the draft's authors weren't aware of the NASA spelling.  Still, little insignificant typos aren't what RFC errata are all about, so this goes in the "hold for document update" bin.
```

## Erratum 3950 — Technical, Rejected

Section 4; reported by Peter Occil on 2014-04-03.

**Original text:**

```
   NOTE TO IMPLEMENTORS:  When checking MIME-Version values any RFC 822
   comment strings that are present must be ignored.  In particular, the
   following four MIME-Version fields are equivalent:

     MIME-Version: 1.0

     MIME-Version: 1.0 (produced by MetaSend Vx.x)

     MIME-Version: (produced by MetaSend Vx.x) 1.0

     MIME-Version: 1.(produced by MetaSend Vx.x)0
```

**Corrected text:**

```
   NOTE TO IMPLEMENTORS:  When checking MIME-Version values any RFC 822
   comment strings that are present must be ignored.  In particular, the
   following three MIME-Version fields are equivalent:

     MIME-Version: 1.0

     MIME-Version: 1.0 (produced by MetaSend Vx.x)

     MIME-Version: (produced by MetaSend Vx.x) 1.0
```

**Notes:**

```
Under RFC 822, a comment placed between two lexical symbols, in the case of the fourth example given, between the dot and the zero, is equivalent to a single space.  Accordingly, the fourth example would result in the field "MIME-Version: 1. 0", which is not exactly equivalent to the previous three examples.
 --VERIFIER NOTES-- 
It is exactly equivalent, because, while the lexical parsing puts a blank in, the parsing of the MIME-Version value then ignores the blank.
```
