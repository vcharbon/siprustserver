# Errata for RFC 3550

Applies to [rfc3550.txt](rfc3550.txt). Snapshot of the RFC Editor errata
database (<https://www.rfc-editor.org/errata/rfc3550>) as of 2026-08-06.
Sorted by status (Verified first), then Technical before Editorial.

| Status | Count |
|---|---|
| Held for Document Update | 2 |
| Rejected | 2 |

---

## Erratum 3263 — Technical, Held for Document Update

Section 6.4.1; reported by Pieter Demuytere on 2012-06-18.

**Original text:**

```
cumulative number of packets lost: 24 bits
   The total number of RTP data packets from source SSRC_n that have
   been lost since the beginning of reception.  This number is
   defined to be the number of packets expected less the number of
   packets actually received, where the number of packets received
   includes any which are late or duplicates.  Thus, packets that
   arrive late are not counted as lost, and the loss may be negative
   if there are duplicates.  The number of packets expected is
   defined to be the extended last sequence number received, as
   defined next, less the initial sequence number received.  This may
   be calculated as shown in Appendix A.3.
```

**Corrected text:**

```
cumulative number of packets lost: 24 bits 
   The total number of RTP data packets from source SSRC_n that have 
   been lost since the beginning of reception. This number is 
   defined to be the number of packets expected less the number of 
   packets actually received, where the number of packets received 
   includes any which are late or duplicates. Thus, packets that 
   arrive late are not counted as lost, and the loss may be negative 
   if there are duplicates. The number of packets expected is 
   defined to be the extended highest sequence number received, as 
   defined next, less the initial sequence number received. This may 
   be calculated as shown in Appendix A.3.
```

**Notes:**

```
Changed 

The number of packets expected is defined to be the extended last sequence number received...

Into

The number of packets expected is defined to be the extended highest sequence number received...
```

---

## Erratum 4770 — Editorial, Held for Document Update

Section 3.; reported by Petr Vaněk on 2016-08-08.

**Original text:**

```
   RTP session: An association among a set of participants
      communicating with RTP.  A participant may be involved in multiple
      RTP sessions at the same time.  In a multimedia session, each
      medium is typically carried in a separate RTP session with its own
      RTCP packets unless the the encoding itself multiplexes multiple
      media into a single data stream.  A participant distinguishes
      multiple RTP sessions by reception of different sessions using
      different pairs of destination transport addresses, where a pair
      of transport addresses comprises one network address plus a pair
      of ports for RTP and RTCP.  All participants in an RTP session may
      share a common destination transport address pair, as in the case
      of IP multicast, or the pairs may be different for each
      participant, as in the case of individual unicast network
      addresses and port pairs.  In the unicast case, a participant may
      receive from all other participants in the session using the same
      pair of ports, or may use a distinct pair of ports for each.
```

**Corrected text:**

```
   RTP session: An association among a set of participants
      communicating with RTP.  A participant may be involved in multiple
      RTP sessions at the same time.  In a multimedia session, each
      medium is typically carried in a separate RTP session with its own
      RTCP packets unless the encoding itself multiplexes multiple
      media into a single data stream.  A participant distinguishes
      multiple RTP sessions by reception of different sessions using
      different pairs of destination transport addresses, where a pair
      of transport addresses comprises one network address plus a pair
      of ports for RTP and RTCP.  All participants in an RTP session may
      share a common destination transport address pair, as in the case
      of IP multicast, or the pairs may be different for each
      participant, as in the case of individual unicast network
      addresses and port pairs.  In the unicast case, a participant may
      receive from all other participants in the session using the same
      pair of ports, or may use a distinct pair of ports for each.
```

**Notes:**

```
typo: double the in 5th line.
```

---

## Erratum 3914 — Technical, Rejected

Section A.1; reported by Hani Mustafa on 2014-03-06.

**Original text:**

```
      init_seq(s, seq);
      s->max_seq = seq - 1;
      s->probation = MIN_SEQUENTIAL;
```

**Corrected text:**

```
      init_seq(s, seq);
      s->max_seq = seq == 0 ? seq : seq - 1;
      s->probation = MIN_SEQUENTIAL;
```

**Notes:**

```
If the first RTP packet has a sequence number of 0, the logic will cause cycles to increase by 1, which will affect "expected number of received packets" calculations.
 --VERIFIER NOTES-- 
Submitter requested rejection.
```

---

## Erratum 4192 — Technical, Rejected

Section 6.4.1; reported by Julius Friedman on 2014-12-03.

**Original text:**

```
sender's octet count: 32 bits
      The total number of payload octets (i.e., not including header or
      padding) transmitted in RTP data packets by the sender since
      starting transmission up until the time this SR packet was
      generated.  The count SHOULD be reset if the sender changes its
      SSRC identifier.  This field can be used to estimate the average
      payload data rate.
```

**Corrected text:**

```
sender's octet count: 32 bits
      The total number of payload octets 
      transmitted in RTP data packets by the sender since
      starting transmission up until the time this SR packet was
      generated.  The count SHOULD be reset if the sender changes its
      SSRC identifier.  This field can be used to estimate the average
      payload data rate.
```

**Notes:**

```
Where as payload octets is defined as the total number of data octets contained in a Rtp Packet minus the 12 Header octets for Rtp Packets.

Padding octets as well as octets which occur in the contributing source list should also be included as they may differ on a per packet basis and would make the total calculation invalid.

During TCP communication any application layer header should NOT be included in the total bytes count when including the header length.

Any Rtcp packet counters should include the total length of the packet (header, padding and any other data).
 --VERIFIER NOTES-- 
   Rejected based on discussion in avtcore
```

