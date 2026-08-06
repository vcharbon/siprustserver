# Errata for RFC 791

Applies to [rfc791.txt](rfc791.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc791>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Verified | 8 |
| Held for Document Update | 8 |
| Reported | 1 |
| Rejected | 3 |

---

## Erratum 716 — Technical, Verified

Section 3.1; reported by Damien Mattei on 2007-01-03.

**Original text:**

```
        +--------+--------+--------+--------+
        |10001000|00000010|    Stream ID    |
        +--------+--------+--------+--------+
         Type=136 Length=4
```

**Corrected text:**

```
        +--------+--------+--------+--------+
        |10001000|00000100|    Stream ID    |
        +--------+--------+--------+--------+
         Type=136 Length=4

Rationale:

This number count the length which is 4 and not 2.
10 in binary is 2 in decimal, 100 in binary is 4 in decimal.

The option-length octet counts the option-type octet and the 
option-length octet as well as the option-data octets.(see page 15)
The length is 4 for the Stream identifier option as we have 4 bytes and 
it is well written in page 16 of RFC 791:

The following internet options are defined:

      CLASS NUMBER LENGTH DESCRIPTION
      ----- ------ ------ -----------
        0     0      -    End of Option list.  This option occupies only
                          1 octet; it has no length octet.
        0     1      -    No Operation.  This option occupies only 1
                          octet; it has no length octet.
        0     2     11    Security.  Used to carry Security,
                          Compartmentation, User Group (TCC), and
                          Handling Restriction Codes compatible with DOD
                          requirements.
        0     3     var.  Loose Source Routing.  Used to route the
                          internet datagram based on information
                          supplied by the source.
        0     9     var.  Strict Source Routing.  Used to route the
                          internet datagram based on information
                          supplied by the source.
        0     7     var.  Record Route.  Used to trace the route an
                          internet datagram takes.
        0     8      4    Stream ID.  Used to carry the stream
                          identifier.
        2     4     var.  Internet Timestamp.
```

**Notes:**

```
from pending
```

---

## Erratum 6356 — Technical, Verified

Section 3.2; reported by Patrick Ni on 2020-12-15.

**Original text:**

```
(14) THEN TL <- TDL+(IHL*4)
```

**Corrected text:**

```
(14) THEN TL <- TDL+(IHL-Of-First-Fragment*4)
```

**Notes:**

```
IHL could be different between the first fragment and the rest. Only the first fragment's IHL is the same as the one in the original datagram before fragmentation

--- Verifier note ---
Updated the type of errata to technical from editorial.

Section 3.2 of RFC 791 clearly states that "When fragmentation occurs, some options are copied, but others remain with the first fragment only." so IHL varies from fragment to fragment. Therefore when copying the IP header of the F=0 fragment (step 11) all options are rightfully copied and must be counted in the re-assembled fragment total length on step 14 as noted by Patrick Ni.
```

---

## Erratum 579 — Editorial, Verified

Section On page 21, it says; reported by Pavel Uvarov on 2004-06-16.

**Original text:**

```
The intitial contents of the route data area must be zero.
```

**Corrected text:**

```
The initial contents of the route data area must be zero.
```

---

## Erratum 583 — Editorial, Verified

Section On page 23, it says; reported by Pavel Uvarov on 2004-06-16.

**Original text:**

```
The intitial contents of the timestamp data area must be zero 
or internet address/zero pairs.
```

**Corrected text:**

```
The initial contents of the timestamp data area must be zero 
or internet address/zero pairs.
```

**Notes:**

```
Spelling error.
```

---

## Erratum 5037 — Editorial, Verified

Section GLOSSARY; reported by Prabhu K Lokesh on 2017-06-10.

**Original text:**

```
NFB
          The Number of Fragment Blocks in a the data portion of an
          internet fragment.  That is, the length of a portion of data
          measured in 8 octet units.
```

**Corrected text:**

```
NFB
          The Number of Fragment Blocks in the data portion of an
          internet fragment.  That is, the length of a portion of data
          measured in 8-octet units.
```

**Notes:**

```
Extra article 'a' before the term 'data portion of an internet fragment'.
```

---

## Erratum 6184 — Editorial, Verified

Section 3.2; reported by Ye Shu on 2020-05-21.

**Original text:**

```
The fragmentation strategy is designed so than an unfragmented datagram
has all zero fragmentation information
```

**Corrected text:**

```
The fragmentation strategy is designed so that an unfragmented datagram
has all zero fragmentation information
```

**Notes:**

```
typo: so than => so that
```

---

## Erratum 7560 — Editorial, Verified

Section 3.1, Line 904; reported by Simon Günther on 2023-07-04.

**Original text:**

```
Bits   5:  0 = Normal Relibility, 1 = High Relibility.
```

**Corrected text:**

```
Bits   5:  0 = Normal Reliability, 1 = High Reliability.
```

**Notes:**

```
Typo
```

---

## Erratum 7561 — Editorial, Verified

Section 3.2; reported by Fernando Gont on 2023-07-10.

**Original text:**

```
  Identification

    The choice of the Identifier for a datagram is based on the need to
    provide a way to uniquely identify the fragments of a particular
    datagram.  The protocol module assembling fragments judges fragments
    to belong to the same datagram if they have the same source,
    destination, protocol, and Identifier.  Thus, the sender must choose
    the Identifier to be unique for this source, destination pair and
    protocol for the time the datagram (or any fragment of it) could be
    alive in the internet.

    It seems then that a sending protocol module needs to keep a table
    of Identifiers, one entry for each destination it has communicated
    with in the last maximum packet lifetime for the internet.

    However, since the Identifier field allows 65,536 different values,
    some host may be able to simply use unique identifiers independent
    of destination.
```

**Corrected text:**

```
  Identification

    The choice of the Identification for a datagram is based on the need to
    provide a way to uniquely identify the fragments of a particular
    datagram.  The protocol module assembling fragments judges fragments
    to belong to the same datagram if they have the same source,
    destination, protocol, and Identification.  Thus, the sender must choose
    the Identification to be unique for this source, destination pair and
    protocol for the time the datagram (or any fragment of it) could be
    alive in the internet.

    It seems then that a sending protocol module needs to keep a table
    of Identification values, one entry for each destination it has communicated
    with in the last maximum packet lifetime for the internet.

    However, since the Identification field allows 65,536 different values,
    some host may be able to simply use unique identifiers independent
    of destination.
```

**Notes:**

```
The field is called "Identification", and not "Identifier" (please note the capitalization in the text).
```

---

## Erratum 4752 — Technical, Held for Document Update

Section 3.1.; reported by Markus Klemm on 2016-07-29.

**Original text:**

```
The Option Length is the number of octets 
in the option counting the type, length, 
pointer, and overflow/flag octets (maximum length 40).
```

**Corrected text:**

```
The Option Length is the number of octets 
in the option counting the type, length, 
pointer, overflow/flag octets 
and timestamp area (maximum length 40).
```

**Notes:**

```
The original version implies that the length is only the sum of the constant fields, in this case, 4. This is in conflict with
A) All prior statements that the length is variable
B) The pointer with it >=5 constraint, implies the timestamp area is always full, which is the case when pointer > length
C) Any need to know how much timestamps will follow
```

---

## Erratum 578 — Editorial, Held for Document Update

Section 3.2; reported by Yin Shuming on 2006-02-18.

**Original text:**

```
Note that this is consistent with the the datagram total length field (of 
course, the header is counted in the total length and not in the 
fragments).
```

**Corrected text:**

```
Note that this is consistent with the datagram total length field (of 
course, the header is counted in the total length and not in the 
fragments).
```

---

## Erratum 2294 — Editorial, Held for Document Update

Section Page 21; reported by Vishwas Manral on 2010-06-03.

**Original text:**

```
If it is, it inserts its
own internet address as known in the environment into which this
datagram is being forwarded into the recorded route begining at
the octet indicated by the pointer, and increments the pointer
by four.
```

**Corrected text:**

```
If it is, it inserts its
own internet address as known in the environment into which this
datagram is being forwarded into the recorded route beginning at
the octet indicated by the pointer, and increments the pointer
by four.
```

**Notes:**

```
s/begining/beginning/g
```

---

## Erratum 2295 — Editorial, Held for Document Update

Section Page 27; reported by Vishwas Manral on 2010-06-03.

**Original text:**

```
For example, one could implement
a fragmentation procedure that repeatly divided large datagrams in
half until the resulting fragments were less than the maximum
transmission unit size.
```

**Corrected text:**

```
For example, one could implement
a fragmentation procedure that repeatedly divided large datagrams in
half until the resulting fragments were less than the maximum
transmission unit size.
```

**Notes:**

```
s/ repeatly/repeatedly/
```

---

## Erratum 2519 — Editorial, Held for Document Update

Section 3.1; reported by Yaakov (J) Stein on 2010-09-14.

**Original text:**

```
Total Length is the length of the datagram, measured in octets,
including internet header and data.  
```

**Corrected text:**

```
Total Length is the length of the datagram or fragment, measured in octets,
including internet header and data.  
```

**Notes:**

```
Section 2.3 makes it clear that during fragmentation the total length field is corrected to the length of the fragment. Wording such as "break a datagram into an almost arbitrary number of pieces" implies that "datagram" means the entire original packet. Thus, without the proposed correction, one may be led to believe that the total length contains the length of the original datagram.
```

---

## Erratum 3074 — Editorial, Held for Document Update

Section 3.1 Page 12; reported by ChengYan Wang on 2012-01-04.

**Original text:**

```
Bits   5:  0 = Normal Relibility, 1 = High Relibility.
```

**Corrected text:**

```
Bits   5:  0 = Normal Reliability, 1 = High Reliability.
```

**Notes:**

```
Spelling error.
```

---

## Erratum 4126 — Editorial, Held for Document Update

Section 2.3; reported by Federico do Pino on 2014-10-04.

**Original text:**

```
    If internet datagram marked don't fragment cannot be
    delivered to its destination without fragmenting it, it is to be
    discarded instead.
```

**Corrected text:**

```
    If an internet datagram marked don't fragment cannot be
    delivered to its destination without fragmenting it, it is to be
    discarded instead.
```

**Notes:**

```
Grammar mistake.
```

---

## Erratum 5679 — Editorial, Held for Document Update

Section 3.1; reported by Greg Skinner on 2019-03-30.

**Original text:**

```
The time is measured in units of seconds, but since every module that
processes a datagram must decrease the TTL by at least one even if it
process the datagram in less than a second, the TTL must be thought of
only as an upper bound on the time a datagram may exist.
```

**Corrected text:**

```
The time is measured in units of seconds, but since every module that
processes a datagram must decrease the TTL by at least one even if it
processes the datagram in less than a second, the TTL must be thought
of only as an upper bound on the time a datagram may exist.
```

**Notes:**

```
Grammar mistake (s/process/processes)
```

---

## Erratum 8691 — Technical, Reported

Section 1.3; reported by Alan Shultz on 2026-01-01.

**Original text:**

```
The ARPANET address would be derived from the internet address by the local network interface
```

**Corrected text:**

```
The ARPANET address would be derived from the internet address by the internet module
```

**Notes:**

```
This sentence claims the local net interface translates an IP address into a local net address. The local net interface knows nothing about internet addresses. This translation is the responsibility of layers above the local net.

The later section 2.3, subsection Addressing, makes this explicit:

>The internet module maps internet addresses to local net addresses.
```

---

## Erratum 3874 — Technical, Rejected

Section 3; reported by Bo-Jhang Ho on 2014-01-23.

**Original text:**

```
    The number 576 is selected to allow a reasonable sized data block to
    be transmitted in addition to the required header information.  For
    example, this size allows a data block of 512 octets plus 64 header
    octets to fit in a datagram.  The maximal internet header is 60
    octets, and a typical internet header is 20 octets, allowing a
    margin for headers of higher level protocols.
```

**Corrected text:**

```
    The number 576 is selected to allow a reasonable sized data block to
    be transmitted in addition to the required header information.  For
    example, this size allows a data block of 516 octets plus 60 header
    octets to fit in a datagram.  The maximal internet header is 60
    octets, and a typical internet header is 20 octets, allowing a
    margin for headers of higher level protocols.
```

**Notes:**

```
It is not consistent that it first give an example which illustrates the header is 64 octets, but then explains the maximum header size is 60 octets.
 --VERIFIER NOTES-- 
I believe that the discrepancy you are seeing is because in the one case the text is referring to the set of all headers, and in the other case it's referring to the IP header alone.   The IP header does have a maximum size of 60 octets, but for example an IP header plus a UDP header would be 28 octets, and IP+TCP would be 40 octets, plus a timestamp header in current practice.   Notice the "for example" at the beginning of the sentence that mentions 64 header octets.
```

---

## Erratum 5038 — Technical, Rejected

Section 3.1; reported by Prabhu K Lokesh on 2017-06-10.

**Original text:**

```
The option begins with the option type code.  The second octet
        is the option length which includes the option type code and the
        length octet, the pointer octet, and length-3 octets of route
        data.
```

**Corrected text:**

```
The option begins with the option type code.  The second octet
        is the option length, measured in octets, including the option 
        type code octet, the length octet, the pointer octet, and the
        octets of route data.
```

**Notes:**

```
The way the second octet is defined, although readable to majority of the audience with no concern, will incorrectly equate to:
length = 'option type code octet' + 'length octet + 'pointer octet' + length - 3 octet of route data

So, what is the value of 'length' in 'length - 3'; when reader encounters 'length - 3', the term 'length' itself is not completely defined. Using the term 'length - 3' to define the term 'length' may not be very appropriate. 

For better readability, we can simply write -
The second octet is the option length, measured in octets, including the option type code octet, the length octet, the pointer octet, and the octets of route data.

The same correction applies to the following sections -
3.1.  Internet Header Format > Loose Source and Record Route
3.1.  Internet Header Format > Strict Source and Record Route
3.1.  Internet Header Format > Record Route
 --VERIFIER NOTES-- 
While the text can be simplified as the submitter of the Erratum implies, the current text has been in use for close to 4 decades without any interoperability issues rising out of it. Hence this
```

---

## Erratum 6677 — Editorial, Rejected

Section 3.1; reported by Øyvind Bolme Fredriksen on 2021-09-06.

**Original text:**

```
Protocol:  8 bits

    This field indicates the next level protocol used in the data
    portion of the internet datagram.  The values for various protocols
    are specified in "Assigned Numbers" [9].
```

**Corrected text:**

```
Protocol:  8 bits

    This field indicates the next upper level protocol used in the data
    portion of the internet datagram.  The values for various protocols
    are specified in "Assigned Numbers" [9], section ASSIGNED INTERNET PROTOCOL NUMBERS.
```

**Notes:**

```
The word 'next' is ambiguous in the sense that it does not indicate whether the 'next' protocol is at the next LOWER or UPPER level (referring to Fig. 1). Although it may be obvious to people well versed in this domain that the next UPPER level protocol is meant, as a newcomer I had to think twice to reach at this conclusion.

Also, the reference to [9] could be more specific.
 --VERIFIER NOTES-- 
The description of the Protocol field states that it refers to the protocol "used in the data portion of the internet datagram".  In the context of Figure 1, this cannot refer to a lower layer protocol.
```

