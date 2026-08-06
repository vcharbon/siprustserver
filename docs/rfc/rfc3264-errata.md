# Errata for RFC 3264

Applies to [rfc3264.txt](rfc3264.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3264>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |
| Held for Document Update | 2 |
| Rejected | 2 |

---

## Erratum 3818 — Technical, Verified

Section 4; reported by Jörgen Axell on 2013-12-02.

**Original text:**

```
   The agent receiving the offer MAY generate an answer, or it MAY
   reject the offer.  The means for rejecting an offer are dependent on
   the higher layer protocol.  The offer/answer exchange is atomic; if
   the answer is rejected, the session reverts to the state prior to the
   offer (which may be absence of a session).
```

**Corrected text:**

```
   The agent receiving the offer MAY generate an answer, or it MAY
   reject the offer.  The means for rejecting an offer are dependent on
   the higher layer protocol.  The offer/answer exchange is atomic; if
   the offer is rejected, the session reverts to the state prior to the
   offer (which may be absence of a session).
```

**Notes:**

```
You can't reject an answer. When the answer is received the SDP negotiation is completed.
```

---

## Erratum 2098 — Technical, Held for Document Update

Section 10; reported by Parveen Verma on 2010-03-30.

**Original text:**

```
10.1 Basic Exchange

   Assume that the caller, Alice, has included the following description
   in her offer.  It includes a bidirectional audio stream and two
   bidirectional video streams, using H.261 (payload type 31) and MPEG
   (payload type 32).  The offered SDP is:

   v=0
   o=alice 2890844526 2890844526 IN IP4 host.anywhere.com
   s=
   c=IN IP4 host.anywhere.com
   t=0 0
   m=audio 49170 RTP/AVP 0
   a=rtpmap:0 PCMU/8000
   m=video 51372 RTP/AVP 31
   a=rtpmap:31 H261/90000
   m=video 53000 RTP/AVP 32
   a=rtpmap:32 MPV/90000
```

**Corrected text:**

```
10.1 Basic Exchange

   Assume that the caller, Alice, has included the following description
   in her offer.  It includes a bidirectional audio stream and two
   bidirectional video streams, using H.261 (payload type 31) and MPEG
   (payload type 32).  The offered SDP is:

   v=0
   o=alice 2890844526 2890844526 IN IP4 host.anywhere.com
   s=-
   c=IN IP4 host.anywhere.com
   t=0 0
   m=audio 49170 RTP/AVP 0
   a=rtpmap:0 PCMU/8000
   m=video 51372 RTP/AVP 31
   a=rtpmap:31 H261/90000
   m=video 53000 RTP/AVP 32
   a=rtpmap:32 MPV/90000
```

**Notes:**

```
The s= session name must have some values as per RFC 4566 Sec 5.3.
----------------------------------------------------------------
RFC 4566
5.3.  Session Name ("s=")

      s=<session name>

   The "s=" field is the textual session name.  There MUST be one and
   only one "s=" field per session description.  The "s=" field MUST NOT
   be empty and SHOULD contain ISO 10646 characters (but see also the
   "a=charset" attribute).  If a session has no meaningful name, the
   value "s= " SHOULD be used (i.e., a single space as the session  name).
----------------------------------------------------------------
RFC 3264 also states in Section 5
"Unfortunately, SDP does not allow the "s=" line to be empty."

Even if we put "s= " , it becomes a bit difficult to read/understand in soft/printed copy.

The same error applies to all SDP examples given in Section 10
```

---

## Erratum 2099 — Technical, Held for Document Update

Section 9; reported by Brett Tate on 2010-03-30.

**Original text:**

```
   v=0
   o=carol 28908764872 28908764872 IN IP4 100.3.6.6
   s=-
   t=0 0
   c=IN IP4 192.0.2.4
   m=audio 0 RTP/AVP 0 1 3
   a=rtpmap:0 PCMU/8000
   a=rtpmap:1 1016/8000
   a=rtpmap:3 GSM/8000
   m=video 0 RTP/AVP 31 34
   a=rtpmap:31 H261/90000
   a=rtpmap:34 H263/90000

   Figure 1: SDP Indicating Capabilities

```

**Corrected text:**

```
   v=0
   o=carol 28908764872 28908764872 IN IP4 100.3.6.6
   s=-
   c=IN IP4 192.0.2.4
   t=0 0
   m=audio 0 RTP/AVP 0 1 3
   a=rtpmap:0 PCMU/8000
   a=rtpmap:1 1016/8000
   a=rtpmap:3 GSM/8000
   m=video 0 RTP/AVP 31 34
   a=rtpmap:31 H261/90000
   a=rtpmap:34 H263/90000

   Figure 1: SDP Indicating Capabilities

```

**Notes:**

```
Location of "c=" line is invalid per RFC 2327 and RFC 4566.  The corrected text reflects the "c=" line within a valid location.
```

---

## Erratum 5177 — Technical, Rejected

Section 5.1; reported by Sandeep Kumar Aitha on 2017-11-03.

**Original text:**

```
Section 5.1 says:
However, for sendonly and sendrecv streams, the answer might indicate
different payload type numbers for the same codecs, in which case,
the offerer MUST send with the payload type numbers from the answer.

Section 6.2 says:
In the case of RTP, if a particular codec was referenced with a
specific payload type number in the offer, that same payload type
number SHOULD be used for that codec in the answer.
```

**Corrected text:**

```
Only one of the above statements can be correct.
```

**Notes:**

```
Above two statements are conflicting.
The answerer should be able to either map the payload type to a different codec or not.
 --VERIFIER NOTES-- 
   See https://mailarchive.ietf.org/arch/msg/mmusic/HRD7ISwLIwiHOA73mc2KW680xAg/
```

---

## Erratum 7597 — Technical, Rejected

Section 6.1; reported by Hugo Tunius on 2023-08-11.

**Original text:**

```
   The interpretation of fmtp parameters in an offer depends on the
   parameters.  In many cases, those parameters describe specific
   configurations of the media format, and should therefore be processed
   as the media format value itself would be.  This means that the same
   fmtp parameters with the same values MUST be present in the answer if
   the media format they describe is present in the answer.  Other fmtp
   parameters are more like parameters, for which it is perfectly
   acceptable for each agent to use different values.  In that case, the
   answer MAY contain fmtp parameters, and those MAY have the same
   values as those in the offer, or they MAY be different.  SDP
   extensions that define new parameters SHOULD specify the proper
   interpretation in offer/answer.
```

**Corrected text:**

```
   The interpretation of fmtp parameters in an offer depends on the
   parameters.  In many cases, those parameters describe specific
   configurations of the media format, and should therefore be processed
   as the media format value itself would be.  This means that the same
   fmtp parameters with the same values MUST be present in the answer if
   the media format they describe is present in the offer.  Other fmtp
   parameters are more like parameters, for which it is perfectly
   acceptable for each agent to use different values.  In that case, the
   answer MAY contain fmtp parameters, and those MAY have the same
   values as those in the offer, or they MAY be different.  SDP
   extensions that define new parameters SHOULD specify the proper
   interpretation in offer/answer.
```

**Notes:**

```
This indicated that the same fmtp parameters with the same values MUST be present in the answer if the media format they describe is present in the **answer**.

I believe the second instance of **answer** should be replace with **offer**, otherwise the quoted text does not makes sense I think.
 --VERIFIER NOTES-- 
See https://mailarchive.ietf.org/arch/msg/mmusic/jDXva6XmGQ4Z6_PUW_TxhZp0-Hc/
```

