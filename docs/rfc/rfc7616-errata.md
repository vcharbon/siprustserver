# Errata for RFC 7616

Applies to [rfc7616.txt](rfc7616.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc7616>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |
| Held for Document Update | 2 |
| Reported | 4 |

---

## Erratum 4495 — Editorial, Verified

Section 3.9.1.; reported by Tuomo Untinen on 2015-10-09.

**Original text:**

```
   Both client and
   server know that the username for this document is "Mufasa" and the
   password is "Circle of Life" (with one space between each of the
   three words).
```

**Corrected text:**

```
   Both client and
   server know that the username for this document is "Mufasa" and the 
   password is "Circle of Life" (with one space between 
   each of the three words and non-capital o in word of).
```

**Notes:**

```
In RFC 2617, the password was "Circle Of Life" with capital O in the word "Of". Also, RFC 7616 section 3.4.5 mentions the password "Circle Of Life" with capital O in the word "Of". It can be difficult to notice a non-capital o from an example password as it is elsewhere capital O.
```

---

## Erratum 5801 — Editorial, Held for Document Update

Section 3.7; reported by Franck MOURRE on 2019-08-06.

**Original text:**

```
   This specification defines the following algorithms:

   o  SHA2-256 (mandatory to implement)

   o  SHA2-512/256 (as a backup algorithm)

   o  MD5 (for backward compatibility).
```

**Corrected text:**

```
   This specification defines the following algorithms:

   o  SHA-256 (mandatory to implement)

   o  SHA-512/256 (as a backup algorithm)

   o  MD5 (for backward compatibility).
```

**Notes:**

```
The SHA-2 family of algorithms are conventionally referred to using just "SHA-" and the bit strength, not "SHA2-" and the bit strength.
```

---

## Erratum 5803 — Editorial, Held for Document Update

Section A; reported by Franck MOURRE on 2019-08-06.

**Original text:**

```
   o  Adds support for two new algorithms, SHA2-256 as mandatory and
      SHA2-512/256 as a backup, and defines the proper algorithm
      negotiation.  The document keeps the MD5 algorithm support but
      only for backward compatibility.
```

**Corrected text:**

```
   o  Adds support for two new algorithms, SHA-256 as mandatory and
      SHA-512/256 as a backup, and defines the proper algorithm
      negotiation.  The document keeps the MD5 algorithm support but
      only for backward compatibility.
```

**Notes:**

```
The SHA-2 family of algorithms are conventionally referred to using just "SHA-" and the bit strength, not "SHA2-" and the bit strength.
```

---

## Erratum 4897 — Technical, Reported

Section 3.9.2; reported by Chaim Geretz on 2016-12-29.

**Original text:**

```
3.9.2.  Example with SHA-512-256, Charset, and Userhash

   The following example assumes that an access-protected document is
   being requested from the server via a GET request.  The URI for the
   request is "http://api.example.org/doe.json".  Both client and server
   know the userhash of the username, support the UTF-8 character
   encoding scheme, and use the SHA-512-256 algorithm.  The username for
   the request is a variation of "Jason Doe", where the 'a' actually is
   Unicode code point U+00E4 ("LATIN SMALL LETTER A WITH DIAERESIS"),
   and the first 'o' is Unicode code point U+00F8 ("LATIN SMALL LETTER O
   WITH STROKE"), leading to the octet sequence using the UTF-8 encoding
   scheme:

      J  U+00E4 s  U+00F8 n      D  o  e
      4A C3A4   73 C3B8   6E 20 44  6F 65

   The password is "Secret, or not?".

   The first time the client requests the document, no Authorization
   header field is sent, so the server responds with:

   HTTP/1.1 401 Unauthorized
   WWW-Authenticate: Digest
       realm="api@example.org",
       qop="auth",
       algorithm=SHA-512-256,
       nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK",
       opaque="HRPCssKJSGjCrkzDg8OhwpzCiGPChXYjwrI2QmXDnsOS",
       charset=UTF-8,
       userhash=true





Shekh-Yusef, et al.          Standards Track                   [Page 19]

RFC 7616            HTTP Digest Access Authentication     September 2015


   The client can prompt the user for the required credentials and send
   a new request with following Authorization header field:

   Authorization: Digest
       username="488869477bf257147b804c45308cd62ac4e25eb717
          b12b298c79e62dcea254ec",
       realm="api@example.org",
       uri="/doe.json",
       algorithm=SHA-512-256,
       nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK",
       nc=00000001,
       cnonce="NTg6RKcb9boFIAS3KrFK9BGeh+iDa/sm6jUMp2wds69v",
       qop=auth,
       response="ae66e67d6b427bd3f120414a82e4acff38e8ecd9101d
          6c861229025f607a79dd",
       opaque="HRPCssKJSGjCrkzDg8OhwpzCiGPChXYjwrI2QmXDnsOS",
       userhash=true

   If the client cannot provide a hashed username for any reason, the
   client can try a request with this Authorization header field:

   Authorization: Digest
       username*=UTF-8''J%C3%A4s%C3%B8n%20Doe,
       realm="api@example.org",
       uri="/doe.json",
       algorithm=SHA-512-256,
       nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK",
       nc=00000001,
       cnonce="NTg6RKcb9boFIAS3KrFK9BGeh+iDa/sm6jUMp2wds69v",
       qop=auth,
       response="ae66e67d6b427bd3f120414a82e4acff38e8ecd9101d
          6c861229025f607a79dd",
       opaque="HRPCssKJSGjCrkzDg8OhwpzCiGPChXYjwrI2QmXDnsOS",
       userhash=false
```

**Corrected text:**

```
3.9.2.  Example with SHA-512-256, Charset, and Userhash

   The following example assumes that an access-protected document is
   being requested from the server via a GET request.  The URI for the
   request is "http://api.example.org/doe.json".  Both client and server
   know the userhash of the username, support the UTF-8 character
   encoding scheme, and use the SHA-512-256 algorithm.  The username for
   the request is a variation of "Jason Doe", where the 'a' actually is
   Unicode code point U+00E4 ("LATIN SMALL LETTER A WITH DIAERESIS"),
   and the first 'o' is Unicode code point U+00F8 ("LATIN SMALL LETTER O
   WITH STROKE"), leading to the octet sequence using the UTF-8 encoding
   scheme:

      J  U+00E4 s  U+00F8 n      D  o  e
      4A C3A4   73 C3B8   6E 20 44  6F 65

   The password is "Secret, or not?".

   The first time the client requests the document, no Authorization
   header field is sent, so the server responds with:

   HTTP/1.1 401 Unauthorized
   WWW-Authenticate: Digest
       realm="api@example.org",
       qop="auth",
       algorithm=SHA-512-256,
       nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK",
       opaque="HRPCssKJSGjCrkzDg8OhwpzCiGPChXYjwrI2QmXDnsOS",
       charset=UTF-8,
       userhash=true





Shekh-Yusef, et al.          Standards Track                   [Page 19]

RFC 7616            HTTP Digest Access Authentication     September 2015


   The client can prompt the user for the required credentials and send
   a new request with following Authorization header field:

   Authorization: Digest
       username="793263caabb707a56211940d90411ea4a575adeccb
          7e360aeb624ed06ece9b0b",
       realm="api@example.org",
       uri="/doe.json",
       algorithm=SHA-512-256,
       nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK",
       nc=00000001,
       cnonce="NTg6RKcb9boFIAS3KrFK9BGeh+iDa/sm6jUMp2wds69v",
       qop=auth,
       response="3798d4131c277846293534c3edc11bd8a5e4cdcbff78
          b05db9d95eeb1cec68a5",
       opaque="HRPCssKJSGjCrkzDg8OhwpzCiGPChXYjwrI2QmXDnsOS",
       userhash=true

   If the client cannot provide a hashed username for any reason, the
   client can try a request with this Authorization header field:

   Authorization: Digest
       username*=UTF-8''J%C3%A4s%C3%B8n%20Doe,
       realm="api@example.org",
       uri="/doe.json",
       algorithm=SHA-512-256,
       nonce="5TsQWLVdgBdmrQ0XsxbDODV+57QdFR34I9HAbC/RVvkK",
       nc=00000001,
       cnonce="NTg6RKcb9boFIAS3KrFK9BGeh+iDa/sm6jUMp2wds69v",
       qop=auth,
       response="3798d4131c277846293534c3edc11bd8a5e4cdcbff78
          b05db9d95eeb1cec68a5",
       opaque="HRPCssKJSGjCrkzDg8OhwpzCiGPChXYjwrI2QmXDnsOS",
       userhash=false
```

**Notes:**

```
If the SHA512/256 algorithm first mentioned in Section 3.2 is to be implemented as defined in FIPS 180.4 Section 6.7 the values of username and response need to be corrected.

Changes start at page 19 of the RFC.

I created this as technical errata since the current values given in the example are for a SHA512/256 algorithm implemented as described in Section 7 of both FIPS 180-3 and FIPS 180-4
```

---

## Erratum 7005 — Technical, Reported

Section 3.9.1; reported by Patrick Ni on 2022-06-23.

**Original text:**

```
uri="/dir/index.html",
```

**Corrected text:**

```
uri="http://www.example.org/dir/index.html",
```

**Notes:**

```
[1] In Section 3.4, uri -> The Effective Request URI (Section 5.5 of [RFC7230]) of the HTTP request;

[2] In Section 5.5 of [RFC7230]: The components of the effective request URI, once determined as above, can be combined into absolute-URI form by concatenating the scheme, "://", authority, and combined path and query component

[3] uri="/dir/index.html" is an absolute-path, not an Effective Request URI
```

---

## Erratum 7936 — Technical, Reported

Section 3.3; reported by Joe Orton on 2024-05-13.

**Original text:**

```
   domain

      A quoted, space-separated list of URIs, as specified in [RFC3986],
      that define the protection space.  If a URI is a path-absolute, it
      is relative to the canonical root URL.  (See Section 2.2 of
```

**Corrected text:**

```
   domain

      A quoted, space-separated list of URI-reference strings, as specified in [RFC3986],
      that define the protection space.  If a URI-reference is in a relative form, it
      is relative to the canonical root URL.  (See Section 2.2 of
```

**Notes:**

```
The definition of the "domain" parameter is inconsistent/contradictory - a list of space-separated URIs cannot include a path-absolute, since path-absolute is not a URI - though it is a URI-reference. If the intent was that "a space-separated list of URI-reference strings" is allowed, that could be used instead, per my suggested corrected text. 

It is likely both that the intent was not to allow any URI-reference here, and that current client implementations accept only absolute-URI or path-absolute. So it could instead be clarified as follows:

    A quoted, space-separated list of either absolute-URI or path-absolute, as specified in [RFC3986], that define the protection space.
```

---

## Erratum 6704 — Editorial, Reported

Section 3.4.1; reported by Bruce Florman on 2021-10-05.

**Original text:**

```
3.4.1.  Response

   If the qop value is "auth" or "auth-int":

         response = <"> < KD ( H(A1), unq(nonce)
                                      ":" nc
                                      ":" unq(cnonce)
                                      ":" unq(qop)
                                      ":" H(A2)
                             ) <">

   See below for the definitions for A1 and A2.
```

**Corrected text:**

```
3.4.1.  Response

   If the qop value is "auth" or "auth-int":

         response = <"> < KD ( H(A1), unq(nonce)
                                      ":" nc
                                      ":" unq(cnonce)
                                      ":" unq(qop)
                                      ":" H(A2)
                             ) > <">

   See below for the definitions for A1 and A2.
```

**Notes:**

```
The open angle bracket following the initial double quote, probably needs a matching close angle bracket before the final double quote. This typographical error appears to have been copied from section 3.2.2.1 of RFC 2617, but the close angle bracket does appear in the corresponding single line of text in section 2.1.2 of RFC 2069 that defines the response-digest production there. However, it's not clear to me that the angle brackets contribute to the clarity of the response production here, so simply removing the unmatched open might be a better solution.
```

