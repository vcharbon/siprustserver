/**
 * A flows document whose bodies are bytes: the head-body payload arm, as the
 * extractor writes a datagram whose body is not UTF-8. The bodies are those of
 * an INFO transfer: a multipart INVITE carrying an SDP beside a binary
 * `application/vnd.sample.indata` part, then an INFO carrying indata alone.
 */

/** 17 bytes, several at or above 0x80: never a UTF-8 string. */
export const INDATA_HEX = "77 0f 47 00 83 0b 83 04 03 90 03 01 8a 01 34 88 00"

export const SDP = [
  "v=0",
  "o=- 1 1 IN IP4 10.0.0.1",
  "s=-",
  "c=IN IP4 10.0.0.1",
  "t=0 0",
  "m=audio 4000 RTP/AVP 8",
  ""
].join("\r\n")

/** `--unique-boundary-1` framing, the SDP at 54, the indata at 291: 333 bytes. */
export const MULTIPART_BODY_B64 =
  "LS11bmlxdWUtYm91bmRhcnktMQ0KQ29udGVudC1UeXBlOiBhcHBsaWNhdGlvbi9zZHANCg0Kdj0wDQpvPS0gMSAxIElOIElQNCAxMC4w" +
  "LjAuMQ0Kcz0tDQpjPUlOIElQNCAxMC4wLjAuMQ0KdD0wIDANCm09YXVkaW8gNDAwMCBSVFAvQVZQIDgNCg0KLS11bmlxdWUtYm91bmRh" +
  "cnktMQ0KQ29udGVudC1UeXBlOiBhcHBsaWNhdGlvbi92bmQuc2FtcGxlLmluZGF0YQ0KQ29udGVudC1UcmFuc2Zlci1FbmNvZGluZzog" +
  "YmluYXJ5DQpDb250ZW50LURpc3Bvc2l0aW9uOiBzaWduYWw7aGFuZGxpbmc9b3B0aW9uYWwNCg0Kdw9HAIMLgwQDkAMBigE0iAANCi0t" +
  "dW5pcXVlLWJvdW5kYXJ5LTEtLQ0K"

/** 320 bytes of head, terminated by the blank line. */
export const INVITE_HEAD = [
  "INVITE sip:+155500000000900004@example.test SIP/2.0",
  "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1",
  "From: <sip:0009001@example.test>;tag=abc",
  "To: <sip:+155500000000900004@example.test>",
  "Call-ID: call-a",
  "CSeq: 1 INVITE",
  "Content-Type: multipart/mixed;boundary=unique-boundary-1",
  "MIME-Version: 1.0",
  "Content-Length: 333",
  "",
  ""
].join("\r\n")

/** 18 bytes of indata: `76 10 47 00 81 0c 81 01 05 83 07 83 10 65 31 53 87 08`. */
export const INFO_BODY_B64 = "dhBHAIEMgQEFgweDEGUxU4cI"

/** 291 bytes of head. */
export const INFO_HEAD = [
  "INFO sip:+155500000000900004@example.test SIP/2.0",
  "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK2",
  "From: <sip:0009001@example.test>;tag=abc",
  "To: <sip:+155500000000900004@example.test>;tag=xyz",
  "Call-ID: call-a",
  "CSeq: 2 INFO",
  "Content-Type: application/vnd.sample.indata",
  "Content-Length: 18",
  "",
  ""
].join("\r\n")

export const headBodyDocument = {
  schema: 5,
  legs: [
    {
      call_id: "call-a",
      final_status: 200,
      terminated_by: "BYE",
      msgs: [
        {
          ts_us: 1_000_000,
          src: "10.0.0.1:5060",
          dst: "10.0.0.2:5060",
          hop: 0,
          retx: false,
          head: INVITE_HEAD,
          body_b64: MULTIPART_BODY_B64,
          summary: {
            kind: "request",
            method: "INVITE",
            uri: "sip:+155500000000900004@example.test",
            cseq: { seq: 1, method: "INVITE" },
            from: { tag: "abc" }
          },
          body: {
            content_type: "multipart/mixed",
            len: 333,
            parts: [
              { content_type: "application/sdp", offset: 54, len: 85 },
              {
                content_type: "application/vnd.sample.indata",
                headers: [
                  { name: "Content-Transfer-Encoding", value: "binary" },
                  { name: "Content-Disposition", value: "signal;handling=optional" }
                ],
                offset: 291,
                len: 17
              }
            ]
          }
        },
        {
          ts_us: 1_500_000,
          src: "10.0.0.1:5060",
          dst: "10.0.0.2:5060",
          hop: 0,
          retx: false,
          head: INFO_HEAD,
          body_b64: INFO_BODY_B64,
          summary: {
            kind: "request",
            method: "INFO",
            uri: "sip:+155500000000900004@example.test",
            cseq: { seq: 2, method: "INFO" },
            from: { tag: "abc" },
            to: { tag: "xyz" }
          },
          body: { content_type: "application/vnd.sample.indata", len: 18 }
        }
      ]
    }
  ],
  groups: [{ legs: [0], t0_us: 1_000_000, final_status: 200 }]
}

/** The indata part of the INVITE, dumped: two lines, sixteen bytes then one. */
export const INDATA_DUMP = [
  "0000  77 0f 47 00 83 0b 83 04  03 90 03 01 8a 01 34 88  |w.G...........4.|",
  "0010  00                                                |.|"
].join("\n")

/** The INFO's body, dumped. */
export const INFO_DUMP = [
  "0000  76 10 47 00 81 0c 81 01  05 83 07 83 10 65 31 53  |v.G..........e1S|",
  "0010  87 08                                             |..|"
].join("\n")
