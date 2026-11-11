/**
 * The session descriptions the parties exchange (RFC 3264 offer/answer, one
 * PCMU audio stream, RFC 4566): the caller's offer on its INVITE, the callee's
 * answer on its 200. The lane rewrites each connection address and media
 * port to the party's own, so the addresses here are the RFC 5737
 * documentation ones.
 */

/** The resource path of the caller's offer. */
export const OFFER = "resources/offer.sdp"

/** The resource path of the callee's answer. */
export const ANSWER = "resources/answer.sdp"

/** The SDP lines a lane rewrites: the connection address and the media port. */
export const REWRITE = ["c=addr", "m=port"] as const

/** A session description from `address`, its media on `port`. */
const sdp = (address: string, port: number): string =>
  [
    "v=0",
    `o=- 1 1 IN IP4 ${address}`,
    "s=-",
    `c=IN IP4 ${address}`,
    "t=0 0",
    `m=audio ${port} RTP/AVP 0`,
    "a=rtpmap:0 PCMU/8000",
    ""
  ].join("\r\n")

/** Each resource by its path. */
export const RESOURCES: Readonly<Record<string, string>> = {
  [OFFER]: sdp("192.0.2.10", 49170),
  [ANSWER]: sdp("192.0.2.20", 49172)
}
