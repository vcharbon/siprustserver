# Errata for RFC 3262

Applies to [rfc3262.txt](rfc3262.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3262>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 6 |
| Rejected | 2 |

---

## Erratum 315 — Technical, Verified

Section (unspecified); reported by Steve Conner on 2002-07-04.

**Original text:** *(none given)*

**Corrected text:** *(none given)*

**Notes:**

```
This document obsoletes RFC 2543.
```

---

## Erratum 4600 — Technical, Verified

Section 3; reported by Christer Holmberg on 2016-01-25.

**Original text:**

```
The provisional response to be sent reliably is constructed by the
UAS core according to the procedures of Section 8.2.6 of RFC 3261.
In addition, it MUST contain a Require header field containing the
option tag 100rel, and MUST include an RSeq header field.  The value
of the header field for the first reliable provisional response in a
transaction MUST be between 1 and 2**31 - 1.  It is RECOMMENDED that
it be chosen uniformly in this range.  The RSeq numbering space is
within a single transaction.  This means that provisional responses
for different requests MAY use the same values for the RSeq number.
```

**Corrected text:**

```
The provisional response to be sent reliably is constructed by the
UAS core according to the procedures of Section 8.2.6 of RFC 3261.
In addition, it MUST contain a Require header field containing the
option tag 100rel, and MUST include an RSeq header field.  The
value of the header field for the first reliable provisional 
response in a transaction MUST be between 1 and 2**31 - 1.  It is
RECOMMENDED that it be chosen uniformly in this range. The RSeq 
numbering space is within a single transaction. This means that
the RSeq value of a provisional response within a fork of a request
is independent of the RSeq value of a provisional response within 
any other fork of that request, or for the responses for any other
request. It thus may be higher, lower, or the same as any other such
RSeq value.
```

---

## Erratum 4601 — Technical, Verified

Section 4; reported by Christer Holmberg on 2016-01-25.

**Original text:**

```
If a provisional response is received for an initial request, and
that response contains a Require header field containing the option
tag 100rel, the response is to be sent reliably.  If the response is
a 100 (Trying) (as opposed to 101 to 199), this option tag MUST be
ignored, and the procedures below MUST NOT be used.
```

**Corrected text:**

```
If a provisional response is received for an initial request, and
that response contains a Require header field containing the option
tag 100rel, the response was sent by the UAS reliably.  If the
response is a 100 (Trying) (as opposed to 101 to 199), this option
tag MUST be ignored, and the procedures below MUST NOT be used.
```

---

## Erratum 4602 — Technical, Verified

Section 4; reported by Christer Holmberg on 2016-01-25.

**Original text:**

```
Assuming the response is to be transmitted reliably, the UAC MUST
create a new request with method PRACK.  This request is sent within
the dialog associated with the provisional response (indeed, the
provisional response may have created the dialog).  PRACK requests
MAY contain bodies, which are interpreted according to their type and
disposition.
```

**Corrected text:**

```
Assuming the response was transmitted reliably by the UAS, the UAC
MUST create a new request with method PRACK.  This request is sent
within the dialog associated with the provisional response (indeed,
the provisional response may have created the dialog). PRACK 
requests MAY contain bodies, which are interpreted according to
their type and disposition.
```

---

## Erratum 4603 — Technical, Verified

Section 4; reported by Christer Holmberg on 2016-01-25.

**Original text:**

```
Once a reliable provisional response is received, retransmissions of
that response MUST be discarded.  A response is a retransmission when
its dialog ID, CSeq, and RSeq match the original response.  The UAC
MUST maintain a sequence number that indicates the most recently
received in-order reliable provisional response for the initial
request.  This sequence number MUST be maintained until a final
response is received for the initial request.  Its value MUST be
initialized to the RSeq header field in the first reliable
provisional response received for the initial request.
```

**Corrected text:**

```
Once a reliable provisional response is received, retransmissions of
that response MUST be discarded.  A response is a retransmission 
when its dialog ID, CSeq, and RSeq match the original response. The
UAC MUST maintain, independently for each dialog ID, a sequence
number that indicates the most recently received in-order reliable
provisional response for the initial request.  This sequence number
MUST be maintained until a final response is received for the
initial request. Its value MUST, for each dialog (or early dialog),
be initialized to the RSeq header field in the first reliable
provisional response associated with the dialog received for the
initial request.
```

**Notes:**

```
Verifier edit: I removed  two commas around "associated with the dialog" from the last sentence of the corrected text.
```

---

## Erratum 4604 — Technical, Verified

Section 4; reported by Christer Holmberg on 2016-01-25.

**Original text:**

```
Handling of subsequent reliable provisional responses for the same
initial request follows the same rules as above, with the following
difference: reliable provisional responses are guaranteed to be in
order.  As a result, if the UAC receives another reliable provisional
response to the same request, and its RSeq value is not one higher
than the value of the sequence number, that response MUST NOT be
acknowledged with a PRACK, and MUST NOT be processed further by the
UAC.  An implementation MAY discard the response, or MAY cache the
response in the hopes of receiving the missing responses.
```

**Corrected text:**

```
Subsequent reliable provisional responses for the same initial
request are guaranteed  to have been generated by the UAS in the
order of their RSeq values and must be acknowledged in that order.
As a result, if the UAC receives another reliable provisional
response to the same request, and its RSeq value is one higher than
the value of the previously received RSeq value in the dialog (or 
early dialog), then the new RSeq value is saved and the response is
handled as described above. If the RSeq value is not one higher than
the value of the sequence number, that response MUST NOT be
acknowledged with a PRACK, and MUST NOT be processed further by the
UAC. An implementation MAY discard the response, or MAY cache the
response to be processed (and acknowledged) after receiving the
missing responses.
```

---

## Erratum 4288 — Technical, Rejected

Section 4; reported by Jörgen Axell on 2015-03-04.

**Original text:**

```
   Handling of subsequent reliable provisional responses for the same
   initial request follows the same rules as above, with the following
   difference: reliable provisional responses are guaranteed to be in
   order.  As a result, if the UAC receives another reliable provisional
   response to the same request, and its RSeq value is not one higher
   than the value of the sequence number, that response MUST NOT be
   acknowledged with a PRACK, and MUST NOT be processed further by the
   UAC.  An implementation MAY discard the response, or MAY cache the
   response in the hopes of receiving the missing responses.
```

**Corrected text:**

```
   Handling of subsequent reliable provisional responses for the same
   initial request follows the same rules as above, with the following
   difference: reliable provisional responses are guaranteed to be in
   order.  As a result, if the UAC receives another reliable provisional
   response to the same request on a given dialog, and its RSeq value is
   not one higher than the value of the sequence number, that response
   MUST NOT be acknowledged with a PRACK, and MUST NOT be processed
   further by the  UAC.  An implementation MAY discard the response, or
   MAY cache the response in the hopes of receiving the missing
   responses. If forking occurs, RSeq values are processed on each early
   dialog independently. 
```

**Notes:**

```
Each UAS receiving a forked request selects the RSeq values independently. Without this addition of the text the forking proxy would need to change the RSeq values on the received responses to keep them in order.
 --VERIFIER NOTES-- 
   The proposed wording is not correct, since RSeq values are managed per transaction, not per dialog. The "on each dialog" language may mislead users. However, I agree that there is a problem with the original language here, and have asked sipcore to decide how to correct it. Therefore I am rejecting this errata, but we may replace it with a new one if that's what sipcore decides
```

---

## Erratum 4605 — Technical, Rejected

Section 3, 4; reported by Christer Holmberg on 2016-01-05.

**Original text:**

```
Section 3:

   The provisional response to be sent reliably is constructed by the
   UAS core according to the procedures of Section 8.2.6 of RFC 3261.
   In addition, it MUST contain a Require header field containing the
   option tag 100rel, and MUST include an RSeq header field.  The value
   of the header field for the first reliable provisional response in a
   transaction MUST be between 1 and 2**31 - 1.  It is RECOMMENDED that
   it be chosen uniformly in this range.  The RSeq numbering space is
   within a single transaction.  This means that provisional responses
   for different requests MAY use the same values for the RSeq number.


Section 4:

   If a provisional response is received for an initial request, and
   that response contains a Require header field containing the option
   tag 100rel, the response is to be sent reliably.  If the response is
   a 100 (Trying) (as opposed to 101 to 199), this option tag MUST be
   ignored, and the procedures below MUST NOT be used.

   The provisional response MUST establish a dialog if one is not yet
   created.

   Assuming the response is to be transmitted reliably, the UAC MUST
   create a new request with method PRACK.  This request is sent within
   the dialog associated with the provisional response (indeed, the
   provisional response may have created the dialog).  PRACK requests
   MAY contain bodies, which are interpreted according to their type and
   disposition.

   Note that the PRACK is like any other non-INVITE request within a
   dialog.  In particular, a UAC SHOULD NOT retransmit the PRACK request
   when it receives a retransmission of the provisional response being
   acknowledged, although doing so does not create a protocol error.

   Once a reliable provisional response is received, retransmissions of
   that response MUST be discarded.  A response is a retransmission when
   its dialog ID, CSeq, and RSeq match the original response.  The UAC
   MUST maintain a sequence number that indicates the most recently
   received in-order reliable provisional response for the initial
   request.  This sequence number MUST be maintained until a final
   response is received for the initial request.  Its value MUST be
   initialized to the RSeq header field in the first reliable
   provisional response received for the initial request.

   Handling of subsequent reliable provisional responses for the same
   initial request follows the same rules as above, with the following
   difference: reliable provisional responses are guaranteed to be in
   order.  As a result, if the UAC receives another reliable provisional
   response to the same request, and its RSeq value is not one higher
   than the value of the sequence number, that response MUST NOT be
   acknowledged with a PRACK, and MUST NOT be processed further by the
   UAC.  An implementation MAY discard the response, or MAY cache the
   response in the hopes of receiving the missing responses.

```

**Corrected text:**

```
Section 3:

   The provisional response to be sent reliably is constructed by the
   UAS core according to the procedures of Section 8.2.6 of RFC 3261.
   In addition, it MUST contain a Require header field containing the
   option tag 100rel, and MUST include an RSeq header field.  The
   value of the header field for the first reliable provisional 
   response in a transaction MUST be between 1 and 2**31 - 1.  It is
   RECOMMENDED that it be chosen uniformly in this range. The RSeq 
   numbering space is within a single transaction. This means that
   the RSeq value of a provisional response within a fork of a request
   is independent of the RSeq value of a provisional response within 
   any other fork of that request, or for the responses for any other
   request. It thus may be higher, lower, or the same as any other such
   RSeq value.


Section 4:

   If a provisional response is received for an initial request, and
   that response contains a Require header field containing the option
   tag 100rel, the response was sent by the UAS reliably.  If the
   response is a 100 (Trying) (as opposed to 101 to 199), this option
   tag MUST be ignored, and the procedures below MUST NOT be used.

   The provisional response MUST establish a dialog if one is not yet
   created.

   Assuming the response was transmitted reliably by the UAS, the UAC
   MUST create a new request with method PRACK.  This request is sent
   within the dialog associated with the provisional response (indeed,
   the provisional response may have created the dialog). PRACK 
   requests MAY contain bodies, which are interpreted according to
   their type and disposition.

   Note that the PRACK is like any other non-INVITE request within a
   dialog. In particular, a UAC SHOULD NOT retransmit the PRACK request
   when it receives a retransmission of the provisional response being
   acknowledged, although doing so does not create a protocol error.

   Once a reliable provisional response is received, retransmissions of
   that response MUST be discarded.  A response is a retransmission 
   when its dialog ID, CSeq, and RSeq match the original response. The
   UAC MUST maintain, independently for each dialog ID, a sequence
   number that indicates the most recently received in-order reliable
   provisional response for the initial request.  This sequence number
   MUST be maintained until a final response is received for the
   initial request. Its value MUST, for each dialog (or early dialog),
   be initialized to the RSeq header field in the first reliable
   provisional response, associated with the dialog, received for the
   initial request.

   Subsequent reliable provisional responses for the same initial
   request are guaranteed  to have been generated by the UAS in the
   order of their RSeq values and must be acknowledged in that order.
   As a result, if the UAC receives another reliable provisional
   response to the same request, and its RSeq value is one higher than
   the value of the previously received RSeq value in the dialog (or 
   early dialog), then the new RSeq value is saved and the response is
   handled as described above. If the RSeq value is not one higher than
   the value of the sequence number, that response MUST NOT be
   acknowledged with a PRACK, and MUST NOT be processed further by the
   UAC. An implementation MAY discard the response, or MAY cache the
   response to be processed (and acknowledged) after receiving the
   missing responses.
```

**Notes:**

```
--VERIFIER NOTES-- 
The erratum is invalid or proposes a significant change to the RFC that should be done by publishing a new RFC that replaces or updates the current one.
```

