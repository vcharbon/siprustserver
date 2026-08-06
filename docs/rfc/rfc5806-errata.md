# Errata for RFC 5806

Applies to [rfc5806.txt](rfc5806.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc5806>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 7 |

---

## Erratum 3081 — Technical, Verified

Section 9.1; reported by Marianne Mohali on 2012-01-05.

**Original text:**

```
ISUP and ISDN define the following diversion reasons:
```

**Corrected text:**

```
ISDN defines the following diversion reasons:
```

**Notes:**

```
The listed reasons (code and text) are not ISUP but ISDN (DSS.1)
```

---

## Erratum 3082 — Technical, Verified

Section 9.1; reported by Marianne Mohali on 2012-01-05.

**Original text:**

```
Mapping of ISUP/ISDN reason codes to Diversion reason codes is
performed as follows:
ISUP/ISDN reason code Diversion reason code
0001                  "user-busy"
0010                  "no-answer"
1111                  "unconditional"
1010                  "deflection"
1001                  "unavailable"
0000                  all others
```

**Corrected text:**

```
Mapping between ISDN reason codes and Diversion reason codes is
performed as follows:
ISDN reason code      Diversion reason code
0001                  "user-busy"
0010                  "no-answer"
1111                  "unconditional"
1010                  "deflection"
1001                  "unavailable"
0000                  all others
all others            "unknown"
```

**Notes:**

```
The reason codes are not ISUP but ISDN (ISUP deleted).
Missing the "all others" line in the ISDN to Diversion header mapping.
```

---

## Erratum 3083 — Technical, Verified

Section 9.1; reported by Marianne Mohali on 2012-01-05.

**Original text:**

```
9.1 Mapping ISUP/ISDN Diversion Reason Codes
```

**Corrected text:**

```
9.1 Mapping ISUP/ISDN Diversion Reason Codes

ISUP defines the following diversion reasons:
0001 = User busy
0010 = no reply
0011 = unconditional
0100 = deflection during alerting
0101 = deflection immediate response
0110 = mobile subscriber not reachable
0000 = Unknown

Mapping between ISUP reason codes and Diversion reason codes is
performed as follows:
ISUP reason code      Diversion reason code
0001                  "user-busy"
0010                  "no-answer"
0011                  "unconditional"
0100 or 0101          "deflection"
0110                  "unavailable"
0000                  all others
all others            "unknown"
```

**Notes:**

```
Section 9.1 mentions mapping with ISUP and ISDN but mapping with ISUP is missing. Indeed ISDN and ISUP reason parameter values are different.
This errata adds the ISUP mapping.
```

---

## Erratum 3177 — Technical, Verified

Section 4; reported by Brett Tate on 2012-04-04.

**Original text:**

```
Diversion = "Diversion" ":" 1# (name-addr *( ";" diversion_params ))
diversion-params = diversion-reason | diversion-counter |
                   diversion-limit | diversion-privacy |
                   diversion-screen | diversion-extension
```

**Corrected text:**

```
Diversion = "Diversion" HCOLON diversion-params *(COMMA diversion-params)
diversion-params    = name-addr *(SEMI (diversion-reason /
                      diversion-counter / diversion-limit /
                      diversion-privacy / diversion-screen /
                      diversion-extension))
```

**Notes:**

```
The original text did not comply with the format defined by RFC 4485 and RFC 3261.  It also did not indicate where to find the #rule (such as within RFC 2543).  Thus the ABNF for Diversion should either be modified or RFC 2543 should be referenced to help interoperability.  The proposed new ABNF was provided by RFC 6044; it also changes ";" to SEMI which addresses the related LWS ambiguity concerning if RFC 3261 or RFC 2543 LWS rules should be followed.
```

---

## Erratum 3178 — Technical, Verified

Section 6.5.1; reported by Brett Tate on 2012-04-04.

**Original text:**

```
privacy="full"
```

**Corrected text:**

```
privacy=full
```

**Notes:**

```
The example incorrectly adds quotes to full.  The quotes add confusion since full was explicitly defined to not use quotes.  Similar quoting issues exist within other examples; see sections 6.5.2, 9.2.5, 9.2.6, 9.3.5, and 9.3.6.
```

---

## Erratum 6448 — Editorial, Verified

Section 4; reported by WK Sze on 2021-03-02.

**Original text:**

```
The following is an extension of tables 4 and 5 in [RFC3261] for the Diversion header:
```

**Corrected text:**

```
The following is an extension of tables 2 and 3 in [RFC3261] for the Diversion header:
```

**Notes:**

```
RFC3261 table 2 & 3 are the "Summary of header fields" which is the correct referencing point of the new Diversion header, while table 4 & 5 are for Timers.
```

---

## Erratum 6991 — Editorial, Verified

Section 9.3.6.; reported by Rémy ALEGRI on 2022-06-14.

**Original text:**

```
;privacy="off
```

**Corrected text:**

```
;privacy="off"
```

**Notes:**

```
Hello,

In example or the 9.3.6. Example of SIP to ISDN Translation, an end quote error is present.


Sincerely,
```

