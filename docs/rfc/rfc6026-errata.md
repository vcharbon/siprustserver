# Errata for RFC 6026

Applies to [rfc6026.txt](rfc6026.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc6026>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 2 |
| Held for Document Update | 1 |

---

## Erratum 2538 — Technical, Verified

Section 7.1, pg.6; reported by Alfred Hoenes on 2010-09-30.

**Original text:**

```
[[ last paragraph on page 6: ]]

   Figures 1 and 2 show the parts of the INVITE server state machine
   that have changed.  The entire new INVITE server state machine is
|  shown in Figure 5.
```

**Corrected text:**

```
   Figures 1 and 2 show the parts of the INVITE server state machine
   that have changed.  The entire new INVITE server state machine is
|  shown in Figure 7.
```

**Notes:**

```
- qualified as Technical because of importance of correct pointer;
- apparently this detail has been missed when the Figures in the
  document have been renumbered (#5 --> #7 and #4 --> #5) to achieve
  the relationship to RFC 3261 explained in Section 8 (top of page 11):

                                         [...]  This document
   intentionally does not contain a Figure 4 or Figure 6 so that the
   labels for Figures 5 and 7 are identical to the labels of the figures
   they are replacing in RFC 3261.
```

---

## Erratum 2539 — Technical, Verified

Section 8.4, pg.12; reported by Alfred Hoenes on 2010-10-01.

**Original text:**

```
|8.4.  Pages 126 through 128

   Section 17.1.1.2.  Replace paragraph 7 (starting "When in either")
   through the end of the section with:
```

**Corrected text:**

```
|8.4.  Pages 126 through 129

   Section 17.1.1.2.  Replace paragraph 7 (starting "When in either")
   through the end of the section with:
```

**Notes:**

```
Rationale:
  In RFC 3261, Section 17.1.1.2. extends to mid-page 129.
  So if the quoted text is correct, the section headline
  here is strongly misleading, contradicts the text, and
  hence needs adjustment.
  Since the textual scope of the change is at the heart of
  this RFC, this Errata note is classified as Technical.
```

---

## Erratum 2536 — Editorial, Held for Document Update

Section 7.2; reported by John Takao Collier on 2010-09-30.

**Original text:**

```
   +-----------+                        +-----------+
   |           |                        |           |
   |  Calling  |                        |  Calling  |
   |           |----------->+           |           |-----------+
   +-----------+ 2xx        |           +-----------+ 2xx       |
                 2xx to TU  |                         2xx to TU |
                            |                                   |
                            |                                   |
```

**Corrected text:**

```
   BEFORE                               AFTER

   +-----------+                        +-----------+
   |           |                        |           |
   |  Calling  |                        |  Calling  |
   |           |----------->+           |           |-----------+
   +-----------+ 2xx        |           +-----------+ 2xx       |
                 2xx to TU  |                         2xx to TU |
                            |                                   |
                            |                                   |
```

**Notes:**

```
Figures 1 and 2 contain "BEFORE" and "AFTER" labels for their respective state machines.  Figure 3 does not contain a "BEFORE" and "AFTER" label.  Although it's pretty obvious from the context that the left-side is the "BEFORE" case and the right-side is the "AFTER" case, I believe for consistency (and to make it explicit), Figure 3 should contain "BEFORE" and "AFTER" labels.
```

