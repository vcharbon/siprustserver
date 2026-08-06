# Errata for RFC 2617

Applies to [rfc2617.txt](rfc2617.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc2617>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 7 |
| Rejected | 2 |

---

## Erratum 410 — Technical, Verified

Section (unspecified); reported by Scott Lawrence on 2001-01-05.

**Original text:** *(none given)*

**Corrected text:** *(none given)*

**Notes:**

```
All known errata for this HTTP RFC will be found at: 
http://purl.org/NET/http-errata and 
http://www.w3.org/Protocols/HTTP/1.1/rfc2616bis/issues/
```

---

## Erratum 1649 — Technical, Verified

Section 5; reported by Ganga Mahesh Siddem on 2009-01-08.

**Original text:**

```
 /* calculate H(A1) as per spec */
      void DigestCalcHA1(
          IN char * pszAlg,
          IN char * pszUserName,
          IN char * pszRealm,
          IN char * pszPassword,
          IN char * pszNonce,
          IN char * pszCNonce,
          OUT HASHHEX SessionKey
          )
      {
            MD5_CTX Md5Ctx;
            HASH HA1;

            MD5Init(&Md5Ctx);
            MD5Update(&Md5Ctx, pszUserName, strlen(pszUserName));
            MD5Update(&Md5Ctx, ":", 1);
            MD5Update(&Md5Ctx, pszRealm, strlen(pszRealm));
            MD5Update(&Md5Ctx, ":", 1);
            MD5Update(&Md5Ctx, pszPassword, strlen(pszPassword));
            MD5Final(HA1, &Md5Ctx);
            if (stricmp(pszAlg, "md5-sess") == 0) {
                  MD5Init(&Md5Ctx);
|                 MD5Update(&Md5Ctx, HA1, HASHLEN);
                  MD5Update(&Md5Ctx, ":", 1);
                  MD5Update(&Md5Ctx, pszNonce, strlen(pszNonce));
                  MD5Update(&Md5Ctx, ":", 1);
                  MD5Update(&Md5Ctx, pszCNonce, strlen(pszCNonce));
                  MD5Final(HA1, &Md5Ctx);
            };
            CvtHex(HA1, SessionKey);
      };
```

**Corrected text:**

```
 /* calculate H(A1) as per spec */
      void DigestCalcHA1(
          IN char * pszAlg,
          IN char * pszUserName,
          IN char * pszRealm,
          IN char * pszPassword,
          IN char * pszNonce,
          IN char * pszCNonce,
          OUT HASHHEX SessionKey
          )
      {
            MD5_CTX Md5Ctx;
            HASH HA1;
|           HASHHEX HA1Hex;

            MD5Init(&Md5Ctx);
            MD5Update(&Md5Ctx, pszUserName, strlen(pszUserName));
            MD5Update(&Md5Ctx, ":", 1);
            MD5Update(&Md5Ctx, pszRealm, strlen(pszRealm));
            MD5Update(&Md5Ctx, ":", 1);
            MD5Update(&Md5Ctx, pszPassword, strlen(pszPassword));
            MD5Final(HA1, &Md5Ctx);
            if (stricmp(pszAlg, "md5-sess") == 0) {
|                 CvtHex(HA1, HA1Hex);
                  MD5Init(&Md5Ctx);
|                 MD5Update(&Md5Ctx, HA1Hex, HASHHEXLEN);
                  MD5Update(&Md5Ctx, ":", 1);
                  MD5Update(&Md5Ctx, pszNonce, strlen(pszNonce));
                  MD5Update(&Md5Ctx, ":", 1);
                  MD5Update(&Md5Ctx, pszCNonce, strlen(pszCNonce));
                  MD5Final(HA1, &Md5Ctx);
            };
            CvtHex(HA1, SessionKey);
      };
```

**Notes:**

```
DigestCalcHA1 sample implemention has to be corrected.
```

---

## Erratum 1959 — Technical, Verified

Section 1.2 p4; reported by Julian Reschke on 2009-12-10.

**Original text:**

```
       credentials = auth-scheme #auth-param
```

**Corrected text:**

```
       credentials = auth-scheme ( token | quoted-string | #auth-param )
```

**Notes:**

```
Alexey Melnikov (updated as per suggestion from Paul Leach):

auth-param doesn't allow for parameters with no '=', so Basic is non conformant to the generic syntax.

Multiple versions of token/quoted-string (with no attribute name) is not allowed, as none of the existing scheme not using auth-param supports that.

(Note that RFC 2617 is using BNF from RFC 2616, which allows for implied LWS.)
```

---

## Erratum 2600 — Technical, Verified

Section 3.2.2; reported by Victor S. Osipov on 2010-11-02.

**Original text:**

```
digest-uri       = "uri" "=" digest-uri-value
digest-uri-value = request-uri   ; As specified by HTTP/1.1
```

**Corrected text:**

```
digest-uri       = "uri" "=" <"> digest-uri-value <">
digest-uri-value = request-uri   ; As specified by HTTP/1.1
```

**Notes:**

```
This is an error here that the digest-uri-value is not enclosed in quotation marks; 
see the correct example in Section 3.5:

Authorization: Digest username="Mufasa",
        realm="testrealm@host.com",
        nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093",
        uri="/dir/index.html",
        . . .
```

---

## Erratum 3720 — Technical, Verified

Section 3.2.2.4; reported by Brett Tate on 2013-09-06.

**Original text:**

```
username="Mufasa", realm=myhost@testrealm.com
```

**Corrected text:**

```
username="Mufasa", realm="myhost@testrealm.com"
```

**Notes:**

```
The realm value within the Authorization header example is missing the quotes.
```

---

## Erratum 606 — Editorial, Verified

Section 3.6; reported by Stéphane Bortzmeyer on 2007-10-17.

**Original text:**

```
These headers are instances of the Proxy-Authenticate and
Proxy-Authorization headers specified in sections 10.33 and 10.34 of the
HTTP/1.1 specification [2] ...
```

**Corrected text:**

```
These headers are instances of the Proxy-Authenticate and
Proxy-Authorization headers specified in sections 14.33 and 14.34 of the
HTTP/1.1 specification [2] ...
```

**Notes:**

```
Wrong section references in RFC 2616.

Reported by Julian Reschke on an IETF mailing list.
```

---

## Erratum 1431 — Editorial, Verified

Section 3.2.2.1; reported by Stefan Santesson on 2008-05-29.

**Original text:**

```
   If the "qop" value is "auth" or "auth-int":

      request-digest  = <"> < KD ( H(A1),     unq(nonce-value)
                                          ":" nc-value
                                          ":" unq(cnonce-value)
                                          ":" unq(qop-value)
                                          ":" H(A2)
                                  ) <">
```

**Corrected text:**

```
   If the "qop" value is "auth" or "auth-int":

      request-digest  = <"> < KD ( H(A1),     unq(nonce-value)
                                          ":" nc-value
                                          ":" unq(cnonce-value)
                                          ":" unq(qop-value)
                                          ":" H(A2)
                                  ) > <">
```

**Notes:**

```
The ">" bracket is missing in the final line, closing the "<" bracket of the first line in "< KD ( H(A1)"...
```

---

## Erratum 1914 — Technical, Rejected

Section 3.2.2.1; reported by Larry Westrick on 2009-10-14.

**Original text:**

```
3.2.2.1 Request-Digest

   If the "qop" value is "auth" or "auth-int":

      request-digest  = <"> < KD ( H(A1),     unq(nonce-value)
                                          ":" nc-value
                                          ":" unq(cnonce-value)
                                          ":" unq(qop-value)
                                          ":" H(A2)
                                  ) <">

   If the "qop" directive is not present (this construction is for
   compatibility with RFC 2069):

      request-digest  =
                 <"> < KD ( H(A1), unq(nonce-value) ":" H(A2) ) >
   <">

```

**Corrected text:**

```
3.2.2.1 Request-Digest

   If the "qop" value is "auth" or "auth-int":

      request-digest  = <"> < KD ( H(A1)  ":" unq(nonce-value)
                                          ":" nc-value
                                          ":" unq(cnonce-value)
                                          ":" unq(qop-value)
                                          ":" H(A2)
                                  ) <">

   If the "qop" directive is not present (this construction is for
   compatibility with RFC 2069):

      request-digest  =
                 <"> < KD ( H(A1) ":" unq(nonce-value) ":" H(A2) ) >
   <">

```

**Notes:**

```
Errata 1796 addressing this issue and was rejected, perhaps for editorial or syntax reasons, when the section as it exists does not indicate the need for a ":" between A1 and unq(nonce-value). The ":" is most certainly required between these variables if the result of the hash is to be correct.
 --VERIFIER NOTES-- 
   The verifier notes on the rejected Erratum 1796 were as follows:

   ###

   KD is defined in the document as:

   KD(secret, data) = H(concat(secret, ":", data))

   So KD takes 2 parameters and the text in the RFC is correct in this respect.

   ###

   If there is good reason to pursue this issue further, please do so outside
   the errata process.
```

---

## Erratum 1796 — Editorial, Rejected

Section 3.2.2.1; reported by Jerry Conrad on 2009-06-19.

**Original text:**

```
3.2.2.1 Request-Digest

   If the "qop" value is "auth" or "auth-int":

      request-digest  = <"> < KD ( H(A1),     unq(nonce-value)
                                          ":" nc-value
                                          ":" unq(cnonce-value)
                                          ":" unq(qop-value)
                                          ":" H(A2)
                                  ) <">

   If the "qop" directive is not present (this construction is for
   compatibility with RFC 2069):

      request-digest  =
                 <"> < KD ( H(A1), unq(nonce-value) ":" H(A2) ) >
   <">
```

**Corrected text:**

```
3.2.2.1 Request-Digest

   If the "qop" value is "auth" or "auth-int":

      request-digest  = <"> < KD ( H(A1)  ":" unq(nonce-value)
                                          ":" nc-value
                                          ":" unq(cnonce-value)
                                          ":" unq(qop-value)
                                          ":" H(A2)
                                  ) <">

   If the "qop" directive is not present (this construction is for
   compatibility with RFC 2069):

      request-digest  =
                 <"> < KD ( H(A1) ":" unq(nonce-value) ":" H(A2) ) >
   <">
```

**Notes:**

```
The "," after H(A1) should be ":" in two places.
 --VERIFIER NOTES-- 
KD is defined in the document as:

  KD(secret, data) = H(concat(secret, ":", data))

So KD takes 2 parameters and the text in the RFC is correct in this respect.
```

