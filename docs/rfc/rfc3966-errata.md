# Errata for RFC 3966

Applies to [rfc3966.txt](rfc3966.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3966>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 2 |
| Held for Document Update | 2 |

---

## Erratum 202 — Editorial, Verified

Section 5.1.5; reported by Alfred Hoenes on 2004-12-04.

**Original text:**

```
        +1-212-555-1 would not be a valid global context, ...
```

**Corrected text:**

```
        +1-212-555-01 would not be a valid global context, ...
```

**Notes:**

```
Although tiny typo, it could possibly be distorting the meaning.
```

---

## Erratum 203 — Editorial, Verified

Section 3; reported by Henning Schulzrinne on 2005-03-01.

**Original text:**

```
isdn-subaddress      = ";isub=" 1*uric
```

**Corrected text:**

```
isdn-subaddress      = ";isub=" 1*paramchar
```

---

## Erratum 702 — Editorial, Held for Document Update

Section 12; reported by Alfred Hoenes on 2004-12-04.

**Original text:**

```
Fate of "fax" and "modem" URI schemes

RFC 3966 re-defines the "tel" URI scheme and obsoletes RFC 2806
which defined (and registered) three URI schemes: "tel", "fax",
and "modem". Section 12 of RFC 3966 (on page 15) merely states:

"references to ... fax and modem URIs ... have been removed."


There are *no* IANA considerations included in RFC 3966 regarding
the latter URIs.

Hence it is not clear whether these URIs are to be regarded as
informally "deprecated" or "de-registered" by this RFC, and therefore
should be marked accordingly in the IANA 'URI Schemes' reqistry.

If however, by existing policy, URI schemes cannot be "deprecated"
or "de-registered", the RFC 3966 meta-information should be changed
to say "Updates: 2806" instead of "Obsoletes: 2806", and another
errata note should be filed to change the RFC 3966 heading
accordingly, to avoid the situation of having no more 'valid'
documentation for two registered URI schemes.
```

**Corrected text:**

```
[see above] 
```

**Notes:**

```
The fate of the "fax" and "modem" URI schemes should be made clear,
formally, and in an appropriate way.

Henning Schulzrinne:
Requires discussion in the IPTEL working group, where I suggest you 
take this discussion. I have my personal opinions as to the deployment 
and deployability of the 'fax' URI scheme, but that's not particularly 
relevant. I suspect a separate document that performs the appropriate 
designation (e.g., historical) would be called for, rather than changing 
3996.


from pending
```

---

## Erratum 4376 — Editorial, Held for Document Update

Section 3; reported by OKUMURA Shinji on 2015-05-26.

**Original text:**

```
phonedigit           = DIGIT / [ visual-separator ]
phonedigit-hex       = HEXDIG / "*" / "#" / [ visual-separator ]
```

**Corrected text:**

```
phonedigit           = DIGIT / visual-separator;
phonedigit-hex       = HEXDIG / "*" / "#" / visual-separator;
```

**Notes:**

```
An optional and alternative rule is typically meaningless.
```

