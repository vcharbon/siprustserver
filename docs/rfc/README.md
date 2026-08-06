# Vendored RFCs

Plain-text copies from <https://www.rfc-editor.org/rfc/rfcNNNN.txt> of every
RFC the codebase cites, so they are on hand offline. Grep the codebase for
`rfcNNNN` / `RFC NNNN` to find the citing sites.

## Core SIP

- [rfc3261.txt](rfc3261.txt) — SIP: Session Initiation Protocol
- [rfc3262.txt](rfc3262.txt) — Reliability of Provisional Responses in SIP (PRACK)
- [rfc3263.txt](rfc3263.txt) — Locating SIP Servers
- [rfc3264.txt](rfc3264.txt) — An Offer/Answer Model with SDP
- [rfc6026.txt](rfc6026.txt) — Correct Transaction Handling for 2xx Responses to INVITE (updates 3261's transaction layer, which `sip-txn` implements; not yet cited in-code)

## SIP methods & header extensions

- [rfc3311.txt](rfc3311.txt) — The SIP UPDATE Method
- [rfc3325.txt](rfc3325.txt) — Private Extensions for Asserted Identity (P-Asserted-Identity)
- [rfc3326.txt](rfc3326.txt) — The Reason Header Field
- [rfc3420.txt](rfc3420.txt) — Internet Media Type message/sipfrag
- [rfc3515.txt](rfc3515.txt) — The SIP REFER Method
- [rfc3581.txt](rfc3581.txt) — Symmetric Response Routing (rport)
- [rfc3891.txt](rfc3891.txt) — The SIP "Replaces" Header
- [rfc4028.txt](rfc4028.txt) — Session Timers in SIP
- [rfc4412.txt](rfc4412.txt) — Communications Resource Priority (Resource-Priority)
- [rfc5009.txt](rfc5009.txt) — P-Header for Authorization of Early Media (P-Early-Media)
- [rfc5806.txt](rfc5806.txt) — Diversion Indication in SIP
- [rfc6442.txt](rfc6442.txt) — Location Conveyance for SIP
- [rfc6665.txt](rfc6665.txt) — SIP-Specific Event Notification (obsoletes 3265)
- [rfc7044.txt](rfc7044.txt) — Request History Information (History-Info)
- [rfc7433.txt](rfc7433.txt) — Transporting User-to-User Call Control Information

## URIs

- [rfc3966.txt](rfc3966.txt) — The tel URI for Telephone Numbers
- [rfc3986.txt](rfc3986.txt) — Uniform Resource Identifier (URI): Generic Syntax

## Test material

- [rfc4475.txt](rfc4475.txt) — SIP Torture Test Messages
- [rfc5118.txt](rfc5118.txt) — SIP Torture Test Messages for IPv6
- [rfc5407.txt](rfc5407.txt) — Example Call Flows of Race Conditions in SIP

## SDP & media

- [rfc4566.txt](rfc4566.txt) — SDP: Session Description Protocol
- [rfc3550.txt](rfc3550.txt) — RTP: A Transport Protocol for Real-Time Applications
- [rfc3551.txt](rfc3551.txt) — RTP Profile for Audio and Video Conferences
- [rfc5761.txt](rfc5761.txt) — Multiplexing RTP Data and Control Packets on a Single Port (rtcp-mux)
- [rfc5022.txt](rfc5022.txt) — Media Server Control Markup Language (MSCML)

## Authentication & crypto

- [rfc2104.txt](rfc2104.txt) — HMAC: Keyed-Hashing for Message Authentication
- [rfc2617.txt](rfc2617.txt) — HTTP Authentication: Basic and Digest
- [rfc4868.txt](rfc4868.txt) — HMAC-SHA-256/384/512 (truncation profile used by the proxy cookie HMAC)
- [rfc7616.txt](rfc7616.txt) — HTTP Digest Access Authentication
- [rfc8760.txt](rfc8760.txt) — SIP Digest Access Authentication Scheme

## Networking

- [rfc791.txt](rfc791.txt) — Internet Protocol (IPv4 reassembly keys in `sip-pcap`)

## Deliberately not vendored

- RFC 2543 (original SIP) — obsoleted by 3261; cited only as history.
- RFC 3265 (SIP events) — obsoleted by 6665, which is vendored.
