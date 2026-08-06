# Errata for RFC 5022

Applies to [rfc5022.txt](rfc5022.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc5022>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 15 |
| Rejected | 8 |

---

## Erratum 1239 — Technical, Verified

Section 6.1.1.1; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   Attributes of <audio>:

   o  url - required, no default value: The URL of the content to be
      retrieved and played.  The target may be a local or remote (NFS)
      "file://" scheme URL or an "http://" or "https://" scheme URL.  If
      the URL is not fully qualified and a "baseurl" attribute was set,
      the value of the "baseurl" attribute will be prepended to this
      value to generate the target URL.
```

**Corrected text:**

```
< see Notes >
```

**Notes:**

```
Again, the RFC should make use of standard terminology
established in the IETF.
STD 66, RFC 3986 should be used here.
The attribute, 'url', should better have been named 'uri'.

Consequently, all subsequent references to an URL w.r.t. this
attribute should be replaced by "URI" or "URI reference", as
appropriate.

Authors comment: This is really an Editorial erratum.  If it were to be changed, it should be changed consistently throughout the document.
```

---

## Erratum 1242 — Technical, Verified

Section 6.1.1.1.; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   o  rate - optional, default value "0": Specifies the absolute
      playback rate of the content relative to normal as either a
      positive percentage (faster) or a negative percentage (slower).
      Any value that attempts to set the rate above the maximum allowed
      or below the minimum allowed silently sets the rate to the maximum
      or minimum.  The rate reverts back to its original value when
      playback of the content URL has been completed.
```

**Corrected text:**

```
|  o  rate - optional, default value "0": Specifies the deviation of the
|     content playback rate from its normal value as either a positive
      percentage (faster) or a negative percentage (slower).
      Any value that attempts to set the rate above the maximum allowed
      or below the minimum allowed silently sets the rate to the maximum
      or minimum.  The rate reverts back to its original value when
|     playback of the content specified by the URI has been completed.

```

**Notes:**

```
There is perfect confusion between 'absolute' and 'relative",
leaving this specification open to gross misunderstanding.

I hope the proposed replacement text says what had been intended
to say; it also corrects sluggish language and applies STD 66 terms.
```

---

## Erratum 1247 — Technical, Verified

Section 6.5.3, p.41; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   The recording example (Figure 19) plays a prompt and records it to
   the destination specified in the "recurl" attribute encoded as MS-GSM
   in wave format.
```

**Corrected text:**

```
|  The recording example (Figure 19) plays a prompt and records the
|  response to the destination specified in the "recurl" attribute
   encoded as MS-GSM in wave format.
```

**Notes:**

```
The RFC text is ambiguous/misleading; what does 'it' refer to ?

I guess that nobody will be interested in recording the prompt.
Therefore, I hope the clarification above says what has been intended.
```

---

## Erratum 1248 — Technical, Verified

Section 8, page 48; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   Managecontent Attributes:

   o  src - required, no default value: Specifies the local source URL
      of the content.  The URL scheme MUST be "file://".

   o  dest - required (see note), no default value: Specifies the
      destination URL.  The URL scheme MUST be "http://".  Note: If the
      selected action is 'delete', this attribute is optional; otherwise
      it is required.
```

**Corrected text:**

```
   Managecontent Attributes:

|  o  src - required, no default value: Specifies the local source URI
|     of the content.  The URI scheme MUST be "file".

   o  dest - required (see note), no default value: Specifies the
|     destination URI.  The URI scheme MUST be "http".  Note: If the
      selected action is 'delete', this attribute is optional; otherwise
      it is required.
```

**Notes:**

```
a)
The RFC should follow STD 66, RFC 3986 terminology and syntax.
URI schemes do not allow ':' and '/' -- see Section 3.1 (p.17) of RFC 3986.

b)  ... MUST be "http" ...
Why the hack does the RFC explicitely forbid using HTTP security
and the "https" URI scheme ????
(I do not believe the authors expect the use of "Upgrade to TLS"
within HTTP, which is rarely implemented -- although it once was
intended as a generice security layer drop-in mechanism.)

Authors comment:  Yes, HTTP is a MUST implement, though not the only option here.
```

---

## Erratum 1234 — Editorial, Verified

Section 5.5; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   A client can modify a leg by issuing an INFO on the dialog associated
 | with the participant leg.  For example, Figure 8 mutes a conference
   leg.
```

**Corrected text:**

```
   A client can modify a leg by issuing an INFO on the dialog associated
 | with the participant leg.  For example, the request in Figure 8 mutes
   a conference leg.
```

**Notes:**

```
Text below Figure 7 (on page 15) is misleading.
Figure 8 is printed on the paper sheet in my hand
and does not mute anything   :-)
```

---

## Erratum 1235 — Editorial, Verified

Section 5.7, 3rd par; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
|  A request for an active talker report is in Figure 9.  The active
   talker report enumerates the current call legs in the mix.
```

**Corrected text:**

```
|  A request for an active talker report is shown in Figure 9.  The
   active talker report enumerates the current call legs in the mix.
```

**Notes:**

```
Improved language.
```

---

## Erratum 1236 — Editorial, Verified

Section 5.8.2.3; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   The corresponding MSCML request is as follows.
   <?xml version="1.0"?>
   <MediaServerControl version="1.0">
   ...

   Figure 12: Join Agent Request
```

**Corrected text:**

```
   The corresponding MSCML request is as follows.
|
   <?xml version="1.0"?>
   <MediaServerControl version="1.0">
   ...

   Figure 12: Join Agent Request

```

**Notes:**

```
On page 22, the XML shown in Figure 12 should have been separated
from the preceding text with a blank line.

The same issue recurs in Section 5.8.2.4 below (on the same page),
with respect to Figure 13.
```

---

## Erratum 1240 — Editorial, Verified

Section 6.1.1.1.; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   o  gain - optional, default value "0": Sets the absolute gain to be
      applied to the content URL.  [...]
```

**Corrected text:**

```
   o  gain - optional, default value "0": Sets the absolute gain to be
      applied to the content specified by the URI.  [...]
```

**Notes:**

```
Sluggish language --
can't mute an URI, or apply gain to an URI !   :-)

Note that other errata request the change from URL to URI throughout.
```

---

## Erratum 1241 — Editorial, Verified

Section 6.1.1.1; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   o  gaindelta - optional, default value "0": Sets the relative gain to
|     be applied to the content URL.  The value of this attribute is
      specified in units of dB.  The media server MAY silently cap
      values that exceed the gain limits imposed by the platform.  The
      level reverts back to its original value when playback of the
|     content URL has been completed.
```

**Corrected text:**

```
   o  gaindelta - optional, default value "0": Sets the relative gain to
|     be applied to the content specified by the URI.  The value of this
      attribute is specified in units of dB.  The media server MAY
      silently cap values that exceed the gain limits imposed by the
      platform.  The level reverts back to its original value when
|     playback of the content has been completed.
```

**Notes:**

```
Again, sluggish language (and non-use of STD 66 terminology).
```

---

## Erratum 1243 — Editorial, Verified

Section 6.1.1.1 p.29; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   Spoken variables are specified using the <variable> element.  This
   element has the attributes described in the list below.  MSCML's
|  spoken variables are based on those described in Audio Server
   Protocol [17].
```

**Corrected text:**

```
   Spoken variables are specified using the <variable> element.  This
   element has the attributes described in the list below.  MSCML's
|  spoken variables are based on those described in the Audio Server
   Protocol [17].
```

**Notes:**

```
Missing article (mid-page 29).

Author comment:  Good catch.
```

---

## Erratum 1245 — Editorial, Verified

Section 6.4.3; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
a) 
    ... "immediate" and "infinite."       (3 instances)          


b)
    ... returnkey is set to #, ...
    
```

**Corrected text:**

```
a) 
    ... "immediate" and "infinite".       (3 instances)          


b)
    ... returnkey is set to "#", ...
 
```

**Notes:**

```
a)  Syntax error; apply 'rational quotation' !
    The issue occurs in the 1st, 3rd, and 4th bullet on page 35.

b)  Clarification; the single pound character needs to be double-quoted
    in the XML anyway, isn't it?
    This also will make the text more uniform.
```

---

## Erratum 1246 — Editorial, Verified

Section 6.5.2, p.40; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
."
```

**Corrected text:**

```
".
```

**Notes:**

```
Again, in the second to seventh bullet on page 40,
'rational quotation' should be applied (6 instances).
```

---

## Erratum 1249 — Editorial, Verified

Section 8, page 49; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   o  name - required (see note), no default value: Specifies the field
      name for the content in the form when using the 'post' method.
      This is not to be confused with the "src" or "dest" attributes.
|     Note: This attribute is required when the "htttpmethod" has the
      value "post" and is optional otherwise.
```

**Corrected text:**

```
   o  name - required (see note), no default value: Specifies the field
      name for the content in the form when using the 'post' method.
      This is not to be confused with the "src" or "dest" attributes.
|     Note: This attribute is required when the "httpmethod" attribute
      has the value "post" and is optional otherwise.
```

**Notes:**

```
A typo, and sluggishly inprecise use of language.
```

---

## Erratum 1251 — Editorial, Verified

Section 8.1, last pa; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
."               (2 instances)
```

**Corrected text:**

```
".
```

**Notes:**

```
The last paragraph of Section 8.1 again contains two syntax errors
based non non-pplication of 'rational quoting'.
```

---

## Erratum 1254 — Editorial, Verified

Section 9.2, Table 5; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
... it MUST match remote terminal's identifier ...
```

**Corrected text:**

```
... it MUST match the remote terminal's identifier ...
```

**Notes:**

```
Insert the missing article in the 5th line from the bottom of Figure 5.
```

---

## Erratum 1233 — Technical, Rejected

Section 5.2; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   Attributes of <configure_conference>:

   o  reservedtalkers - optional (see note), no default value: The
      maximum number of talker legs allocated for the conference.  Note:
+     required when establishing the Conference Control Leg but optional
+     in subsequent <configure_conference> requests.

  o  reserveconfmedia - optional, default value "yes": Controls
      allocation of resources to enable playing or recording to or from
?     the entire conference.
?
?  When the reservedtalkers+1st INVITE arrives at the media server, the
   media server SHOULD generate a 486 Busy Here response.  Failure to
   send a 486 response to this condition can cause the media server to
   oversubscribe its resources.
```

**Corrected text:**

```
< amended/clarified text should be supplied by authors >
```

**Notes:**

```
The text spanning from page 12 to page 13 lacks of precision.

In the first bullet, it precisely specifies in which kind of leg
the attribute applies.
Similar information is missing entirely from the second bullet.
Also, it is not unambiguously clear whether "reservedtalkers+1st INVITE"
shall count the control leg or not.

Without added clarification, interoperability might suffer.

Authors response: In four years of deployment, no one has run into a real problem in the real world.  No change needed.

 --VERIFIER NOTES--
```

---

## Erratum 1237 — Technical, Rejected

Section 6.1.1; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
URL       and        URLs
```

**Corrected text:**

```
URI       and        URIs
```

**Notes:**

```
Throughout Section 6.1.1. ff., the RFC should better use
IETF standard terminology as codified in STD 66, RFC 3986.

In particular, the attribute name "baseurl" is a misnomer
and should better have been named "baseuri" (bottom of page 24).
 --VERIFIER NOTES-- 
   Author comment: Sounds editorial to me. If you want to change it everywhere, go for it.  Changing baseurl to baseuri would change the protocol. Since it really is a seven-character string that seems to mean something, no harm in leaving it as is.
```

---

## Erratum 1238 — Technical, Rejected

Section 6.1.1; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   o  locale - optional, no default value: Specifies the language and
      country variant used for resolving spoken variables.  The language
      is defined as a two-letter code per ISO 639.  The country variant
      is also defined as a two-letter code per ISO 3166.  These codes
      are concatenated with a single underscore (%x5F) character.
```

**Corrected text:**

```
< to be specified >
```

**Notes:**

```
The RFC should better apply existing IETF standards and not
try to establish ad-hoc usage conventions and/or syntaxes
for details already well specified in the IETF.

In particular, it is considered an error (on top of page 26)
to not apply, and refer to, RFC 4646 (BCP 47) for the
definition of language tags. 
RFC 4646 deliberately has modified and extended the earlier
conventions roughly corresponding to what is described here.
 --VERIFIER NOTES-- 
   Authors comment: BCP 47 wasn't out yet when the base spec was published.
A new protocol - hence a new RFC - should make the changes suggested here.
```

---

## Erratum 1244 — Technical, Rejected

Section 6.1.1.1 p.29; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   Attributes of <variable>:

   o  type - required, no default value: Specifies the major type format
      of the spoken variable to be played.  Allowable values are "dat"
      (date), "dig" (digit), "dur" (duration), "mth" (month), "mny"
      (money), "num" (number), "sil" (silence), "str" (string), "tme"
      (time), and "wkd" (weekday).

   o  subtype - optional, no default value: Specifies the minor type
      format of the spoken variable to be played.  Allowable values
      depend on the value of the corresponding "type" attribute.
      Possible values are "mdy", "ymd", and "dmy" for dates, "t12" and
      "t24" for times, "gen", "ndn", "crd", and "ord" for digits, and
      "USD" for money.
```

**Corrected text:**

```
< see Notes >
```

**Notes:**

```
The RFC here underspecifies many important details necessary
to be specified precisely to ensure interoperability:

a)  What is the intended range of values for "wkd" ?
    I guess, it may be  "0", ..., "6" .
    Or is it  "1", ..., "7" ??
    (Textual values make no sense -- or at least would make
    implementations overly complicated requiring an any-language
    to any-languange translation facility -- because the value
    needs to be played out in the intended language according
    to effective preferences.)

b)  What are  "gen", "ndn", "crd", and "ord"  ?
    No details are given to explain these abbreviations, and
    most RFC readers will not be accustomed either to them.
    I guess that "ord" might mean 'ordinal', "crd"  be
    'cardinal', and perhaps "gen" for 'generic', but "ndn" ?
    The semantics need to be specified carefully for interoperability!

c)  Another instance of overly US-centric thinking is the single "USD"
    for money.  To ensure wide-spread applicability and maintainability
    of the protocol, the RFC should better apply the standard method
    of incorporating the applicable 'official' registry by *reference*,
    which in this case is the list of monetary units and their
    internationally agreed-upon / assigned three-character
    abbreviations from the ISO 4217 Maintenance Agency.
 --VERIFIER NOTES-- 
   Authors comment: This is an Informational RFC, not a Standards Track one.
```

---

## Erratum 1253 — Technical, Rejected

Section 9.2; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   Attributes of <faxplay>:

   o  lclid - optional, default value "" (the empty string): A string
      that identifies the called station.
```

**Corrected text:**

```
   Attributes of <faxplay>:

   o  lclid - optional, default value "" (the empty string): A string
|     that identifies the calling station.
```

**Notes:**

```
I suspect that the RFC specifies the wrong station ID.
In Section 9.1, "lclid" clearly specifies the PSTN 'role'
the Media Server shall assume in processing a fax in answer mode;
there, it makes no sense to specify an identity that is not under
control of the media server accepting the incoming call.
Similarly, for <faxplay> in Section 9.2, the local ID the Media
Server shall assume in sending the fax needs to be specified;
as far as I understand the text, the Media Server is the *calling*
station in this scenario.
Should I be wrong with this diagnosis, perhaps other claraifcation(s)
to the RFC are needed.
 --VERIFIER NOTES-- 
   Authors comment: Incorrect diagnosis.
```

---

## Erratum 1232 — Editorial, Rejected

Section 5.1, page 11; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
   When the conference is created by sending an INVITE containing a
   MSCML <configure_conference> payload, the resulting SIP dialog is
|  termed the "Conference Control Leg."  This leg has several useful
   properties.  The lifetime of the conference is the same as that of
```

**Corrected text:**

```
   When the conference is created by sending an INVITE containing a
   MSCML <configure_conference> payload, the resulting SIP dialog is
|  termed the "Conference Control Leg".  This leg has several useful
   properties.  The lifetime of the conference is the same as that of
```

**Notes:**

```
Syntax error in second paragraph on page 11.
(Period is not part of the term.)
Please apply 'rational quotation' !
 --VERIFIER NOTES-- 
   This (period inside the quote marks) is a common typographic convention, especially among LaTeX users.
```

---

## Erratum 1250 — Editorial, Rejected

Section 8, p.49-50; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
MIME Type               or:              MIME type

(multiple instances each)
```

**Corrected text:**

```
Media Type               or:             media type
```

**Notes:**

```
a)
The RFC should apply IETF standard terminology.
In this case, it is clear that the RFC does *not* apply to the
context of email, and hence "MIME" should be avoided even more!
Please use the terminology established in RFC 2045 ff. !

b)
Furthermore, Table 3 on page 50 contains a couple of questionable
entries:
  audio/x-alaw-basic      is a private media type;
  audio/ms-gsm            looks like an IETF tree media subtype,
                          but it has not been registered with the IANA;
  audio/x-wav             is also a private media type; I guess it should
                          better be  audio/vnd.wave  (RFC 2361)
                          -- although the IANA apparently has lost
                          records of this media type registration.
 --VERIFIER NOTES-- 
   Authors comment: In four years of deployment, no one has run into a real problem in the real world. This is not a standards track RFC.
```

---

## Erratum 1252 — Editorial, Rejected

Section 9.1; reported by Alfred Hoenes on 2008-01-04.

**Original text:**

```
a)
     URL


b)
     ://

c)

          ... it MUST match remote terminal's identifier, ...
```

**Corrected text:**

```
a)
     URI


b)
                     (nothing!)

c)

          ... it MUST match the remote terminal's identifier, ...
```

**Notes:**

```
Again, this section should apply strict IETF established terminology,
as per STD 66, RFC 3986; it contains a wealth of badly formed said
[URI] schemes.

Throughout the section,
a) replace   "URL"  by   "URI" , and
b) use the precise forms of the URI schemes mentioned,
   i.e.   "file"  ,  "http"  ,  "https"  .

c) In Table 4 on page 52, insert the missing article in the
   5th line from the bottom.
 --VERIFIER NOTES-- 
   Authors comment: In four years of deployment, no one has run into a real problem in the real world.
```

