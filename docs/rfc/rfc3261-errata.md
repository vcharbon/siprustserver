# Errata for RFC 3261

Applies to [rfc3261.txt](rfc3261.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3261>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 12 |
| Held for Document Update | 16 |
| Rejected | 6 |

---

## Erratum 316 — Technical, Verified

In Section 25.1; reported by RFC Editor on 2003-10-03.

**Original text:**

```
     digest-uri-value  =  rquest-uri ; Equal to request-uri as
                          specified by HTTP/1.1 
```

**Corrected text:**

```
     digest-uri-value  =  request-uri ; Equal to request-uri as
                          specified by HTTP/1.1
```

---

## Erratum 1051 — Technical, Verified

Section 25.1; reported by Alexandre Machado on 2007-08-23.

**Original text:**

```
    srvr           =  [ [ userinfo "@" ] hostport ]
```

**Corrected text:**

```
    srvr           =  [ [ userinfo ] hostport ]
```

**Notes:**

```
The character "@" should not appear in this rule since it already appears at the end of the rule "userinfo":

userinfo         =  ( user / telephone-subscriber ) [ ":" password ] "@"

According to the use of rule "userinfo" in other rules such as "SIP-URI" and "SIPS-URI", the correct place of character "@" is really at the end of rule "userinfo" and not in all other rules using it.
```

---

## Erratum 1073 — Technical, Verified

Section 8.1.3.4; reported by Marco Ambu on 2005-12-15.

**Original text:**

```
sip:user@host?Subject=foo&Call-Info=<http://www.foo.com>
```

**Corrected text:**

```
sip:user@host?Subject=foo&Call-Info=%3Chttp://www.foo.com%3E 
```

**Notes:**

```
[from pending]
I would like to show you an error in RFC 3261. I've discussed in
SIP-implementors mailing list too.

in RFC 3261 page 44 par 8.1.3.4 there is an example of URI (contact URI)
with uri headers:

sip:user@host?Subject=foo&Call-Info=<http://www.foo.com>

I've noticed that URI header "Call-Info" has a value with characters not
allowed in uri headers.

The BNF says that an header value must contain characters in   
hnv-unreserved or unreserved or escaped but '<' and '>' are not in that set.

The right example should be the following:
sip:user@host?Subject=foo&Call-Info=%3Chttp://www.foo.com%3E
```

---

## Erratum 1470 — Technical, Verified

Section 17.2.2; reported by Iñaki Baz Castillo on 2008-07-14.

**Original text:**

```
If the TU passes a final response (status codes 200-699) to the 
server while in the "Proceeding" state, the transaction MUST enter 
the "Completed" state...
```

**Corrected text:**

```
If the TU passes a final response (status codes 200-699) to the 
server while in the "Trying" or "Proceeding" state, the transaction 
MUST enter the "Completed" state...
```

**Notes:**

```
"17.2.2 Non-INVITE Server Transaction" doesn't consider the case in which the transaction state is "Trying" and the transaction receives from the TU a final response.
It's totally possible that TU sends a final response without sending before a provisional response. Note that this case is perfectly valid in the diagram of page 139.
```

---

## Erratum 3183 — Technical, Verified

Section 23.4.3; reported by Bruno CHATRAS on 2012-04-10.

**Original text:**

```
         --boundary42
         Content-Type: application/pkcs7-mime; smime-type=enveloped-data;
              name=smime.p7m
         Content-Transfer-Encoding: base64
         Content-Disposition: attachment; filename=smime.p7m
            handling=required
         Content-Length: 231

```

**Corrected text:**

```
   --boundary42
         Content-Type: application/pkcs7-mime; smime-type=enveloped-data;
              name=smime.p7m
         Content-Transfer-Encoding: base64
         Content-Disposition: attachment; filename=smime.p7m
            handling=required

```

**Notes:**

```
A Content-Length header is shown for a body-part within a multipart body. But Content-Length is an HTTP/SIP header, not a IANA-registered MIME header and should therefore not appear at that location in valid examples. The length of a body part within a multipart body is determined by MIME framing. A Content-Length header found for a body-part within a multipart body is meaningless and should be ignored.

This was discussed on both the SIP Implementors and SIP Core mailing lists.
```

---

## Erratum 4567 — Technical, Verified

Section 20.11; reported by Christer Holmberg on 2015-12-17.

**Original text:**

```
The handling parameter, handling-param, describes how the UAS should
react if it receives a message body whose content type or disposition
type it does not understand. The parameter has defined values of
"optional" and "required". If the handling parameter is missing, the
value "required" SHOULD be assumed. The handling parameter is
described in RFC 3204 [19].
```

**Corrected text:**

```
The handling parameter, handling-param, describes how the UAS should
react if it receives a message body whose content type or disposition
type it does not understand. The parameter has defined values of
"optional" and "required". If the handling parameter is missing, or if
the Content-Disposition header field is missing, the value "required" 
SHOULD be assumed. The handling parameter is described in RFC 3204 [19].
```

---

## Erratum 6645 — Technical, Verified

Section 23.3; reported by Matt Hertogs on 2021-07-20.

**Original text:**

```
Content-Disposition: attachment; filename=smime.p7m
           handling=required
```

**Corrected text:**

```
Content-Disposition: attachment; filename=smime.p7m;
           handling=required
```

**Notes:**

```
The example for Content-Disposition header is incorrectly formatted according to the BNF.
It is missing a semicolon between each disp-param.
(Relevant section of BNF posted below)

Content-Disposition   =  "Content-Disposition" HCOLON
                         disp-type *( SEMI disp-param )
disp-type             =  "render" / "session" / "icon" / "alert"
                         / disp-extension-token
disp-param            =  handling-param / generic-param
handling-param        =  "handling" EQUAL
                         ( "optional" / "required"
                         / other-handling )
other-handling        =  token
disp-extension-token  =  token

This also occurs in 23.4.3
```

---

## Erratum 7529 — Technical, Verified

Section 25.1; reported by Shraddha Soni on 2023-05-29.

**Original text:**

```
This is intended to behave exactly as HTTP/1.1 as described in RFC 2616 [8].
The SWS construct is used when linear white space is optional, generally between tokens and separators.

      LWS  =  [*WSP CRLF] 1*WSP ; linear whitespace
```

**Corrected text:**

```
This is intended to behave exactly as HTTP/1.1 as described in RFC 2616 [8].
The SWS construct is used when linear white space is optional, generally between tokens and separators.

      LWS  =  [CRLF] 1*WSP ; linear whitespace
```

**Notes:**

```
RFC 2616 states 
LWS            = [CRLF] 1*( SP | HT )

RFC 2234 states 
WSP            =  SP / HTAB
                               ; white space
RFC 3261 is referencing both for BNF to follow, but looks like a new BNF is stated. So either it should not reference to follow RFC 2616 exactly or follow the same BNF as 2616.
```

---

## Erratum 2051 — Editorial, Verified

Section 26.3.1; reported by Cullen Jennings on 2010-02-24.

**Original text:**

```
   Proxy servers, redirect
   servers, and registrars SHOULD possess a site certificate whose
   subject corresponds to their canonical hostname. 
```

**Corrected text:**

```
   Proxy servers, redirect
   servers, and registrars SHOULD possess a site certificate whose
   subject corresponds to the DNS name other SIP devices will use to reach them. 
```

**Notes:**

```
The term hostname seemed to make some people think if you had two sip servers for the domain example.com with hostnames host1 and host2, the the cert should have host1.example.com when in fact the sip signaling always used exmaple.com and the cert should have a example.com as the name.
```

---

## Erratum 4084 — Editorial, Verified

Section 13.2.2.4; reported by Xing Lou Han on 2014-08-15.

**Original text:**

```
   If the dialog identifier in the 2xx response matches the dialog
   identifier of an existing dialog, the dialog MUST be transitioned to
   the "confirmed" state, and the route set for the dialog MUST be
   recomputed based on the 2xx response using the procedures of Section
   12.2.1.2.
```

**Corrected text:**

```
   If the dialog identifier in the 2xx response matches the dialog
   identifier of an existing dialog, the dialog MUST be transitioned to
   the "confirmed" state, and the route set for the dialog MUST be
   recomputed based on the 2xx response using the procedures of Section
   12.2.1.1.
```

**Notes:**

```
The procedures of recomputing the route set should refer to the Section 12.2.1.1, rather than Section 12.2.1.2. 

Actually in Section 12.2.1.2, there is no procedure of computing route set. Instead, the related procedures can only be found in Section 12.2.1.1.

To avoid misleading, "12.2.1.2" here should be "12.2.1.1" instead.
```

---

## Erratum 4300 — Editorial, Verified

Section 24.2; reported by Tamas Horvath on 2015-03-12.

**Original text:**

```
F2 100 Trying atlanta.com proxy -> Alice
(...)
F3 INVITE atlanta.com proxy -> biloxi.com proxy
(...)
F4 100 Trying biloxi.com proxy -> atlanta.com proxy
(...)
F5 INVITE biloxi.com proxy -> Bob
```

**Corrected text:**

```
F3 100 Trying atlanta.com proxy -> Alice
(...)
F2 INVITE atlanta.com proxy -> biloxi.com proxy
(...)
F5 100 Trying biloxi.com proxy -> atlanta.com proxy
(...)
F4 INVITE biloxi.com proxy -> Bob
```

**Notes:**

```
Figure 1 in Section 4 shows different indexes for the 100 Trying and INVITE messages.
```

---

## Erratum 4637 — Editorial, Verified

Section 23.4.3; reported by Boris Shtrasman on 2016-03-11.

**Original text:**

```
        ghyHhHUujhJhjH77n8HHGTrfvbnj756tbB9HG4VQpfyF467GhIGfHfYT6
        4VQpfyF467GhIGfHfYT6jH77n8HHGghyHhHUujhJh756tbB9HGTrfvbnj
        n8HHGTrfvhJhjH776tbB9HG4VQbnj7567GhIGfHfYT6ghyHhHUujpfyF4
        7GhIGfHfYT64VQbnj756

        --boundary42-
```

**Corrected text:**

```
        ghyHhHUujhJhjH77n8HHGTrfvbnj756tbB9HG4VQpfyF467GhIGfHfYT6
        4VQpfyF467GhIGfHfYT6jH77n8HHGghyHhHUujhJh756tbB9HGTrfvbnj
        n8HHGTrfvhJhjH776tbB9HG4VQbnj7567GhIGfHfYT6ghyHhHUujpfyF4
        7GhIGfHfYT64VQbnj756

        --boundary42--
```

**Notes:**

```
As far as I know a boundary end should end with two dashes as for RFC 3851 for that specific case, hence it should be :

 --boundary42--
```

---

## Erratum 678 — Technical, Held for Document Update

Section 25.1; reported by Matthew S. Harris on 2006-08-14.

**Original text:**

```
On page 220, it says "The TEXT-UTF8-TRIM rule is used for descriptive
field contents that are n t quoted strings, where leading and trailing
LWS is not meaningful."  (And no, I'm not talking about the "n t"
typo.)  The rule for TEXT-UTF8-TRIM is

 TEXT-UTF8-TRIM  = 1*TEXT-UTF8char *(*LWS TEXT-UTF8char)
 TEXT-UTF8char = %x21-7E / UTF8-NONASCII

which pretty clearly says that the final character of TEXT-UTF8-TRIM
cannot be whitespace.  TEXT-UTF8-TRIM appears only as the value of a
header field, and the rule for message-header does not generate
trailing whitespace either.

So the text talks about trailing whitespace, which seems reasonable
because whitespace is allowed in many other places, but the grammar
does not allow it.  Was trailing whitespace intended?
```

**Corrected text:**

```
[not submitted]
```

**Notes:**

```
from pending
```

---

## Erratum 832 — Technical, Held for Document Update

Section (unspecified); reported by Marco Ambu on 2006-01-11.

**Original text:**

```
I would like to show you an ambiguity in RFC 3261.

The ABNF for SIP in RFC 3261 page 227 defines header Accept as 
"Accept = "Accept" HCOLON [accept-range *(COMMA accept-range)].

Expanding this form we have:

Accept = "Accept" HCOLON [( (...) *(SEMI m-parameter) *(SEMI 
accept-param) ) *(COMMA accept-range)]

For example we can have

Accept: 
application/sdp;m_extension_parameter=value1;accept_extension_param=value2;q=0.5
We know from RFC 3261 that q is an accept-param.
We don't know how to consider the first two unknown parameters: how to 
distinguish from m-parameter and accept-param?

While in other cases RFC 3261 shows the rules to solve ambiguities (for 
example how to consider the parameters in a Contact URI if the URI is 
not enclosed in angular brackets) I have not found any suggestion for 
this specific case in RFC 3261.
```

**Corrected text:**

```
[not submitted]
```

**Notes:**

```
from pending
```

---

## Erratum 1682 — Technical, Held for Document Update

Section 20.7; reported by Iñaki Baz Castillo on 2009-02-10.

**Original text:**

```
Authorization: Digest username="Alice", realm="atlanta.com",
  nonce="84a4cc6f3082121f32b42a2187831a9e",
  response="7587245234b3434cc3412213e5f113a5432"
```

**Corrected text:**

```
Authorization: Digest username="Alice", realm="atlanta.com",
  nonce="84a4cc6f3082121f32b42a2187831a9e",
  response="7587245234b3434cc3412213e5f113a5"
```

**Notes:**

```
'response' field in original example has 35 hexadecimal characters while they must be 32:

  dresponse         =  "response" EQUAL request-digest
  request-digest    =  LDQUOT 32LHEX RDQUOT
```

---

## Erratum 2769 — Technical, Held for Document Update

Section 17.1.2.2; reported by Iñaki Baz Castillo on 2011-04-08.

**Original text:**

```
   MUST be passed to the transport layer for transmission.  If an
   unreliable transport is in use, the client transaction MUST set timer
   E to fire in T1 seconds.  If timer E fires while still in this state,
   the timer is reset, but this time with a value of MIN(2*T1, T2).
   When the timer fires again, it is reset to a MIN(4*T1, T2).  This
   process continues so that retransmissions occur with an exponentially
   increasing interval that caps at T2.
```

**Corrected text:**

```
   MUST be passed to the transport layer for transmission.  If an
   unreliable transport is in use, the client transaction MUST set timer
   E to fire in T1 seconds.  If timer E fires while still in this state,
   the timer is reset, but this time with a value of MIN(2*T1, T2).
   When the timer fires again, it is reset to a MIN(4*T1, T2). Multiplier
   on T1 doubles with each reset so that retransmissions occur with an
   exponentially increasing interval that caps at T2.
```

**Notes:**

```
The original text doesn't clarify that the multiplier of T1 is doubled with each timer reset, so it could be understood that the maximum value the timer takes is MIN(4*T1, T2) which is 2s (so the timer would never reach 4s and the resulting intervals would be 500ms, 1s, 2s, 2s, 2s, 2s, etc).
```

---

## Erratum 3098 — Technical, Held for Document Update

Section 12.2.1.1; reported by Alec Davis on 2012-01-27.

**Original text:**

```
A client could, for example, choose the 31 most significant bits of a 32-bit
 second clock as an initial sequence number.

```

**Corrected text:**

```
A client could, for example, choose the 31 least significant bits of a 32-bit
 second clock as an initial sequence number.

```

**Notes:**

```
Choosing the most sig 31 bits would violate 8.1.1.5, where initial CSeq number must be less than 2**31

8.1.1.5: The sequence number value MUST be expressible as a 32-bit unsigned integer and MUST be less than 2**31.


REVIEWER NOTE (rjsparks@nostrum.com) : It was explicitly the intent to point to the most significant bits. The confusion in this report is not recognizing that the intent is to treat those as a 31-bit integer (which by definition will be less than 2**31). Rather than reject the errata, I'm putting this in hold for document update so future revisions can clarify the point.
```

---

## Erratum 3102 — Technical, Held for Document Update

Section 12.2.1.1; reported by Alec Davis on 2012-02-01.

**Original text:**

```
      With a length of 32 bits, a client could generate, within a single
      call, one request a second for about 136 years before needing to
      wrap around.  The initial value of the sequence number is chosen
      so that subsequent requests within the same call will not wrap
      around.
```

**Corrected text:**

```
      With a length of 32 bits, a client could generate, within a single
      call, one request a second for about 136 years before exhausting
      available sequence numbers. Sequence numbers must not wrap around to 0.
      The initial value of the sequence number (less than 2**31) is chosen
      so that subsequent requests within the same call will not exceed 2**32-1.
      
```

**Notes:**

```
Highlight that within a dialog sequence numbers;
   1). can increment to 2**32-1
   2). must not wrap around to 0
```

---

## Erratum 3684 — Technical, Held for Document Update

Section 7.3; reported by Colin Fraizer on 2013-07-16.

**Original text:**

```
 Specifically, any SIP header whose grammar is of the form

      header  =  "header-name" HCOLON header-value *(COMMA header-value)

   allows for combining header fields of the same name into a comma-
   separated list.  The Contact header field allows a comma-separated
   list unless the header field value is "*".
```

**Corrected text:**

```
 Specifically, any SIP header whose grammar is of the form

      header  =  header-name HCOLON header-value *(COMMA header-value)

   allows for combining header fields of the same name into a comma-
   separated list.  The Contact header field allows a comma-separated
   list unless the header field value is "*".
```

**Notes:**

```
That is, remove the double quotes around the word `header-name' in the ABNF.
```

---

## Erratum 3875 — Technical, Held for Document Update

Section 17; reported by Varun Bharadwaj S V on 2014-01-31.

**Original text:**

```
17 Transcations

 - Additionally, in the case of an INVITE request, the client
transaction is responsible for generating the ACK request for any
final response accepting a 2xx response.
```

**Corrected text:**

```
17 Transcations

 - Additionally, in the case of an INVITE request, the client
transaction is responsible for generating the ACK request for any
final response excepting a 2xx response.
```

**Notes:**

```
Client transaction should generate the ACK except for 2xx responses.It should not accept & generate the ACK for 2xx response.
For Server transaction RFC says - "In the case of an INVITE transaction, it absorbs the ACK request for any final response excepting a 2xx response."
```

---

## Erratum 5598 — Technical, Held for Document Update

Section 25.1; reported by Roman Shpount on 2019-01-11.

**Original text:**

```
display-name   =  *(token LWS)/ quoted-string
```

**Corrected text:**

```
display-name   =  token *(LWS token)/ quoted-string
```

**Notes:**

```
There is a contradiction between the ABNF rule and specification.

The name-addr ABNF rule in RFC 3261 specifies:

name-addr      =  [ display-name ] LAQUOT addr-spec RAQUOT
addr-spec      =  SIP-URI / SIPS-URI / absoluteURI
display-name   =  *(token LWS)/ quoted-string

Based on this, LWS is always required between token and the "<" and the following Name-Address value is invalid: foo<sip:foo@bar.com>

At the same time section 20.10 says:

There may or may not be LWS between the display-name and the "<".

This implies that foo<sip:foo@bar.com> should be acceptable.

I propose to change ABNF rule for display-name to allow no LWS between last token and the "<"
```

---

## Erratum 5653 — Technical, Held for Document Update

Section 17.1.1.2; reported by Vimal Chandra Tewari on 2019-03-12.

**Original text:** *(none given)*

**Corrected text:** *(none given)*

**Notes:**

```
When a INVITE client transaction transitions to Proceeding state (upon receiving a provisional response), the request retransmission stops (for an unreliable transport) i.e. Timer A is stopped. 
However in Proceeding state, Timer B, i.e. Transaction Timeout Timer can still fire if no final response is received in stipulated time in which case the TU should be informed and the transaction should transition to Terminated state.
"Figure 5: INVITE client transaction" in RFC 3261 does not show the Timer B expiry event in Proceeding state. This means that we are saying there is a guarantee that a final response will always be received in Proceeding state which may not always be the case.
In my opinion, the Proceeding state in Figure 5 should be updated to include a Timer B event.
```

---

## Erratum 6793 — Technical, Held for Document Update

Section 8.1.1.5; reported by Mojtaba Esfandiari.S on 2021-12-21.

**Original text:**

```
The CSeq header field serves as a way to identify and order
   transactions.  It consists of a sequence number and a method.  The
   method MUST match that of the request.  For non-REGISTER requests
   outside of a dialog, the sequence number value is arbitrary.  The
   sequence number value MUST be expressible as a 32-bit unsigned
   integer and MUST be less than 2**31.  As long as it follows the above
   guidelines, a client may use any mechanism it would like to select
   CSeq header field values.
```

**Corrected text:**

```
The CSeq header field serves as a way to identify and order
   transactions.  It consists of a sequence number and a method.  The
   method MUST match that of the request.  For non-REGISTER requests
   outside of a dialog, the sequence number value is arbitrary. For requests with its credentials after receiving a
   401 (Unauthorized) or 407 (Proxy Authentication Required) response,
  the CSeq header field value MUST increment. The
   sequence number value MUST be expressible as a 32-bit unsigned
   integer and MUST be less than 2**31.  As long as it follows the above
   guidelines, a client may use any mechanism it would like to select
   CSeq header field values.
```

**Notes:**

```
For more clarity and more understanding, It would be nice to have a quiet bit of update in RFC 3261 section 8.1.1.5. Because, the word "arbitrary" in the CSeq value may be confused with the same value as before.
```

---

## Erratum 1471 — Editorial, Held for Document Update

Section 17; reported by Iñaki Baz Castillo on 2008-07-14.

**Original text:**

```
Additionally, in the case of an INVITE request, the client
transaction is responsible for generating the ACK request for any
final response accepting a 2xx response.
```

**Corrected text:**

```
Additionally, in the case of an INVITE request, the client
transaction is responsible for generating the ACK request for any
final response excepting a 2xx response.
```

**Notes:**

```
"accepting a 2xx response" => "excepting a 2xx response"
```

---

## Erratum 1472 — Editorial, Held for Document Update

Section 25.1; reported by Iñaki Baz Castillo on 2008-07-14.

**Original text:**

```
The TEXT-UTF8-TRIM rule is used for descriptive field 
contents that are n t quoted strings,
```

**Corrected text:**

```
The TEXT-UTF8-TRIM rule is used for descriptive field 
contents that are not quoted strings,
```

**Notes:**

```
"are n t" => "are not"
```

---

## Erratum 3237 — Editorial, Held for Document Update

Section 8.1.3.5; reported by Kevin P. Fleming on 2012-05-31.

**Original text:**

```
   In all of the above cases, the request is retried by creating a new
   request with the appropriate modifications.  This new request
   constitutes a new transaction and SHOULD have the same value of the
   Call-ID, To, and From of the previous request, but the CSeq should
   contain a new sequence number that is one higher than the previous.
```

**Corrected text:**

```
   In all of the above cases, the request is retried by creating a new 
   request with the appropriate modifications.  This new request 
   constitutes a new transaction and SHOULD have the same value of the 
   Call-ID, To, and From of the previous request, but the CSeq SHOULD 
   contain a new sequence number that is one higher than the previous.
```

**Notes:**

```
We have had one implementor claim that they are not required to increment CSeq when retrying the request because the RFC says 'should' and not 'SHOULD'. Based on current IETF discussions, though, these should probably be changed to MUST anyway, but that's a much more substantive change throughout the whole RFC.
```

---

## Erratum 4385 — Editorial, Held for Document Update

Section 17.2.1; reported by Krisztian Sinka on 2015-06-02.

**Original text:**

```
                               |INVITE
                               |pass INV to TU
            INVITE             V send 100 if TU won't in 200ms
            send response+-----------+
                +--------|           |--------+101-199 from TU
                |        | Proceeding|        |send response
                +------->|           |<-------+
                         |           |          Transport Err.
                         |           |          Inform TU
                         |           |--------------->+
                         +-----------+                |
```

**Corrected text:**

```
                               |INVITE
                               |-
            INVITE             V send 100 if TU won't in 200ms
            send response+-----------+
                +--------|           |--------+101-199 from TU
                |        | Proceeding|        |send response
                +------->|           |<-------+
                         |           |          Transport Err.
                         |           |          Inform TU
                         |           |--------------->+
                         +-----------+                |
```

**Notes:**

```
The standard states about lifetime of the server transaction:

"Server transactions are created by the
core when a request is received, and transaction handling is desired
for that request (this is not always the case)."(17.2)

So this means that the server transaction is created by the TU (the core). This also means that it is useless to *pass INV to TU* due to that TU itself already has the request.

The same logic should apply to Figure 8. as well.
```

---

## Erratum 4675 — Editorial, Held for Document Update

Section 8.1.1.3; reported by Bruce Florman on 2016-04-21.

**Original text:**

```
   Examples:

      From: "Bob" <sips:bob@biloxi.com> ;tag=a48s
      From: sip:+12125551212@phone2net.com;tag=887s
      From: Anonymous <sip:c8oqz84zk7z@privacy.org>;tag=hyh8
```

**Corrected text:**

```
   Examples:

      From: "Bob" <sips:bob@biloxi.com> ;tag=a48smh2
      From: sip:+12125551212@phone2net.com;tag=887s6pa
      From: Anonymous <sip:c8oqz84zk7z@privacy.org>;tag=hyh8mtf
```

**Notes:**

```
I know this is more than a little picayune, but...

Section 19.3 says:

   When a tag is generated by a UA for insertion into a request or
   response, it MUST be globally unique and cryptographically random
   with at least 32 bits of randomness.

This implies that the tag value must be at least 32 bits in size.

Section 25.1 says:

      token       =  1*(alphanum / "-" / "." / "!" / "%" / "*"
                     / "_" / "+" / "`" / "'" / "~" )
and
      tag-param   =  "tag" EQUAL token

Although the actual representation of the tag value is implementation-specific, since there are only 72 characters available with which to encode it, a tag value with only four characters can represent a maximum of 72^4 distinct numeric values, which is less than the 2^32 values that can be represented by 32 bits by a factor of nearly 160.

Even if the implementation chooses to omit "leading zeros" (or something equivalent), the likelihood of three 32 bit random values all falling in the range of values that can be represented in four characters would be less than one in five million. So even if the given examples are theoretically possible for a conforming implementation, they seem rather misleading.

For a "typical" 32 bit random value, even with base74 encoding, the shortest tag value would require six characters. Given that all three examples (which are also repeated almost unchanged in section 20.20) contain no uppercase ALPHA characters in the tag values, the implied encoding is more likely something like base32 or base36, which would require at least seven characters to represent a typical 32 bit random value. So that's how I'm suggesting that the examples be amended.
```

---

## Erratum 1742 — Technical, Rejected

Section 17.1.1.2; reported by Pankaj Jain on 2009-03-25.

**Original text:**

```
Timer D reflects the amount of time that the server transaction can
   remain in the "Completed" state when unreliable transports are used.
```

**Corrected text:**

```
Timer D reflects the amount of time that the client transaction can
   remain in the "Completed" state when unreliable transports are used.
```

**Notes:**

```
server transaction => client transaction
 --VERIFIER NOTES-- 
   The original text is correct. The value (and reason) for timer D is to reflect what's happening in the server transaction. See the discussion of Timer H that immediately follows the text quoted above.
```

---

## Erratum 2910 — Technical, Rejected

Section Table 2; reported by Iñaki Baz Castillo on 2011-08-02.

**Original text:**

```
      Header field          where   proxy ACK BYE CAN INV OPT REG
      ___________________________________________________________
      Contact                1xx           -   -   -   o   -   -
```

**Corrected text:**

```
      Header field          where   proxy ACK BYE CAN INV OPT REG
      ___________________________________________________________
      Contact                1xx           -   -   -   m   -   -
```

**Notes:**

```
RFC 3261 says:

Section 12.1: "Dialogs are created through the generation of non-failure responses to requests with specific methods.  Within this specification, only 2xx and 101-199 responses with a To tag, where the request was INVITE, will establish a dialog."

Section 12.1.1: "When a UAS responds to a request with a response that establishes a dialog (such as a 2xx to INVITE), the UAS MUST copy all Record-Route header field values from the request into the response [...].  The UAS MUST add a Contact header field to the response."

So it's clear that a 1xx response to an INVITE creates a dialog and then it MUST contain a Contact header and mirrored Record-Route headers.

However Table 2 (page 162) says:

      Header field          where   proxy ACK BYE CAN INV OPT REG
      ___________________________________________________________
      Contact                1xx           -   -   -   o   -   -
      Record-Route        2xx,18x    mr    -   o   o   o   o   -

Obviously Record-Route is optional since in the absence of a proxy doing record-routing, such header will not be present. However Contact header should appear as mandatory (m) for 1xx responses for INVITE rather than optional (o).
 --VERIFIER NOTES-- 
1xx also includes 100 Trying, which cannot establish a Dialog. Contact is not mandatory in 100 Trying responses.
```

---

## Erratum 4275 — Technical, Rejected

Section 15; reported by Chao Wang on 2015-02-19.

**Original text:**

```
The caller's UA MAY send a BYE for either confirmed or early dialogs
```

**Corrected text:**

```
The caller's UA MUST send a BYE for confirmed dialogs
```

**Notes:**

```
In general, BYE shall be handled in the same way as CANCEL if it is for early dialogs.
In case, when BYE is on the way to the destination, the callee probably accepts INVITE, the race condition will occure.
If we follow the procedure as CANCEL, caller shall send ACK for 200 OK and immediately release the session using BYE.
So two BYEs will be triggered in the case, it does not make sense.
 --VERIFIER NOTES-- 
This behavior was clarified in https://www.rfc-editor.org/rfc/rfc6141#section-5.3
```

---

## Erratum 4553 — Technical, Rejected

Section 20.11; reported by Christer Holmberg on 2015-12-04.

**Original text:**

```
The handling parameter, handling-param, describes how the UAS should
react if it receives a message body whose content type or disposition
type it does not understand.  The parameter has defined values of
"optional" and "required".  If the handling parameter is missing, the
value "required" SHOULD be assumed.  The handling parameter is
described in RFC 3204 [19].
```

**Corrected text:**

```
The handling parameter, handling-param, describes how the UAS should
react if it receives a message body whose content type or disposition
type it does not understand.  The parameter has defined values of
"optional" and "required".  If the handling parameter is missing, or if
the Content-Disposition header field is missing, the value "required" 
MUST be assumed.  The handling parameter is described in RFC 3204 [19].
```

**Notes:**

```
SIPCORE discussion: https://mailarchive.ietf.org/arch/msg/sipcore/bn-pRgDyGL2FP_M7eYpyT35zNp4
 --VERIFIER NOTES-- 
   See https://mailarchive.ietf.org/arch/msg/sipcore/fjlcEaK4zFS3Yelzkqvljl5SxRw/
```

---

## Erratum 5619 — Technical, Rejected

Section 4; reported by Isabella Damböck on 2019-01-31.

**Original text:**

```
The address 
of the atlanta.com SIP server could have been configured in Alice's
softphone, or it could have been discovered by DHCP, for example.
```

**Corrected text:**

```
The address 
of the atlanta.com SIP server could have been configured in Alice's 
softphone, or it could have been discovered by DNS, for example.
```

**Notes:**

```
DHCP Server gives away the DNS-Server to use (or sets it with DHCP option) but usually does no address translation itself. It would also be possible to omit the whole sentence.
 --VERIFIER NOTES-- 
   DHCP was the intent of the example.
```

---

## Erratum 4379 — Editorial, Rejected

Section 25.1; reported by OKUMURA Shinji on 2015-05-28.

**Original text:**

```
Errata ID: 316
digest-uri-value  =  request-uri ; Equal to request-uri
                                   as specified by HTTP/1.1
```

**Corrected text:**

```
digest-uri-value  =  rquest-uri ; Equal to request-uri
                                  as specified by HTTP/1.1
```

**Notes:**

```
Rule names are case-insensitive.
Request-URI has been defined already.
An original definition is correct.
Alternatively, it may refer to request-target as specified by RFC 7230.
 --VERIFIER NOTES-- 
The original text includes: Errata ID: 316

But the current text matches the suggestion:

https://datatracker.ietf.org/doc/html/rfc3261#section-25.1
```

