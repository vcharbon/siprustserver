# Errata for RFC 4566

Applies to [rfc4566.txt](rfc4566.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc4566>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 2 |
| Held for Document Update | 3 |
| Reported | 2 |
| Rejected | 3 |

---

## Erratum 1089 — Technical, Verified

Section 9; reported by Colin Perkins on 2007-12-06.

**Original text:**

```
IP6-multicast =       hexpart [ "/" integer ]

IP6-address =         hexpart [ ":" IP4-address ]

hexpart =             hexseq / hexseq "::" [ hexseq ] /
                              "::" [ hexseq ]

hexseq  =             hex4 *( ":" hex4)

hex4    =             1*4HEXDIG
```

**Corrected text:**

```
IP6-multicast =       IP6-address [ "/" integer ]

IP6-address =                                      6( h16 ":" ) ls32
                          /                       "::" 5( h16 ":" ) ls32
                          / [               h16 ] "::" 4( h16 ":" ) ls32
                          / [ *1( h16 ":" ) h16 ] "::" 3( h16 ":" ) ls32
                          / [ *2( h16 ":" ) h16 ] "::" 2( h16 ":" ) ls32
                          / [ *3( h16 ":" ) h16 ] "::"    h16 ":"   ls32
                          / [ *4( h16 ":" ) h16 ] "::"              ls32
                          / [ *5( h16 ":" ) h16 ] "::"              h16
                          / [ *6( h16 ":" ) h16 ] "::"

h16 =                 1*4HEXDIG

ls32 =                ( h16 ":" h16 ) / IP4-address
```

**Notes:**

```
Correct IPv6 ABNF.
```

---

## Erratum 5183 — Editorial, Verified

Section 6.  SDP Attr; reported by no space before encoding parameter in rtpmap value on 2017-11-14.

**Original text:**

```
 a=rtpmap:<payload type> <encoding name>/<clock rate> [/<encoding
         parameters>]
```

**Corrected text:**

```
 a=rtpmap:<payload type> <encoding name>/<clock rate>[/<encoding
         parameters>]
```

**Notes:**

```
rtpmap requires format below

a=rtpmap:97 L16/8000
a=rtpmap:98 L16/11025/2

no space between clock rate and optional parameter
```

---

## Erratum 1795 — Technical, Held for Document Update

Section 9; reported by Leandro Lucarella on 2009-06-19.

**Original text:**

```
   decimal-uchar =       DIGIT
                         / POS-DIGIT DIGIT
                         / ("1" 2*(DIGIT))
                         / ("2" ("0"/"1"/"2"/"3"/"4") DIGIT)
                         / ("2" "5" ("0"/"1"/"2"/"3"/"4"/"5"))
```

**Corrected text:**

```
   decimal-uchar =       DIGIT
                         / POS-DIGIT DIGIT
                         / ("1" 2(DIGIT))
                         / ("2" ("0"/"1"/"2"/"3"/"4") DIGIT)
                         / ("2" "5" ("0"/"1"/"2"/"3"/"4"/"5"))
```

**Notes:**

```
The "*" is removed from the 3rd line.

The current RFC accepts any number starting with "1" and followed by
2 or more digits (like 1000 or 123456789) as a valid "decimal-uchar"
because of this error, because "1" 2*(DIGIT) means 2 or more digits
following a "1". The correction, 2(DIGIT) means *exactly* 2 digits
following a "1", which covers the range 100-199.
```

---

## Erratum 2517 — Technical, Held for Document Update

Section 5.14; reported by Victor S. Osipov on 2010-09-13.

**Original text:**

```
If the <proto> sub-field is "RTP/AVP" or "RTP/SAVP" the <fmt> 
sub-fields contain RTP payload type numbers.  When a list of 
payload type numbers is given, this implies that all of these 
payload formats MAY be used in the session, but the first of these 
formats SHOULD be used as the default format for the session.
```

**Corrected text:**

```
If the <proto> sub-field is "RTP/AVP" or "RTP/SAVP" the <fmt> 
sub-fields contain RTP payload type numbers.  When a list of 
payload type numbers is given, this implies that all of these 
payload formats MAY be used in the session, and these payload 
formats are listed in order of preference, with the first format listed 
being preferred; in that case the first acceptable payload format 
from the beginning of the list SHOULD be used for the session.
```

**Notes:**

```
The word ‘default‘ is improper here, because payload format is not implied, but explicitly indicated. This may lead to a misunderstanding and confusion.
The corrections are proposed in order to avoid confusion and to give complete clear instructions in case of multiple payload type numbers indicated.
```

---

## Erratum 4903 — Technical, Held for Document Update

Section 9; reported by Jxck on 2017-01-12.

**Original text:**

```
   ; SDP Syntax
   session-description = proto-version
                         origin-field
                         session-name-field
                         information-field
                         uri-field
                         email-fields
                         phone-fields
                         connection-field
                         bandwidth-fields
                         time-fields
                         key-field
                         attribute-fields
                         media-descriptions

   connection-field =    [%x63 "=" nettype SP addrtype SP
                         connection-address CRLF]
                         ;a connection field must be present
                         ;in every media description or at the
                         ;session-level

   media-descriptions =  *( media-field
                         information-field
                         *connection-field
                         bandwidth-fields
                         key-field
                         attribute-fields )
```

**Corrected text:**

```
   ; SDP Syntax
   session-description = proto-version
                         origin-field
                         session-name-field
                         information-field
                         uri-field
                         email-fields
                         phone-fields
                         [connection-field]
                         bandwidth-fields
                         time-fields
                         key-field
                         attribute-fields
                         media-descriptions

   connection-field =    %x63 "=" nettype SP addrtype SP
                         connection-address CRLF
                         ;a connection field must be present
                         ;in every media description or at the
                         ;session-level

   media-descriptions =  *( media-field
                         information-field
                         *connection-field
                         bandwidth-fields
                         key-field
                         attribute-fields )
```

**Notes:**

```
in BNF of SDP, 
connection-field itself is optional [0, 1], but media-description has multiple [0,) connection-field.
it seems conflict, because multiple of optional fields doesn't finish because multiple filed can't find when to end.

so change connection-field itself not optional (remove []) and, replace session-descriptions connection-field as optional.
```

---

## Erratum 5595 — Technical, Reported

Section 5; reported by Gavin Scallan on 2019-01-08.

**Original text:**

```
An example SDP description is:

      v=0
      o=jdoe 2890844526 2890842807 IN IP4 10.47.16.5
      s=SDP Seminar
      i=A Seminar on the session description protocol
      u=http://www.example.com/seminars/sdp.pdf
      e=j.doe@example.com (Jane Doe)
      c=IN IP4 224.2.17.12/127
      t=2873397496 2873404696
      a=recvonly
      m=audio 49170 RTP/AVP 0
      m=video 51372 RTP/AVP 99
      a=rtpmap:99 h263-1998/90000
```

**Corrected text:**

```
An example SDP description is:

      v=0
      o=jdoe 2890844526 2890842807 IN IP4 10.47.16.5
      s=SDP Seminar
      i=A Seminar on the session description protocol
      u=http://www.example.com/seminars/sdp.pdf
      e=j.doe@example.com (Jane Doe)
      c=IN IP4 224.2.17.12/127
      a=recvonly
      t=2873397496 2873404696
      m=audio 49170 RTP/AVP 0
      m=video 51372 RTP/AVP 99
      a=rtpmap:99 h263-1998/90000
```

**Notes:**

```
In section 5 it indicates that the SDP lines MUST appear in the exact order given at the top of Page 9.
The order that is shown indicates that the Session Attributes  appear before the Time Description.

However in the example included in this section which explains that Media Attributes have precedence over Session Attributes for a media stream  contradicts the order as the  Session Attribute of sendonly is included below the  Time Description.

Either the example is incorrect, or the normative description is poorly explained and some variability in the order of lines in the SDP is allowed.
```

---

## Erratum 6022 — Technical, Reported

Section 9; reported by Megan Ruggiero on 2020-03-16.

**Original text:**

```
   ; sub-rules of 'e=', see RFC 2822 for definitions
   email-address        = address-and-comment / dispname-and-address
                          / addr-spec
   address-and-comment  = addr-spec 1*SP "(" 1*email-safe ")"
   dispname-and-address = 1*email-safe 1*SP "<" addr-spec ">"

   ; sub-rules of 'p='
   phone-number =        phone *SP "(" 1*email-safe ")" /
                         1*email-safe "<" phone ">" /
                         phone
```

**Corrected text:**

```
   ; sub-rules of 'e=', see RFC 2822 for definitions
   email-address        = address-and-comment / dispname-and-address
                          / addr-spec
   address-and-comment  = addr-spec 1*SP "(" 1*email-safe ")"
   dispname-and-address = 1*email-safe 1*SP "<" addr-spec ">"

   ; sub-rules of 'p='
   phone-number =        phone *SP "(" 1*email-safe ")" /
                         1*email-safe 1*SP "<" phone ">" /
                         phone
```

**Notes:**

```
There's an inconsistency between the definitions of dispname-and-address and phone-number. I am not sure if this is intentional or not, and in practice this doesn't change what's matched (as email-safe includes spaces), but I thought it'd be worth mentioning since I myself got tripped up when translating the grammar.

Alternatively, perhaps 1*SP should be removed from dispname-and-address.
```

---

## Erratum 57 — Technical, Rejected

Section (unspecified); reported by Alfred Hoenes on 2006-11-09.

**Original text:**

```
(1)  typo

On page 7 of RFC 4566, Section 4.6 says:

   The SDP specification recommends the use of the ISO 10646 character
|  sets in the UTF-8 encoding [5] to allow many different languages to
   be represented.  However, to assist in compact representations, SDP
   also allows other character sets such as ISO 8859-1 to be used when
   desired.  Internationalisation only applies to free-text fields
   (session name and background information), and not to SDP as a whole.

It should say:

   The SDP specification recommends the use of the ISO 10646 character
|  set in the UTF-8 encoding [5] to allow many different languages to
   be represented.  However, to assist in compact representations, SDP
   also allows other character sets such as ISO 8859-1 to be used when
   desired.  Internationalisation only applies to free-text fields
   (session name and background information), and not to SDP as a whole.


(2)  inconsistent specification

Contrary to the text in Section 4.6 (see above), Section 5 of RFC 4566
specifies, at the bottom of page 7:

   An SDP session description is entirely textual using the ISO 10646
   character set in UTF-8 encoding.  SDP field names and attribute names
   use only the US-ASCII subset of UTF-8, but textual fields and
   attribute values MAY use the full ISO 10646 character set.  [...]

These conflicting specifications might become a source of
interoperability problems, and therefore, the conflict should be
resolved by a clarification !!!

Note: Subsequent text in sections 5.* specifies behaviour related
to the "a=charset" attribute required if other charsets than UTF-8
are used in certain fields; therefore, the latter exclusive UTF-8
rule is most probably too restrictive and hence inappropriate.


(3)  misleading text / inconsistency + typo (missing article)

Still in Section 5, the second-to-last paragraph on page 8 of RFC 4566
says:

   An SDP session description consists of a session-level section
   followed by zero or more media-level sections.  The session-level
   part starts with a "v=" line and continues to the first media-level
|  section.  Each media-level section starts with an "m=" line and
|  continues to the next media-level section or end of the whole session
   description.  In general, session-level values are the default for
   all media unless overridden by an equivalent media-level value.

The second sentence does not accomodate for an important corner case
mentioned in the first sentence, and thus should be amended with
similar wording as below. The third sentence is imprecise as well,
because the "or" does not uniquely select the 'right' condition.

It should better say:

   An SDP session description consists of a session-level section
   followed by zero or more media-level sections.  The session-level
   part starts with a "v=" line and continues to the first media-level
|  section (or the end of the whole description, whichever comes first).
   Each media-level section starts with an "m=" line and continues to
|  the next media-level section or the end of the whole session
|  description - whichever comes first.  In general, session-level
   values are the default for all media unless overridden by an
   equivalent media-level value.


(4)  significant mis-specification

In the "Session description" block, on top of page 9, the RFC says:

         c=* (connection information -- not required if included in
              all media)
         b=* (zero or more bandwidth information lines)

It should say:

         c=* (connection information -- not required if included in
              all media descriptions)
         b=* (zero or more bandwidth information lines)

Rationale:
Strictly differentiating between "media" and "media description"
is always required to avoid confusion! ...
And connection information is rarely (if ever) seen in the media!


(5)  improper wording related with IP addresses

IP addresses (in IPv4 and IPv6) are always assigned to an interface,
not to a host or 'machine'.  Therefore, a Standards Track RFC
should never talk about  '*the* IP address of a machine'.
FQDNs are also not necessarily unique for a 'machine', e.g. a server
having multiple, 'role-based' FQDNs.

Therefore, in Section 5.2, the last paragraph on page 11,

|  <unicast-address> is the address of the machine from which the
      session was created.  For an address type of IP4, this is either
|     the fully qualified domain name of the machine or the dotted-
|     decimal representation of the IP version 4 address of the machine.
|     For an address type of IP6, this is either the fully qualified
      domain name of the machine or the compressed textual
|     representation of the IP version 6 address of the machine.  For
      both IP4 and IP6, the fully qualified domain name is the form that
|     SHOULD be given unless this is unavailable, in which case the
      globally unique address MAY be substituted.  A local IP address
      MUST NOT be used in any context where the SDP description might
      leave the scope in which the address is meaningful (for example, a
      local address MUST NOT be included in an application-level
      referral that might leave the scope).

should say:

|  <unicast-address> is an address of the machine from which the
      session was created.  For an address type of IP4, this is either
|     a fully qualified domain name of the machine or the dotted-
|     decimal representation of an IP version 4 address of the machine.
|     For an address type of IP6, this is either a fully qualified
      domain name of the machine or the compressed textual
|     representation of an IP version 6 address of the machine.  For
      both IP4 and IP6, the fully qualified domain name is the form that
|     SHOULD be given unless this is unavailable, in which case a
      globally unique address MAY be substituted.  A local IP address
      MUST NOT be used in any context where the SDP description might
      leave the scope in which the address is meaningful (for example, a
      local address MUST NOT be included in an application-level
      referral that might leave the scope).


(6)  improper term used

a)
Section 5.6 twice uses the term, "conference", where IMHO, according
to the definitions given in Section 2, the term, "session" should
be used.  This change makes the text better conform with the
classification of the "e=" and "p=" lines on page 9, as well.

The first textual paragraph of Section 5.6, on page 13, says:

   The "e=" and "p=" lines specify contact information for the person
|  responsible for the conference.  This is not necessarily the same
|  person that created the conference announcement.

It should say:

   The "e=" and "p=" lines specify contact information for the person
|  responsible for the session.  This is not necessarily the same person
|  that created the session announcement.

b)
Another instance of this issue occurs in Section 5.7, at the bottom
of page 14.  There, the RFC says:

   o  Sessions using an IPv4 multicast connection address MUST also have
      a time to live (TTL) value present in addition to the multicast
      address.  The TTL and the address together define the scope with
|     which multicast packets sent in this conference will be sent.  TTL
      values MUST be in the range 0-255.  [...]

It should say:

   o  Sessions using an IPv4 multicast connection address MUST also have
      a time to live (TTL) value present in addition to the multicast
      address.  The TTL and the address together define the scope with
|     which multicast packets sent in this session will be sent.  TTL
      values MUST be in the range 0-255.  [...]

c)
Finally, the last sentence of the second paragraph of Section 5.13,
on page 21,

                                                   [...].  Attribute
   fields can also be added before the first media field; these
   "session-level" attributes convey additional information that applies
   to the conference as a whole rather than to individual media.

should say, accordingly:

                                                   [...].  Attribute
   fields can also be added before the first media field; these
   "session-level" attributes convey additional information that applies
|  to the session as a whole rather than to individual media.


(7)  clarification

The second paragraph of Section 5.9, on page 17, says:

   The first and second sub-fields give the start and stop times,
   respectively, for the session.  These values are the decimal
   representation of Network Time Protocol (NTP) time values in seconds
   since 1900 [13].  To convert these values to UNIX time, subtract
   decimal 2208988800.

To make the time base unique and absolute, the time zone (UTC) needs to
be specified.
Hence, the RFC should say:

   The first and second sub-fields give the start and stop times,
   respectively, for the session.  These values are the decimal
   representation of Network Time Protocol (NTP) time values in seconds
|  since 1900, UTC [13].  To convert these values to UNIX time, subtract
   decimal 2208988800.


(8)  typo (missing article)

The second text paragraph of section 5.10, on page 18, says:

   To make description more compact, times may also be given in units of
   days, hours, or minutes.  [...]

It should say:

|  To make the description more compact, times may also be given in
   units of days, hours, or minutes.  [...]


(9)  legacy term not replaced

Since the advent of SIP and the Offer/Answer Model for SDP, it is
no more appropriate, in a general context, to name a
'session description' just a 'session announcement'.

The final paragraph of Section 5.11, on page 19, says:

   If a session is likely to last several years, it is expected that the
   session announcement will be modified periodically rather than
   transmit several years' worth of adjustments in one session
   announcement.

It should say therefore:

   If a session is likely to last several years, it is expected that the
|  session description will be modified periodically rather than
|  transmitting several years' worth of adjustments in one session
|  description.


(10) clarification of 'charset' related terminology, plus typos

Section 5.13 makes use of legacy terminology related to character
sets and "charsets".  It should use the stable, IESG policy based
IETF terminology.

a)
Hence, the first paragraph on page 22:

   Attribute values are octet strings, and MAY use any octet value
   except 0x00 (Nul), 0x0A (LF), and 0x0D (CR).  By default, attribute
   values are to be interpreted as in ISO-10646 character set with UTF-8
   encoding.  Unlike other text fields, attribute values are NOT
   normally affected by the "charset" attribute as this would make
   comparisons against known values problematic.  However, when an
   attribute is defined, it can be defined to be charset dependent, in
   which case its value should be interpreted in the session charset
   rather than in ISO-10646.

should better say:

   Attribute values are octet strings, and MAY use any octet value
   except 0x00 (Nul), 0x0A (LF), and 0x0D (CR).  By default, attribute
|  values are to be interpreted as in the ISO-10646 character set with
   UTF-8 encoding.  Unlike other text fields, attribute values are NOT
   normally affected by the "charset" attribute as this would make
   comparisons against known values problematic.  However, when an
   attribute is defined, it can be defined to be charset dependent, in
   which case its value should be interpreted in the session charset
   rather than in UTF-8.

Note: RFC 1815 has been deprecated; 'ISO-10646' is *not* considered a
"charset", whereas 'UTF-8' is.

b)
In Section 6, at the bottom of page 28, the RFC says:

      a=charset:<character set>

         This specifies the character set to be used to display the
         session name and information data.  By default, the ISO-10646
         character set in UTF-8 encoding is used.  If a more compact
         representation is required, other character sets may be used.
         For example, the ISO 8859-1 is specified with the following SDP
         attribute:

            a=charset:ISO-8859-1

The above headline should say:

      a=charset:<charset>

or perhaps even better:

      a=charset:<IANA-charset>

and the text body above should be changed to say:

|        This specifies the character set and encoding ("charset") to be
|        used for the session name and information data.  By default,
|        the ISO-10646 character set in UTF-8 encoding (charset "UTF-8")
         is used.  If a more compact representation is required, other
|        charsets may be used.  For example, the ISO 8859-1 charset is
         specified with the following SDP attribute:
         [...]

Note: The session description does not determine how parts of it
are *displayed* (theres may be some transcoding used, and fonts
or typefaces, etc., the charset must only be specified for SDP.)

The subsequent text on page 29,

         This is a session-level attribute and is not dependent on
         charset.  The charset specified MUST be one of those registered
         with IANA, such as ISO-8859-1.  The character set identifier is
         a US-ASCII string and MUST be compared against the IANA
         identifiers using a case-insensitive comparison.  If the
         identifier is not recognised or not supported, all strings that
         are affected by it SHOULD be regarded as octet strings.

         Note that a character set specified MUST still prohibit the use
         of bytes 0x00 (Nul), 0x0A (LF), and 0x0d (CR).  Character sets
         requiring the use of these characters MUST define a quoting
         mechanism that prevents these bytes from appearing within text
         fields.

should be changed to say (fixing typos as well):

         This is a session-level attribute and is not dependent on
         charset.  The charset specified MUST be one of those registered
|        with IANA, such as ISO-8859-1.  The charset identifier is a
         US-ASCII string and MUST be compared against the IANA
         identifiers using a case-insensitive comparison.  If the
         identifier is not recognised or not supported, all strings that
         are affected by it SHOULD be regarded as octet strings.

|        Note that a charset specified MUST still prohibit the use of
|        bytes 0x00 (NUL), 0x0A (LF), and 0x0D (CR).  Charsets requiring
         the use of these characters MUST define a quoting mechanism
         that prevents these bytes from appearing within text fields.


(11)  language related issues, plus typos

The RFC text on pp. 29/30 does not properly distinguish between, and
messes up, the specifics of session content (and its language[s]) and
the session *description* content (and the language[s] used therein).

Note: I show more context than absolutely necessary to perform the
recommended changes, to make these better understandable.

a)
On mid-page 29, RFC 4566 says:

      a=sdplang:<language tag>

         This can be a session-level attribute or a media-level
         attribute.  As a session-level attribute, it specifies the
         language for the session description.  As a media-level
         attribute, it specifies the language for any media-level SDP
         information field associated with that media.  Multiple sdplang
         attributes can be provided either at session or media level if
|        multiple languages in the session description or media use
|        multiple languages, in which case the order of the attributes
|        indicates the order of importance of the various languages in
|        the session or media from most important to least important.

(The last half-sentence is wrong and inappropriate.)

It should better say:

      a=sdplang:<language tag>

         This can be a session-level attribute or a media-level
         attribute.  As a session-level attribute, it specifies the
         language for the session description.  As a media-level
         attribute, it specifies the language for any media-level SDP
         information field associated with that media.  Multiple sdplang
         attributes can be provided either at session or media level if
|        multiple languages in the session or media description use
|        multiple languages.

b)
Skipping one paragraph, the next one says:

         The "sdplang" attribute value must be a single RFC 3066
         language tag in US-ASCII [9].  It is not dependent on the
         charset attribute.  An "sdplang" attribute SHOULD be specified
|        when a session is of sufficient scope to cross geographic
         boundaries where the language of recipients cannot be assumed,
|        or where the session is in a different language from the
         locally assumed norm.

It should say:

         The "sdplang" attribute value must be a single RFC 3066
         language tag in US-ASCII [9].  It is not dependent on the
         charset attribute.  An "sdplang" attribute SHOULD be specified
|        when a session description is of sufficient scope to cross
         geographic boundaries where the language of recipients cannot
|        be assumed, or where the session description is in a different
         language from the locally assumed norm.

c)
Next, in the explanations for:

      a=lang:<language tag>

extending from page 29 to page 30,

         This can be a session-level attribute or a media-level
         attribute.  As a session-level attribute, it specifies the
         default language for the session being described.  As a media-
         level attribute, it specifies the language for that media,
  <<page break>>
         overriding any session-level language specified.  Multiple lang
         attributes can be provided either at session or media level if
|        the session description or media use multiple languages, in
         which case the order of the attributes indicates the order of
         importance of the various languages in the session or media
         from most important to least important.

should be corrected to say:

         This can be a session-level attribute or a media-level
         attribute.  As a session-level attribute, it specifies the
         default language for the session being described.  As a media-
         level attribute, it specifies the language for that media,
  <<page break>>
         overriding any session-level language specified.  Multiple lang
         attributes can be provided either at session or media level if
|        the session or media use multiple languages, in which case the
         order of the attributes indicates the order of importance of
|        the various languages in the session or media, from most
         important to least important.




Note: In the meantime (since the publication of RFC 4566), RFC 3066
has been superseded by RFC 4646 plus RFC 4647, now together denoted
as BCP 47.


(12)  typo

On page 31, in the second paragraph of Section 7, RFC 4566 says:

            [...].  Many different transport protocols may be used to
   distribute session description, and the nature of the authentication
   will differ from transport to transport.  [...]

It should say:

            [...].  Many different transport protocols may be used to
|  distribute session descriptions, and the nature of the authentication
   will differ from transport to transport.  [...]


(13)  SDP Grammar issues  (ABNF in Section 9, p. 39 ff.),

a)
The rule for 'repeat-interval'  (page 41)  could easily have
re-used (incorporated) the 'integer' rule:

   repeat-interval =     POS-DIGIT *DIGIT [fixed-len-time-unit]

is equivalent to:

   repeat-interval =     integer [fixed-len-time-unit]

b)
The rule for FQDN, at the bottom of page 42, is wrong;
FQDNs really should allow up to 255 characters!
                         v
|  FQDN =                4*(alpha-numeric / "-" / ".")
                         ; fully qualified domain name as specified
                         ; in RFC 1035 (and updates)

Because details are not gived (can be found at other places),
this rule should perhaps be only minimally corrected to say:

|  FQDN =                *(alpha-numeric / "-" / ".")
                         ; fully qualified domain name as specified
                         ; in RFC 1035 (and updates)


At a minimum, serious issues should be addressed,
e.g. (2), (10), (11), and the ABNF mistake (13b).
```

**Corrected text:** *(none given)*

**Notes:**

```
from pending

--VERIFIER COMMENT--
While you raise a number of valid minor issues, I see nothing which
needs urgent attention or correction. I'll keep a record of these
issues, to be included if there is a revision to RFC 4566, but I see
no need to submit an errata note to the RFC Editor.
```

---

## Erratum 3278 — Technical, Rejected

Section 5; reported by Riccardo Bernardini on 2012-07-03.

**Original text:**

```
At page 9:


      Media description, if present
         m=  (media name and transport address)
```

**Corrected text:**

```

      Media description, if present
         m=* (media name and transport address)

```

**Notes:**

```
A '*' is added making "m=" lines optional

Rationale: 
The description about media lines in section 5, page 9 is conflicting with the ABNF syntax of media-description in Section 9, page 40

media-descriptions =  *( media-field
                         information-field
                         *connection-field
                         bandwidth-fields
                         key-field
                         attribute-fields )

According to the ABNF grammar, an SDP description can have zero "m=" lines, but according to the description at page 9, at least one line must be present.  Note that the conflict could be solved also by replacing the media-descriptions definition with 

   media-descriptions =  1*( media-field ... )

but at page 8 (still Section 5) it is said

   An SDP session description consists of a session-level section
   followed by zero or more media-level sections. ...
               ^^^^^^^^^^^^^^^^^^^^^^^^

so, I guess that the ABNF grammar is correct.
 --VERIFIER NOTES-- 
 Please see the discussion at  http://www.ietf.org/mail-archive/web/mmusic/current/msg09398.html
```

---

## Erratum 4769 — Technical, Rejected

Section 5.3.; reported by Petr Vaněk on 2016-08-08.

**Original text:**

```
5.3.  Session Name ("s=")

      s=<session name>

   The "s=" field is the textual session name.  There MUST be one and
   only one "s=" field per session description.  The "s=" field MUST NOT
   be empty and SHOULD contain ISO 10646 characters (but see also the
   "a=charset" attribute).  If a session has no meaningful name, the
   value "s= " SHOULD be used (i.e., a single space as the session
   name).
```

**Corrected text:**

```
5.3.  Session Name ("s=")

      s=<session name>

   The "s=" field is the textual session name.  There MUST be one and
   only one "s=" field per session description.  The "s=" field MUST NOT
   be empty and SHOULD contain ISO 10646 characters (but see also the
   "a=charset" attribute).  If a session has no meaningful name, the
   value "s=-" SHOULD be used (i.e., a single dash as the session
   name).
```

**Notes:**

```
In 3rd paragraph of section 5. is written: whitespace MUST NOT be used on either side of the "=" sign, which is incosistent with subsection 5.3. Therefore I suggest to use "-" (single dash) instead of " " (single space) for sessions without meaningful name.
 --VERIFIER NOTES-- 
 The mmusic working group is in the process of updating RFC 4566. If the issue applies to the updated text ( https://datatracker.ietf.org/doc/draft-ietf-mmusic-rfc4566bis/ ), please send comments to mmusic@ietf.org rather than file errata reports against RFC 4566.
```

