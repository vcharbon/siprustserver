# Errata for RFC 4028

Applies to [rfc4028.txt](rfc4028.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc4028>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 4 |
| Held for Document Update | 1 |
| Reported | 2 |

---

## Erratum 632 — Technical, Verified

Section 9; reported by Ervin Wittner on 2007-06-25.

**Original text:**

```
If the incoming request contains a Supported header field with a
value 'timer' but does not contain a Session-Expires header, it means
that the UAS is indicating support for timers but is not requesting
one.
```

**Corrected text:**

```
If the incoming request contains a Supported header field with a
value 'timer' but does not contain a Session-Expires header, it means
that the UAC is indicating support for timers but is not requesting
one.
```

**Notes:**

```
I believe that in the first sentence, the reference to "...the UAS is
indicating support...." should read "... the UAC is indicating
support..."
```

---

## Erratum 1681 — Technical, Verified

Section 13; reported by Muthu Arul Mozhi on 2009-02-09.

**Original text:**

```
In section 13 (Example Call Flow) the From tag never changes 
between the initial INVITE message and the subsequent INVITE 
messages sent after receiving a 422:

message 1
   INVITE sips:bob@biloxi.example.com SIP/2.0
   Via: SIP/2.0/TLS pc33.atlanta.example.com;branch=z9hG4bKnashds8
   Supported: timer
   Session-Expires: 50
   Max-Forwards: 70
   To: Bob <sips:bob@biloxi.example.com>
   From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
   Call-ID: a84b4c76e66710
   CSeq: 314159 INVITE
   Contact: <sips:alice@pc33.atlanta.example.com>
   Content-Type: application/sdp
   Content-Length: 142

message 4
   INVITE sips:bob@biloxi.example.com SIP/2.0
   Via: SIP/2.0/TLS pc33.atlanta.example.com;branch=z9hG4bKnashds9
   Supported: timer
   Session-Expires: 3600
   Min-SE: 3600
   Max-Forwards: 70
   To: Bob <sips:bob@biloxi.example.com>
   From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
   Call-ID: a84b4c76e66710
   CSeq: 314160 INVITE
   Contact: <sips:alice@pc33.atlanta.example.com>
   Content-Type: application/sdp
   Content-Length: 142

message 10
   INVITE sips:bob@biloxi.example.com SIP/2.0
   Via: SIP/2.0/TLS pc33.atlanta.example.com;branch=z9hG4bKnashds10
   Supported: timer
   Session-Expires: 4000
   Min-SE: 4000
   Max-Forwards: 70
   To: Bob <sips:bob@biloxi.example.com>
   From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
   Call-ID: a84b4c76e66710
   CSeq: 314161 INVITE
   Contact: <sips:alice@pc33.atlanta.example.com>
   Content-Type: application/sdp

However, as per RFC 3261, if an initial INVITE generates a non-2xx final
response, that terminates all sessions and all dialogs that were created. 

Hence, these are not re-INVITE messages, rather new INVITE messages and 
should use a new From tag.
```

**Corrected text:**

```
message 1
   INVITE sips:bob@biloxi.example.com SIP/2.0
   Via: SIP/2.0/TLS pc33.atlanta.example.com;branch=z9hG4bKnashds8
   Supported: timer
   Session-Expires: 50
   Max-Forwards: 70
   To: Bob <sips:bob@biloxi.example.com>
   From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
   Call-ID: a84b4c76e66710
   CSeq: 314159 INVITE
   Contact: <sips:alice@pc33.atlanta.example.com>
   Content-Type: application/sdp
   Content-Length: 142

message 4
   INVITE sips:bob@biloxi.example.com SIP/2.0
   Via: SIP/2.0/TLS pc33.atlanta.example.com;branch=z9hG4bKnashds9
   Supported: timer
   Session-Expires: 3600
   Min-SE: 3600
   Max-Forwards: 70
   To: Bob <sips:bob@biloxi.example.com>
   From: Alice <sips:alice@atlanta.example.com>;tag=2568701785
   Call-ID: a84b4c76e66710
   CSeq: 314160 INVITE
   Contact: <sips:alice@pc33.atlanta.example.com>
   Content-Type: application/sdp
   Content-Length: 142

message 10
   INVITE sips:bob@biloxi.example.com SIP/2.0
   Via: SIP/2.0/TLS pc33.atlanta.example.com;branch=z9hG4bKnashds10
   Supported: timer
   Session-Expires: 4000
   Min-SE: 4000
   Max-Forwards: 70
   To: Bob <sips:bob@biloxi.example.com>
   From: Alice <sips:alice@atlanta.example.com>;tag=5647301796
   Call-ID: a84b4c76e66710
   CSeq: 314161 INVITE
   Contact: <sips:alice@pc33.atlanta.example.com>
   Content-Type: application/sdp
```

**Notes:**

```
-----Original Message-----
From: Paul Kyzivat (pkyzivat) 
Sent: Monday, February 09, 2009 10:09 PM
To: Muthu ArulMozhi Perumal (mperumal)
Cc: Radha Krishna Saragadam (rsaragad); Jonathan Rosenberg (jdrosen); Ram Mohan R (rmohanr)
Subject: Re: UAS behavior after sending 422 for initial INVITE

yes, I think so.

	Paul

Muthu ArulMozhi Perumal (mperumal) wrote:
> In section 13 (Example Call Flow) of RFC 4028 the From tag never changes
> between the initial INVITE message and the subsequent INVITE messages
> sent after receiving a 422:
> 
> message 1
>    INVITE sips:bob@biloxi.example.com SIP/2.0
>    From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
>    Call-ID: a84b4c76e66710
> 
> message 4
>    INVITE sips:bob@biloxi.example.com SIP/2.0
>    From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
>    Call-ID: a84b4c76e66710
> 
> message 10
>    INVITE sips:bob@biloxi.example.com SIP/2.0
>    From: Alice <sips:alice@atlanta.example.com>;tag=1928301774
>    Call-ID: a84b4c76e66710
> 
> Is this a bug in the RFC?
> 
> thanks,
> Muthu
> 
> |-----Original Message-----
> |From: Paul Kyzivat (pkyzivat)
> |Sent: Wednesday, February 04, 2009 12:36 AM
> |To: Radha Krishna Saragadam (rsaragad)
> |Cc: Jonathan Rosenberg (jdrosen); Muthu ArulMozhi Perumal (mperumal);
> Ram Mohan R (rmohanr)
> |Subject: Re: UAS behavior after sending 422 for initial INVITE
> |
> |Radha,
> |
> |It is not a reinvite, because a dialog was never established - the
> first
> |call failed.
> |
> |So you are starting a new invite. You can use the same callid, but
> |should use a new from-tag.
> |
> |	Thanks,
> |	Paul
> |
> |Radha Krishna Saragadam (rsaragad) wrote:
> |> Hi Paul
> |>
> |> 	My question is for initial INVITE. For initial INVITE if UA
> |> receives 422 and UA want to retry INVITE with new value increased
> value
> |> then what should be the To(with tag), From(with tag) and CallID
> values?
> |> Is it a Re-INVITE or new a Dialog? Section 7.3 says same value for
> |> To,From and CallID
> |>
> |> Regards
> |> S.Radha krishna
```

---

## Erratum 3614 — Technical, Verified

Section 13; reported by José María González Calabozo on 2013-05-06.

**Original text:**

```
   Record-Route: sips:p1.atlanta.example.com;lr
   Route: sips:p1.atlanta.example.com;lr
```

**Corrected text:**

```
   Record-Route: <sips:p1.atlanta.example.com;lr>
   Route: <sips:p1.atlanta.example.com;lr>
```

**Notes:**

```
In the examples, all the Record-Route and Route headers are wrong.

They are:
   Record-Route: sips:p1.atlanta.example.com;lr
   Route: sips:p1.atlanta.example.com;lr

But according with the section "25.1 Basic Rules" of RFC3261 those headers are defined as:

name-addr      =  [ display-name ] LAQUOT addr-spec RAQUOT
Record-Route  =  "Record-Route" HCOLON rec-route *(COMMA rec-route)
rec-route     =  name-addr *( SEMI rr-param )
rr-param      =  generic-param
Route        =  "Route" HCOLON route-param *(COMMA route-param)
route-param  =  name-addr *( SEMI rr-param )

So the headers should be:
   Record-Route: <sips:p1.atlanta.example.com;lr>
   Route: <sips:p1.atlanta.example.com;lr>
```

---

## Erratum 4056 — Editorial, Verified

Section 8; reported by Lucas Wang on 2014-07-17.

**Original text:**

```
8.  Proxy Behavior

   Session timers are mostly of interest to call stateful proxy servers
   (that is, to servers that maintain the state of calls and dialogs
   established through them).  However, a stateful proxy server (that
   is, a server which is aware of transaction state but does not retain
   call or dialog state) MAY also follow the rules described here.
   Stateless proxies MUST NOT attempt to request session timers.
   Proxies that ask for session timers SHOULD record-route, as they
   won't receive refreshes if they don't.
```

**Corrected text:**

```
   Session timers are mostly of interest to call stateful proxy servers
   (that is, to servers that maintain the state of calls and dialogs
   established through them).  However, a stateless proxy server (that
   is, a server which is aware of transaction state but does not retain
   call or dialog state) MAY also follow the rules described here.
   Stateless proxies MUST NOT attempt to request session timers.
   Proxies that ask for session timers SHOULD record-route, as they
   won't receive refreshes if they don't.
```

**Notes:**

```
After the "However", it should be a stateless proxy server.
```

---

## Erratum 1687 — Technical, Held for Document Update

Section 9; reported by Radha krishna Saragadam on 2009-02-19.

**Original text:**

```
The UAS MUST NOT increase the value of the Session-Expires header field.
```

**Corrected text:**

```
same as session 8.1
   If the request doesn't indicate support for the session timer but
   contains a session interval that is too small, the UAS cannot
   usefully reject the request, as this would result in a call failure.
   Rather, the UAS SHOULD insert a Min-SE header field containing its
   minimum interval.  If a Min-SE header field is already present, the
   UAS SHOULD increase (but MUST NOT decrease) the value to its
   minimum interval.  The UAS MUST then increase the Session-Expires
   header field value to be equal to the value in the Min-SE header
   field
```

**Notes:**

```
----- Forwarded Message ----
From: Radha krishna <krishna_srk2003@yahoo.com>
To: Brett Tate <brett@broadsoft.com>; "sip-implementors@lists.cs.columbia.edu" <sip-implementors@lists.cs.columbia.edu>
Sent: Thursday, February 19, 2009 10:56:31 AM
Subject: Re: [Sip-implementors] Sending 422

Thanks Brett,

      So I think same should be added for UAS

<Snip from RFC section 8.1>

  If the request doesn't indicate support for the session timer but
 
contains a session interval that is too small, the proxy cannot
  usefully
reject the request, as this would result in a call failure.
  Rather, the
proxy SHOULD insert a Min-SE header field containing its
  minimum
interval.  If a Min-SE header field is already present, the
  proxy SHOULD
increase (but MUST NOT decrease) the value to its
  minimum interval.  The
proxy MUST then increase the Session-Expires
  header field value to be equal to the value in the
Min-SE header
  field, as described above.
</Snip from RFC>


Regards
S.Radha krishna




________________________________
From: Brett Tate <brett@broadsoft.com>
To: Radha krishna <krishna_srk2003@yahoo.com>; "sip-implementors@lists.cs.columbia.edu" <sip-implementors@lists.cs.columbia.edu>
Sent: Wednesday, February 18, 2009 6:42:31 PM
Subject: RE: [Sip-implementors] Sending 422

It looks like Section 9 may have forgotten to indicate the behavior when UAC timer support not indicated.  Section 8.1 allows a proxy to increase the Session-Expires; I see no reason why the same cannot be done by the UAS.

> -----Original Message-----
> From: sip-implementors-bounces@lists.cs.columbia.edu [mailto:sip-
> implementors-bounces@lists.cs.columbia.edu] On Behalf Of Radha krishna
> Sent: Tuesday, February 17, 2009 9:48 PM
> To: sip-implementors@lists.cs.columbia.edu
> Subject: [Sip-implementors] Sending 422
>
> Hi
>
>          Consider the following topology
>              UA1 ----- Call-stateful-proxy ------ UA2
>
>          UA1 does not support session timer, Make a call to UA2. Call-
> stateful-proxy adds Session-Expires:100 header and forwards to UA2. UA2
> minimum session expires is 900. But in this case INVITE will not contain
> "support: timer". According section 9, UAS can reject with 422 only if
> there is a timer tag in supported header
>
> <Snip from RFC>
>
>    If an incoming request contains a Supported header field with a value
>    'timer' and a Session Expires header field, the UAS MAY reject the
>    INVITE request with a 422 (Session Interval Too Small) response if
>    the session interval in the Session-Expires header field is smaller
>    than the minimum interval defined by the UAS' local policy.  When
>    sending the 422 response, the UAS MUST include a Min-SE header field
>    with the value of its minimum interval.  This minimum interval MUST
>    NOT be lower than 90 seconds.
> </Snip from RFC>
>
>        Also UAS cannot increase the session expires duration
> <Snip from RFC>
> The UAS MUST
>    NOT increase the value of the Session-Expires header field.
> </Snip from RFC>
>
> What should be the behavior of UAS here?
>    1) accept the call with 100 seconds?
>    2) Increase the duration to 900 seconds while sending 200 Ok?
> Note: Session timer should not be turned-off
>
> Regards
> S.Radha krishna
>
>
>
>
> _______________________________________________
> Sip-implementors mailing list
> Sip-implementors@lists.cs.columbia.edu
> https://lists.cs.columbia.edu/cucslists/listinfo/sip-implementors



     
_______________________________________________
Sip-implementors mailing list
Sip-implementors@lists.cs.columbia.edu
https://lists.cs.columbia.edu/cucslists/listinfo/sip-implementors
```

---

## Erratum 4744 — Technical, Reported

Section 3; reported by Chao Wang on 2016-07-19.

**Original text:**

```
   A UAC starts by sending an INVITE.  This includes a Supported header
   field with the option tag 'timer', indicating support for this
   extension.
```

**Corrected text:**

```
   A UAC starts by sending an INVITE.  This includes a Supported header
   field with the option tag 'timer', indicating support for this
   extension.If UAC or UAS knows one negotiation of session timer is 
   ongoing, it SHALL not start a new one.
```

**Notes:**

```
Add one more sentence here "If UAC knows one negotiation of session timer is ongoing, it SHALL  not start a new one."
Actually in the scenarios of session set-up or session modification, it is easy to see the message exchange of UPDATE and (re-)INVITE, according to this RFC, both of them are allowed for the negotiation of session timer, the decision shall be taken on their own 200 OK. Normally the negotiation of session time of (re-)INVITE is started at first and the UPDATE's will be taken place, from UAS perspective, it needs to treat two negotiations of session timer in-parallel, since there is no kind of mechanism to protect the parameters carried in (re-)INVITE and UPDATE being inconsistent, it brings a problem on UAS that the result of two negotiations might be different specially for the parameter of refresher and also the inconsistency will cause UAS to meet a conflict while it is deciding the parameters especially for refresher on 200 OK of (re-)INVITE. In order to avoid this problem, it is better to disallow a net negotiation of session timer until the ongoing one is finished.
```

---

## Erratum 8926 — Technical, Reported

Section 8.2; reported by Tran Minh Duc on 2026-05-25.

**Original text:**

```
If, however, the proxy remembers that
   the UAC did support the session timer, additional processing is
   needed.
```

**Corrected text:**

```
If, however, the proxy remembers that
   the UAC did support the session timer, and UAC's desired refresher is not set to 'uas',
   additional processing is needed.
```

**Notes:**

```
A UAC MAY include the refresher parameter with value 'uas' in the Session-Expires header of an initial INVITE request, explicitly indicating that it is not willing to act as the refreshing party and expects the UAS to perform session refresh. In such a case, a proxy SHOULD NOT override the refresher parameter to 'uac' and SHOULD NOT insert the 'timer' option tag into any Require header field in the response. The proxy SHOULD forward the response upstream normally.

Note that Section 7.2 defines a case: when no Require or Session-Expires header field is present in the 2xx response (i.e., the UAS does not support the session timer extension), the UAC that still wishes to use the session timer performs refresh itself.
```

