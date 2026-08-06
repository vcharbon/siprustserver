# Errata for RFC 2104

Applies to [rfc2104.txt](rfc2104.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc2104>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 1 |
| Held for Document Update | 3 |
| Rejected | 3 |

---

## Erratum 501 — Editorial, Verified

Section 9; reported by Gregory Ogonowski on 2005-06-23.

**Original text:**

```
        MD5Update(&context, k_ipad, 64)      /* start with inner pad */
```

**Corrected text:**

```
        MD5Update(&context, k_ipad, 64);     /* start with inner pad */
```

---

## Erratum 2794 — Technical, Held for Document Update

Section Appendix; reported by Kasper Dupont on 2011-05-02.

**Original text:**

```
  key =         "Jefe"
  data =        "what do ya want for nothing?"
  data_len =    28 bytes
  digest =      0x750c783e6ab0b503eaa86e310a5db738
```

**Corrected text:**

```
  key =         "Jefe"
  key_len =     4
  data =        "what do ya want for nothing?"
  data_len =    28 bytes
  digest =      0x750c783e6ab0b503eaa86e310a5db738
```

**Notes:**

```
key_len was omitted from this test vector. The other test vectors specify both key_len and data_len.
```

---

## Erratum 4809 — Technical, Held for Document Update

Section 2; reported by Erdem Memisyazici on 2016-09-23.

**Original text:**

```
Applications that use keys longer
   than B bytes will first hash the key using H and then use the
   resultant L byte string as the actual key to HMAC.
```

**Corrected text:**

```
Applications MUST not use keys longer than B bytes.
```

**Notes:**

```
Using this approach creates an exploitable vulnerability where there are two known K instances, one the hashed key, and the other the key itself.  As shown in the sample Java code below:

    final byte[] keyBytes = KEY.getBytes();
    final byte[] sha1 = HashUtil.sha1(keyBytes);
    final String a = hmac_sha1(keyBytes, TEXT);
    final String b = hmac_sha1(sha1, TEXT);

As demonstrated a equals b.  To cite a real world vulnerability; for all keys longer than B, using password storage configurations which store the hash of the key for integrity checks, and store the key itself in a tamper proof device, there will exist plain text keys stored on both storage systems.  Compromising a hash database should not reveal plain text secrets, which will only be true if an implementation first hashes the key and uses the resultant L byte string as the actual key to HMAC.

I suggest simply not allowing keys longer than B bytes, which will greatly improve the security of the standard.

Verifier notes: I started a thread [1] on the CFRG mailing list to discuss this. My reading of that thread leads me to conclude there there's consensus to not verify the erratum on the basis that the threat isn't that significant and a backwards incompatible change as would be required is not justified. However, if HMAC were to be updated in a manner that didn't require backwards compatibility then one would likely consider this. Hence marking this as "hold for document update"
```

---

## Erratum 3694 — Editorial, Held for Document Update

Section References; reported by Christopher Dearlove on 2013-08-14.

**Original text:**

```
"Keyed Hash Functions and Message Authentication"
```

**Corrected text:**

```
"Keying Hash Functions for Message Authentication"
```

**Notes:**

```
This is reference [BCK1]. It is also no longer directly available at the advertised URL, though can be found on that site. Alternatively it is easily available elsewhere, by searching with the quoted corrected title (which is why this erratum may help).
```

---

## Erratum 4459 — Technical, Rejected

Section Appendix; reported by Bozhi ZHENG on 2015-08-27.

**Original text:**

```
        /* start out by storing key in pads */
        bzero( k_ipad, sizeof k_ipad);
        bzero( k_opad, sizeof k_opad);
        bcopy( key, k_ipad, key_len);
        bcopy( key, k_opad, key_len);

        /* XOR key with ipad and opad values */
        for (i=0; i<64; i++) {
                k_ipad[i] ^= 0x36;
                k_opad[i] ^= 0x5c;
        }
```

**Corrected text:**

```
        /* start out by storing key in pads */
        bzero( k_ipad, sizeof k_ipad);
        bzero( k_opad, sizeof k_opad);
        bcopy( k_ipad, key, key_len);
        bcopy( k_opad, key, key_len);

        /* XOR key with ipad and opad values */
        for (i=0; i<64; i++) {
                k_ipad[i] ^= 0x36;
                k_opad[i] ^= 0x5c;
        }
```

**Notes:**

```
The ipad = the byte 0x36 repeated 64 times, opad = the type 0x5C repeated B times and then ipad and opad XOR K after it appended to 64 byptes.
 --VERIFIER NOTES-- 

The net effect of the suggested change would be to zero the key 
and make HMAC useless.
```

---

## Erratum 8185 — Editorial, Rejected

Section 5; reported by ev on 2024-11-22.

**Original text:**

```
length t be not less than
```

**Corrected text:**

```
length to be not less than
```

**Notes:**

```
typo on what I assume should be 'to'

 --VERIFIER NOTES-- 
We assume that "the output length t" refers to the t bits of output in the previous sentence:

"Applications of HMAC can choose to truncate the output of HMAC by outputting the t leftmost bits of the HMAC computation for some parameter t (namely, the computation is carried in the normal way as defined in section 2 above but the end result is truncated to t bits). We recommend that the output length t be not less than half the length of the hash output (to match the birthday attack bound) and not less than 80 bits (a suitable lower bound on the number of bits that need to be
predicted by an attacker)."
```

---

## Erratum 8296 — Editorial, Rejected

GLOBAL; reported by Andreas Johannessen on 2025-02-14.

**Original text:**

```
[BCK1]  M. Bellare, R. Canetti, and H. Krawczyk,
           "Keyed Hash Functions and Message Authentication",
           Proceedings of Crypto'96, LNCS 1109, pp. 1-15.
           (http://www.research.ibm.com/security/keyed-md5.html)
```

**Corrected text:**

```
[BCK1]  M. Bellare, R. Canetti, and H. Krawczyk,
           "Keyed Hash Functions and Message Authentication",
           Proceedings of Crypto'96, LNCS 1109, pp. 1-15.
```

**Notes:**

```
The link is dead.
 --VERIFIER NOTES-- 
The link was valid at the time of publication.
```

