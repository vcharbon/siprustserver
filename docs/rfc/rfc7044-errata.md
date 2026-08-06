# Errata for RFC 7044

Applies to [rfc7044.txt](rfc7044.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc7044>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Reported | 3 |
| Rejected | 2 |

---

## Erratum 5014 — Technical, Reported

Section 9.1 and 10.3; reported by Dinoop Paloli on 2017-05-10.

**Original text:** *(none given)*

**Corrected text:** *(none given)*

**Notes:**

```
Ambiguity exists regarding the handling of missing history info entry

Section 9.1 says,
	If the Request-URI of the incoming request does not match the hi
   -targeted-to-uri in the last hi-entry (i.e., the previous SIP entity
   that sent the request did not include a History-Info header field),
   the SIP entity MUST add an hi-entry to the end of the cache on
   behalf of the previous SIP entity
  
	According to that, for example, if request is received with,
	Request URI : sip:peter@example.com
	and History info :  <sip:bob@example.com>;index=1
						<sip:alice@example>;index=1.1
						<sip:jain@example>;index=1.1.1
						<sip:dave@example>;index=1.1.2

	Then processing entity has to add an history info in to cache on behalf of previous entity as,
	History info : <sip:bob@example.com>;index=1
               <sip:alice@example>;index=1.1
			   <sip:jain@example>;index=1.1.1
			   <sip:dave@example>;index=1.1.2
			   <sip:peter@example.com>;index=1.1.2.1
			   
But in section 10.3 basic rules 6 states,
		If the request clearly has a gap in the hi-entry
       (i.e., the last hi-entry and Request-URI differ), the entity
       adding an hi-entry MUST add a single index with a value of "0"
       (i.e., the non negative integer zero) prior to adding the
       appropriate index for the action to be taken. For example, if
       the index of the last hi-entry in the request received was 1.1.2
       and there was a missing hi-entry and the request was being
       forwarded to the next hop, the resulting index will be 1.1.2.0.1.
	   
	   But as per 9.1 stated above, once an entity receive a request with missing history info
		it has to add an entry to cache on behalf of previous one.
		So referring the previous example the added index would be 1.1.2.1
		And by applying the rule in 10.3, the index for the new request created by this entity would be 1.1.2.1.0.1 not 1.1.2.0.1
```

---

## Erratum 5442 — Technical, Reported

Section 9.3,10.2; reported by Ted Zhou on 2018-07-27.

**Original text:**

```
9.3   
   When a SIP entity receives a non-100 response or a request times out,
   the SIP entity performs the following steps:

      If the response is not a 100 or 2xx response, the SIP entity adds
      one or more Reason header fields to the hi-targeted-to-uri in the
      (newly) cached hi-entry reflecting the SIP response code in the
      non-100 or non-2xx response, per the procedures of Section 10.2.

10.2
   A Reason header field is added when the hi-entry is added to the
   cache based upon the receipt of a SIP response that is neither a 100
   nor a 2xx response, as described in Section 9.3. 
```

**Corrected text:**

```
9.3   
   When a SIP entity receives a non-18x response or a request times out,
   the SIP entity performs the following steps:

      If the response is not a 18x or 2xx response, the SIP entity adds
      one or more Reason header fields to the hi-targeted-to-uri in the
      (newly) cached hi-entry reflecting the SIP response code in the
      non-18x or non-2xx response, per the procedures of Section 10.2.

10.2
   A Reason header field is added when the hi-entry is added to the
   cache based upon the receipt of a SIP response that is neither a 18x
   nor a 2xx response, as described in Section 9.3. 
```

**Notes:**

```
I see we have several places using "100" or "non-100". I think the correct one should be "18x" or "non-18x".
Or, we can use "1xx" or "non-1xx".

100 means "100 Tring", it's not accurate.
```

---

## Erratum 4345 — Editorial, Reported

Section 6.2; reported by chao wang on 2015-04-22.

**Original text:**

```
6.2.  User Agent Server (UAS) Behavior

   When receiving a request, a UAS MUST follow the procedures defined in
   Section 9.2.
```

**Corrected text:**

```
6.2.  User Agent Server (UAS) Behavior

   When receiving a request, a UAS MUST follow the procedures defined in
   Section 9.1.
```

**Notes:**

```
In fact, the subject of 9.2 is "Sending a Request with History-Info", according to the original text, it wants to refer to how to handle a received request in UAS, the right reference shall be section 9.1 (9.1.  Receiving a Request).
```

---

## Erratum 4181 — Editorial, Rejected

Section 9.3; reported by Hans Erik van Elburg on 2014-11-14.

**Original text:**

```
9.3. Receiving a Response with History-Info or Request Timeouts
```

**Corrected text:**

```
9.3. Receiving a Response or Request times out
```

**Notes:**

```
The title of section 9.3 is misleading in the sense that it suggests that it only applies to responses that contain the History-Info header. This is not correct, it applies to all responses when the RFC7044 applies.
 --VERIFIER NOTES-- 
   This errata indicates an editorial preference rather than an error.
```

---

## Erratum 5012 — Editorial, Rejected

Section 10.1.2; reported by Dinoop Paloli on 2017-05-10.

**Original text:**

```
If there is not a Privacy header field in the message
```

**Corrected text:**

```
If there is no Privacy header field in the message
```

**Notes:**

```
"If there is not a Privacy header" is not a correct sentence. We have to use  "If there is no Privacy header"
 --VERIFIER NOTES-- 
   Either usage is acceptable.
```

