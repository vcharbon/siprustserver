# Errata for RFC 2046

Applies to [rfc2046.txt](rfc2046.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc2046>) as of 2026-11-07.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 10 |
| Held for Document Update | 2 |
| Reported | 1 |
| Rejected | 1 |

---

## Erratum 507 — Technical, Verified

Section 5.1.5; reported by Bruce Lilly on 2003-10-04.

**Original text:**

```
      Date: Mon, 22 Mar 1994 13:34:51 +0000
```

**Corrected text:**

```
      Date: Tue, 22 Mar 1994 13:34:51 +0000 
```

## Erratum 508 — Technical, Verified

Section Appendix A states: ; reported by Herman Meerlo on 2001-10-04.

**Original text:**

```
     discard-text := *(*text CRLF)
```

**Corrected text:**

```
     discard-text := *(*text CRLF) *text
```

## Erratum 588 — Technical, Verified

Section 5.2.2.2; reported by Bruce Lilly on 2001-12-22.

**Original text:**

```
Date: Fri, 26 Mar 1993 12:59:38 -0500 (EST)
Subject: Audio mail
Message-ID: <anotherid@foo.com>
```

**Corrected text:**

```
Date: Fri, 26 Mar 1993 12:59:38 -0500 (EST)
Message-ID: <anotherid@foo.com>
Subject: Audio mail
```

## Erratum 589 — Technical, Verified

Section 5.2.3.7; reported by Bruce Lilly on 2001-12-22.

**Original text:**

```
Content-Type: message/external-body;
              access-type=mail-server
              server="listserv@bogus.bitnet";
              expiration="Fri, 14 Jun 1991 19:13:14 -0400 (EDT)"
```

**Corrected text:**

```
Content-Type: message/external-body;
              access-type=mail-server;
              server="listserv@bogus.bitnet";
              expiration="Fri, 14 Jun 1991 19:13:14 -0400 (EDT)"
```

**Notes:**

```
Semicolons were missing.
```

## Erratum 6800 — Technical, Verified

Section 5.1.4; reported by Daniel Shahaf on 2021-12-27.

**Original text:**

```
                                    In the case where one of the
   alternatives is itself of type "multipart" and contains unrecognized
   sub-parts, the user agent may choose either to show that alternative,
   an earlier alternative, or both.
```

**Corrected text:**

```
                                    In the case where one of the
   alternatives is itself of type "multipart" and contains unrecognized
   sub-parts, the user agent may choose to either show that alternative,
   show an earlier alternative, or let the user choose which alternative
   to show.
```

**Notes:**

```
The quoted sentence conflicts with the following sentence later in the same section:

                  What is most critical, however, is that the user not
   automatically be shown multiple versions of the same data.  Either
   the user should be shown the last recognized version or should be
   given the choice.

I assume the correction should be as proposed, but other options are conceivable.
```

## Erratum 509 — Editorial, Verified

Section 9; reported by Steve Bellovin on 2004-02-03.

**Original text:**

```
9.  Security Considerations
```

**Corrected text:**

```
8.  Security Considerations
```

**Notes:**

```
In the Table of Contents, the Security Considerations is listed to be in Section 8.  However, in the text, both the Security Considerations and Authors' Addresses are in Section 9; there is no Section 8.
```

## Erratum 510 — Editorial, Verified

Section 5.1.5; reported by Bruce Lilly on 2001-12-22.

**Original text:**

```
undesireble 

seperate
```

**Corrected text:**

```
undesirable

separate
```

**Notes:**

```
Spelling errors.
```

## Erratum 511 — Editorial, Verified

Section 3; reported by Lars Kasper on 2005-01-06.

**Original text:**

```
    (2)   image -- image data.  "Image" requires a display device
          (such as a graphical display, a graphics printer, or a
          FAX machine) to view the information. An initial
          subtype is defined for the widely-used image format
          JPEG. .  subtypes are defined for two widely-used image
          formats, jpeg and gif.
```

**Corrected text:**

```
    (2)   image -- image data.  "Image" requires a display device
          (such as a graphical display, a graphics printer, or a
          FAX machine) to view the information. Subtypes are defined
          for two widely-used image formats, jpeg and gif.
```

**Notes:**

```
The sentence previous to the last should be deleted, as it is covered by the last sentence.
```

## Erratum 6582 — Editorial, Verified

Section 4.1.4; reported by ojab on 2021-05-15.

**Original text:**

```
Unrecognized subtypes which also specify an unrecognized charset 
should be treated as "application/octet- stream".
```

**Corrected text:**

```
Unrecognized subtypes which also specify an unrecognized charset 
should be treated as "application/octet-stream".
```

**Notes:**

```
Extra space in "application/octet- stream"
```

## Erratum 7341 — Editorial, Verified

Section 5.2.3.7; reported by Viatrix on 2023-02-07.

**Original text:**

```
provided in the same format but via different accces mechanisms.
```

**Corrected text:**

```
provided in the same format but via different access mechanisms.
```

**Notes:**

```
"accces" is a typo
```

## Erratum 6776 — Technical, Held for Document Update

Section 5.1.1; reported by Brian Antos on 2021-12-05.

**Original text:**

```
NOTE:  The CRLF preceding the boundary delimiter line is conceptually
   attached to the boundary so that it is possible to have a part that
   does not end with a CRLF (line  break).  Body parts that must be
   considered to end with line breaks, therefore, must have two CRLFs
   preceding the boundary delimiter line, the first of which is part of
   the preceding body part, and the second of which is part of the
   encapsulation boundary.
```

**Corrected text:**

```
NOTE:  The CRLF preceding the boundary delimiter line is conceptually
   attached to the boundary so that it is possible to have a part that
   does not end with a CRLF (line  break).  Body parts that must be
   considered to end with line breaks, therefore, must have two CRLFs
   preceding the boundary delimiter line, the first of which is part of
   the preceding body part, and the second of which is part of the
   encapsulation boundary.

The requirement that the encapsulation boundary begins  with
a  CRLF  implies  that  the  body of a multipart entity must
itself begin with a CRLF before the first encapsulation line
--  that  is, if the "preamble" area is not used, the entity
headers must be followed by TWO CRLFs.  This is  indeed  how
such  entities  should be composed.  A tolerant mail reading
program, however, may interpret a  body  of  type  multipart
that  begins  with  an encapsulation line NOT initiated by a
CRLF  as  also  being  an  encapsulation  boundary,  but   a
compliant  mail  sending  program  must  not  generate  such
entities.
```

**Notes:**

```
Recommend re-introducing the wording from the original RFC (1341) regarding preceding CRLF and the first delimiter line. Current RFC is ambiguous about handling the initial line without it.
```

## Erratum 4609 — Editorial, Held for Document Update

Section 3; reported by Oleg Andriyanov on 2016-02-01.

**Original text:**

```
          particular, formats that employ embeddded binary
```

**Corrected text:**

```
          particular, formats that employ embedded binary
```

**Notes:**

```
"embeddded" has triple "d".
```

## Erratum 8667 — Technical, Reported

Section 4.1.2; reported by Jason Yundt on 2025-12-02.

**Original text:**

```
   The complete US-ASCII character set is listed in ANSI X3.4- 1986.
```

**Corrected text:**

```
   In US-ASCII octet sequences, the highest order bit (most significant
   bit) in each octet is always zero.  The correct interpretation of the
   remaining 7 bits in each octet is listed in ANSI X3.4-1986.
```

**Notes:**

```
Section 2.2 of RFC 2045 defines the term “character set”:

>    The term "character set" is used in MIME to refer to a method of
>    converting a sequence of octets into a sequence of characters.

RFC 2046 says that US-ASCII is a character set and points to ANSI X3.4-1986 for the definition of that character set. Unfortunately, ANSI X3.4-1986 does not give a complete definition of a character set because ANSI X3.4-1986 is not about sequences of octets. Section 4 of ANSI X3.4-1986 says:

> The bits of the bit combinations of the 7-bit code are
> identified by b₇, b₆, b₅, b₄, b₃, b₂, and b₁, where
> b₇ is the highest order bit (most significant bit), and
> b₁ is the lowest order bit (least significant bit).

ANSI X3.4-1986 never mentions a b₈ bit, or what to do if you want to decode a stream of octets. It only alludes to octets in passing. Section 2.1.1 of ANSI X3.4-1986 says:

>   2.1.1 Conforming Interchange. The data stream
> comprising the exchange of information using the
> coding of this standard is subject to the following rules.
> Conforming interchange:
>   (1) Shall be a sequence of 7-bit or 8-bit bit combin-
> ations.
>   (2) Shall use the names of all control characters as
> specified in this standard.
>   (3) Shall not include any bit combinations allo-
> cated by this standard for any purpose other than that
> defined in this standard, unless they represent func-
> tions or elements of other coded character sets that
> have been invoked by code extension in accordance
> with ANSI X3.41-1974, subject to mutual agreement
> between the interchange parties.

(An 8-bit bit combination is an octet that contains character data. See section 3.2 of ANSI X3.4-1986.)

Unfortunately, the rest of ANSI X3.4-1986 doesn’t tell us how to handle sequences of 8-bit bit combinations. It only tells us how to handle 7-bit bit combinations. Section 9.2 of ANSI X3.41-1974 specifies that the highest order bit should be zero when storing ASCII in 8-bit bit combinations, but you don’t have to implement anything from ANSI X3.41-1974 in order to properly implement ANSI X3.4-1986. In fact, ANSI X3.41-1974 is mostly about shift and escape sequences so most of it must not be implemented for US-ASCII. As section 4.1.2 of RFC 2046 says:

>    All of these character sets are used as pure 7bit or 8bit sets
>    without any shift or escape functions.

Additionally, there shouldn’t have been a space after the hyphen in “ANSI X3.4- 1986”.

See also: RFC 20.
```

## Erratum 6583 — Editorial, Rejected

Section 5.1.2; reported by ojab on 2021-05-15.

**Original text:**

```
It is essential that such entities be handled correctly when they are
themselves imbedded inside of another "multipart" structure.
```

**Corrected text:**

```
It is essential that such entities be handled correctly when they are 
themselves embedded inside of another "multipart" structure.
```

**Notes:**

```
`imbedded` is a typo
 --VERIFIER NOTES-- 
“Imbed” is a less common spelling of “embed” (see https://www.merriam-webster.com/dictionary/imbed). It is used in a number of early RFCs (e.g., RFCs 549, 1035, 1258, 1580, 1689, 2567, 3423, and more). It was last used in RFC 5045. Since then, “embed” has been used in the RFC Series.  
```
