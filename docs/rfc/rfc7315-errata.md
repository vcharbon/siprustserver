# Errata for RFC 7315

Applies to [rfc7315.txt](rfc7315.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc7315>) as of 2026-11-07.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Reported | 4 |

---

## Erratum 4474 — Technical, Reported

Section 5.4; reported by Christer Holmberg on 2015-09-14.

**Original text:**

```
extension-access-info  = gen-value
```

**Corrected text:**

```
extension-access-info  = generic-param
```

**Notes:**

```
Most of the pre-defined access-info values are following the generic-param syntax. New access-info values (extensions) should also be allowed to follow the generic-param syntax, in order to allow both for a name and value of the extension.
```

## Erratum 4540 — Technical, Reported

Section 5.1; reported by Dongo Yi on 2015-11-20.

**Original text:**

```
5.1. P-Associated-URI Header Syntax


   The syntax of the P-Associated-URI header field is described as
   follows:

         P-Associated-URI       = "P-Associated-URI" HCOLON
                                  [p-aso-uri-spec]
                                  *(COMMA p-aso-uri-spec)
         p-aso-uri-spec         = name-addr *(SEMI ai-param)
         ai-param               = generic-param
```

**Corrected text:**

```
5.1. P-Associated-URI Header Syntax


   The syntax of the P-Associated-URI header field is described as
   follows:

         P-Associated-URI       = "P-Associated-URI" HCOLON
                                  (p-aso-uri-spec)
                                  *(COMMA p-aso-uri-spec)
         p-aso-uri-spec         = name-addr *(SEMI ai-param)
         ai-param               = generic-param
```

**Notes:**

```
P-Associated-URI cannot include an empty header value (which was able to be included according to RFC 3455)

- Background.

The policy of P-Associated-URI has been updated from RFC 3455.
RFC 3455 : 4.1.2.2 Procedures at the registrar
...
In case the address-of-record under registration does not have any
   other SIP or SIPS URIs associated, the registrar MUST include an
   empty P-Associated-URI header value.


RFC 7315 : 4.1.2.2 Procedures at the Registrar
...
If the address-of-record under registration does not have any
   associated URIs, the P-Associated-URI header field SHALL NOT be
   included.
...

Moreover, we can say RFC 3455 has same errata based on this analysis.
```

## Erratum 4447 — Editorial, Reported

Section 4.6.3.1.; reported by Roland Jesske on 2015-08-17.

**Original text:**

```
	4.6.3.1. Procedures at the Proxy
	
	
	Procedures described within 4.5.2.2 apply. A transit-ioi MAY be
	added or modified by a proxy. A deletion of the transit-ioi or a
	entry within the tranist-ioi could appear depending on the network
	
	policy and trust rules. This is also valid by replacing the transit-
	ioi with a void value.
```

**Corrected text:**

```
4.6.3.1. Procedures at the Proxy


Procedures described within Section 4.6.2.2 apply. A transit-ioi MAY
be added or modified by a proxy. A deletion of the transit-ioi or a
entry within the tranist-ioi could appear depending on the network

policy and trust rules. This is also valid by replacing the
transit-ioi with a void value.
```

**Notes:**

```
Problem is that the Reader is lead to the wrong procedures.
```

## Erratum 4448 — Editorial, Reported

Section 4.6.4.2.; reported by Roland Jesske on 2015-08-17.

**Original text:**

```
4.6.4.2.  Procedures at the Proxy

   Procedures described within Section 4.5.2.2 apply.  A related-icid
   and "related-icid-generated-at" MAY be added or modified by a proxy.
   A deletion of the elements could appear depending on the network
   policy and trust rules.
```

**Corrected text:**

```
4.6.4.2.  Procedures at the Proxy

   Procedures described within Section 4.6.2.2 apply.  A related-icid
   and "related-icid-generated-at" MAY be added or modified by a proxy.
   A deletion of the elements could appear depending on the network
   policy and trust rules.
```

**Notes:**

```
This pointer to a wrong section may lead to a wrong implementation
```
