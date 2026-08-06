# Errata for RFC 3986

Applies to [rfc3986.txt](rfc3986.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3986>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 2 |
| Held for Document Update | 7 |
| Rejected | 8 |

---

## Erratum 2525 — Technical, Verified

Section 3.1; reported by Christopher Yeleighton on 2010-09-17.

**Original text:**

```
Advice for designers of new URI schemes can be found in [RFC2718].
```

**Corrected text:**

```
Advice for designers of new URI schemes can be found in [RFC4395].
```

**Notes:**

```
The document [RFC2718] is for designers of designers of new URL schemes only.  It has been obsoleted by [RFC4395] that covers all URI schemes.

[RFC4395]  
T. Hansen, T. Hardie, T. and L. Masinter,
"Guidelines and Registration Procedures for New URI Schemes", 
RFC 4395, February 2006.
```

---

## Erratum 7590 — Editorial, Verified

Section 5.2.4; reported by Ben Kallus on 2023-08-06.

**Original text:**

```
N/A
```

**Corrected text:**

```
N/A 
```

**Notes:**

```
The "1" in step 1 of the second example for "remove_dot_segments" has an unintentional hyperlink to section 1 of the document. The hyperlink should be removed.
```

---

## Erratum 2624 — Technical, Held for Document Update

Section Appendix B; reported by Peter Saint-Andre on 2010-11-11.

**Original text:**

```
^(([^:/?#]+):)?(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?
```

**Corrected text:**

```
/^(([^:\/?#]+):)?(\/\/([^\/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?/
```

**Notes:**

```
This is a copy of Erratum #1933, reported against RFC 2396.
```

---

## Erratum 4547 — Technical, Held for Document Update

Section 5.4.2; reported by siva elango ramaswamy on 2015-12-01.

**Original text:**

```
5.4.2.  Abnormal Examples

   Although the following abnormal examples are unlikely to occur in
   normal practice, all URI parsers should be capable of resolving them
   consistently.  Each example uses the same base as that above.

   Parsers must be careful in handling cases where there are more ".."
   segments in a relative-path reference than there are hierarchical
   levels in the base URI's path.  Note that the ".." syntax cannot be
   used to change the authority component of a URI.

      "../../../g"    =  "http://a/g"
      "../../../../g" =  "http://a/g"
```

**Corrected text:**

```
5.4.2.  Abnormal Examples

   Although the following abnormal examples are unlikely to occur in
   normal practice, all URI parsers should be capable of resolving them
   consistently.  Each example uses the same base as that above.

   Parsers must be careful in handling cases where there are more ".."
   segments in a relative-path reference than there are hierarchical
   levels in the base URI's path.  Note that the ".." syntax cannot be
   used to change the authority component of a URI.

      "../../../g"    =  "http://a/../g"
      "../../../../g" =  "http://a/../../g"
```

**Notes:**

```
The example which was taken from RFC 1808 had proper resolved URL ( 5.2 ).

As the base URL has two levels ( http://a/b/c/ ) and if the relative url's have two ".." segments then from the resolved URI both the hierarchical levels can be removed to form the resolved URL, as below:

../../g    = http://a/g 

and if there are more ".." segments than hierarchical level in base URI's path... then the number of ".." segments that doesn't have corresponding segments in base URI should be left as is in the resolved URI,  like below

"../../../g" = "http://a/../g"
```

---

## Erratum 4789 — Technical, Held for Document Update

Section 5.2.3; reported by Dinar Qurbanov on 2016-08-31.

**Original text:**

```
   o  If the base URI has a defined authority component and an empty
      path, then return a string consisting of "/" concatenated with the
      reference's path; otherwise,
```

**Corrected text:**

```
   o  If the base URI has a defined authority component and an empty 
      path, or if the base URI's path is ending with "/..", then return 
      a string consisting of base's path concatenated with "/" and then 
      concatenated with the reference's path; otherwise,
```

**Notes:**

```
this is about case when reference does not have scheme and authority and its path is not starting with "/".
```

---

## Erratum 4942 — Technical, Held for Document Update

Section 3.2.2. Host; reported by r. de raat on 2017-02-19.

**Original text:**

```
Such a name consists of a sequence of domain labels separated by ".",
each domain label starting and ending with an alphanumeric character
and possibly also containing "-" characters.  The rightmost domain
label of a fully qualified domain name in DNS may be followed by a
single "." and should be if it is necessary to distinguish between
the complete domain name and some local domain.

   reg-name    = *( unreserved / pct-encoded / sub-delims )

If the URI scheme defines a default for host, then that default
```

**Corrected text:**

```
Such a name consists of a sequence of domain labels separated by ".",
each domain label starting and ending with an alphanumeric character
and possibly also containing "-" characters.  The rightmost domain
label of a fully qualified domain name in DNS may be followed by a
single "." and should be if it is necessary to distinguish between
the complete domain name and some local domain.

   reg-name    = *( unreserved / pct-encoded / "-" / ".")

If the URI scheme defines a default for host, then that default
```

**Notes:**

```
sub-delims  are defined as "!" / "$" / "&" / "'" / "(" / ")" / "*" / "+" / "," / ";" / "=" these are not characters that are allowed in host, domain or tld names, in addition the sub-delims do not sontain the "-" and "." 
therefore the reg-name (fully qualified domain name) definition is incorrect   

The same issue appears in "Appendix A."
```

---

## Erratum 5428 — Technical, Held for Document Update

Section 4.2; reported by Kevin Layer on 2018-07-17.

**Original text:**

```
      relative-part = "//" authority path-abempty
                    / path-absolute
                    / path-noscheme
                    / path-empty
```

**Corrected text:**

```
      relative-part = "//" authority path-abempty
                    / path-absolute
                    / path-noscheme
		    / path-abempty    ; this was added
                    / path-empty
```

**Notes:**

```
As written, the ABNF excludes "/" being a valid URI.  It is hard to believe that href="/" when converted to a URI, would be illegal.
```

---

## Erratum 2033 — Editorial, Held for Document Update

Section 3.3.; reported by Tanaka Akira on 2010-02-05.

**Original text:**

```
      path-empty    = 0<pchar>
```

**Corrected text:**

```
      path-empty    = ""
```

**Notes:**

```
According to ABNF, 0<pchar> is interpreted as
zero repeatation of the prose-val: pchar.

However pchar is an non-terminal.
So I think the production should be follows:
  path-empty = 0pchar

However this production means a empty string.
So
  path-empty = ""
is more clear.

Appendix A. has also the same production.
```

---

## Erratum 2933 — Editorial, Held for Document Update

Section 5.2.2.; reported by Bjoern Hoehrmann on 2011-08-13.

**Original text:**

```
T.path = merge(Base.path, R.path);
```

**Corrected text:**

```
T.path = merge(Base, R);
```

**Notes:**

```
In 5.2.3. the "If the base URI has a defined authority component" condition requires knowing the authority component, so passing just the path component is misleading.

During discussion of this issue, Roy Fielding noted: "No, this is not an error in the algorithm.  The algorithm is just saying that you need to merge the two paths.  The two paths are not parameters to a function, nor is the merge procedure limited to those two parameters." Saving this report as "Hold For Document Update" so that future implementers do not experience the same confusion.
```

---

## Erratum 3330 — Technical, Rejected

Section Appendix A; reported by David Sheets on 2012-08-29.

**Original text:**

```
fragment      = *( pchar / "/" / "?" )
```

**Corrected text:**

```
fragment      = *( pchar / "/" / "?" / "#" )
```

**Notes:**

```
Appendix B's regex doesn't fail with this correction. Additionally, this gives freedom to media type designers. Specifically, the ((path?),(query?),(fragment?)) subsyntax could be reused in hypermedia type design as the "?" delimiter transitions path => query and the "#" delimiter transitions query => fragment. It also follows the pattern:
path         = *( pchar / "/" )
query         = *( pchar / "/" / "?" )
fragment      = *( pchar / "/" / "?" / "#" )
 --VERIFIER NOTES-- 
This is something that should be looked at further, but it is not an error in the spec and is unlikely to be a direct change we'd make in a revision of the spec.

Some applications at the time the specification was written parsed the fragment from left to right, and others parsed from right to left, which means they would get different results if "#" were allowed inside of a fragment.  That's why it was not allowed in the ABNF.  It's possible that situation has improved in the years since, but it would be difficult to test so many implementations.  Deciding the right way to handle this goes beyond what can be handled by an erratum.
```

---

## Erratum 4293 — Technical, Rejected

Section Appendix B; reported by Cesar Crusius on 2015-03-07.

**Original text:**

```
^(([^:/?#]+):)?(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?
```

**Corrected text:**

```
^(([^:/?#]+):)(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?
```

**Notes:**

```
The regular expression makes the scheme part optional, but both the ABNF in Appendix A and the text in Section 3 state that the scheme is in fact required.
 --VERIFIER NOTES-- 
The context is that this is for parsing references:

   The following line is the regular expression for breaking-down a
   well-formed URI reference into its components.

The ABNF makes it clear that the scheme is, in fact, NOT required for URI references (see the ABNF for the "relative-ref" production).
```

---

## Erratum 4393 — Technical, Rejected

Section 3.2.2; reported by Steven Liekens on 2015-06-15.

**Original text:**

```
dec-octet   = DIGIT                 ; 0-9
            / %x31-39 DIGIT         ; 10-99
            / "1" 2DIGIT            ; 100-199
            / "2" %x30-34 DIGIT     ; 200-249
            / "25" %x30-35          ; 250-255
```

**Corrected text:**

```
dec-octet = "25" %x30-35          ; 250-255
          / "2" %x30-34 DIGIT     ; 200-249
          / "1" 2DIGIT            ; 100-199
          / %x31-39 DIGIT         ; 10-99
          / DIGIT                 ; 0-9
```

**Notes:**

```
The 'dec-octet' rule requires more than 1 lookahead symbol.

Example value: 127

Parsers that implement a first-match-wins strategy will erroneously match 127 as ( DIGIT ), followed by two unexpected symbols.

Parsers that implement a first-match-wins strategy with the corrected grammar will correctly match 127 as ( "1" 2DIGIT ).
 --VERIFIER NOTES-- 
Yes, except that ABNF defined in RFC 5234 is designed to describe what's valid to produce -- it's not designed as the definitive source for building a perfect parser.  In particular, the order of the alternatives is not significant in ABNF, so reordering them is irrelevant.

In fact, the ABNF here is more specific than it often is.  In other RFCs it will say things like
   xyz = 0 / %x31-39 *2DIGIT ; valid values are 0-255
...and just let the comment restrict the maximum value.
```

---

## Erratum 4394 — Technical, Rejected

Section 3.2.2; reported by Steven Liekens on 2015-06-15.

**Original text:**

```
IPv6address =                            6( h16 ":" ) ls32
                 /                       "::" 5( h16 ":" ) ls32
                 / [               h16 ] "::" 4( h16 ":" ) ls32
                 / [ *1( h16 ":" ) h16 ] "::" 3( h16 ":" ) ls32
                 / [ *2( h16 ":" ) h16 ] "::" 2( h16 ":" ) ls32
                 / [ *3( h16 ":" ) h16 ] "::"    h16 ":"   ls32
                 / [ *4( h16 ":" ) h16 ] "::"              ls32
                 / [ *5( h16 ":" ) h16 ] "::"              h16
                 / [ *6( h16 ":" ) h16 ] "::"
```

**Corrected text:**

```
IPv6address = (                6( h16 ":" ) ls32 ) 
            / (           "::" 5( h16 ":" ) ls32 )
            / ( [   h16 ] "::" 4( h16 ":" ) ls32 )
            / ( [ h16-2 ] "::" 3( h16 ":" ) ls32 )
            / ( [ h16-3 ] "::" 2( h16 ":" ) ls32 )
            / ( [ h16-4 ] "::"    h16 ":"   ls32 )
            / ( [ h16-5 ] "::"              ls32 )
            / ( [ h16-6 ] "::"               h16 )
            / ( [ h16-7 ] "::"                   )

h16-7       = ( *6( h16 ":" ) h16 ) / h16-6

h16-6       = ( *5( h16 ":" ) h16 ) / h16-5

h16-5       = ( *4( h16 ":" ) h16 ) / h16-4

h16-4       = ( *3( h16 ":" ) h16 ) / h16-3

h16-3       = ( *2( h16 ":" ) h16 ) / h16-2

h16-2       = (   [ h16 ":" ] h16 ) / h16
```

**Notes:**

```
The 'IPv6address' rule requires more than 1 lookahead symbol.

Example value: 1::

Parsers that implement a first-match-wins strategy will erroneously match 1:: as ( h16 ":" ), followed by an unexpected symbol.

Parsers that implement a first-match-wins strategy with the corrected grammar will correctly match 1:: as ( h16 "::" ).
 --VERIFIER NOTES-- 
As with errata report 4393, this is trying to use ABNF beyond what it's meant for.
```

---

## Erratum 7019 — Technical, Rejected

Section B; reported by Tim McSweeney on 2022-07-10.

**Original text:**

```
^(([^:/?#]+):)?(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?
```

**Corrected text:**

```
^(([^:/?#]+):#)?(//([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?
```

**Notes:**

```
Added the missing '#" delimiter.
 --VERIFIER NOTES-- 
In rejecting this Errata report I note that the reported error is not an error, but a deliberate decision of the authors and working group. The change, therefore, if it is to be applied needs to be achieved through a consensus document and definitely not via an errata report.
```

---

## Erratum 8442 — Technical, Rejected

Section 4.1; reported by Timothy McSweeney on 2025-06-01.

**Original text:**

```
Readers familiar with regular expressions should see Appendix B for an example of a non-validating URI-reference parser that will take any given string and extract the URI components.
```

**Corrected text:**

```
"any given string" is the problem here.  You can come up with the corrected text yourself.
```

**Notes:**

```
The original statement is false.
 --VERIFIER NOTES-- 
 The reporter has not supplied actual corrected text nor notes with sufficient explanation of the issue. This errata was discussed on the art@ietf.org list with the conclusion to reject this errata as too vague.
```

---

## Erratum 1783 — Editorial, Rejected

Section 3.1.; reported by Christopher Yeleighton on 2009-05-15.

**Original text:**

```
Advice for designers of new URI schemes can be found in [RFC2718].
```

**Corrected text:**

```
Advice for designers of new URL schemes can be found in [RFC2718].
```

**Notes:**

```
[RFC2718] does not contain advice for designers of new URN schemes; it is applies to URL schemes only and it is titled accordingly.
The information as published is misleading.
 --VERIFIER NOTES-- 
   Given that RFC 4395 ("Guidelines and Registration Procedures for New URI Schemes") obsoletes the referenced RFC 2718 ("Guidelines for new URL Schemes"), this erratum is best considered in error.
```

---

## Erratum 2717 — Editorial, Rejected

Section 3; reported by Winfred Qin on 2011-02-14.

**Original text:**

```
      hier-part   = "//" authority path-abempty
                  / path-absolute
                  / path-rootless
                  / path-empty
```

**Corrected text:**

```
      hier-part   = "//" authority path-abempty
                  / path-absolute
                  / path-noscheme
                  / path-rootless
                  / path-empty
```

**Notes:**

```
There are four ABNF rules for path, but the following words says:

'These restrictions result in five different ABNF rules for a path (Section 3.3)'

And in section 3.3, there are five rules.
 --VERIFIER NOTES-- 
   PSA: There is no error here, because the hierarchical part excludes
   paths that are not preceded by "//", whereas the path rule includes
   paths that are not preceded by "//" (thus five rules for "path" but
   only four rules for "hier-part").
```

