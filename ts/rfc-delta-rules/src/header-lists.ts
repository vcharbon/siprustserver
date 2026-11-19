/**
 * The header lists the generic rows name a family by, lowercase, each once
 * beside its rows. A list cites the clause that defines its members, or says
 * it is a grouping where no one clause names them together. A deployment row
 * that names one of these families imports the list from here.
 */

/**
 * A grouping: the headers a stack composes from its own transaction and dialog
 * state on a message it originates or answers itself. The routing and
 * sequencing set (RFC 3261 §8.1.1), the dialog's From/To (§12.2.1.1) and the
 * stack's own Contact (§8.1.1.8). The keepalive exchange's
 * `keepalive-exchange-locally-terminated` reads it.
 */
export const STACK_COMPOSED = [
  "via",
  "call-id",
  "cseq",
  "from",
  "to",
  "max-forwards",
  "content-length",
  "contact",
  "route",
  "record-route"
]

/**
 * A grouping: the advertisements in which a sender states its own
 * capabilities. Accept, Accept-Encoding, Accept-Language, Allow and Supported
 * (RFC 3261 §20.1, §20.2, §20.3, §20.5, §20.37) and Allow-Events (RFC 6665
 * §8.2.2).
 */
export const ADVERTISEMENT = ["allow", "allow-events", "supported", "accept", "accept-encoding", "accept-language"]

/**
 * A grouping: the identity assertions one network element makes to the next.
 * P-Asserted-Identity and P-Preferred-Identity (RFC 3325 §9), the Identity
 * header (RFC 8224 §4). A deployment whose network asserts identity in further
 * headers of its own extends the list for its rows.
 */
export const IDENTITY_ASSERTION = ["p-asserted-identity", "p-preferred-identity", "identity"]

/**
 * The product banner a stack stamps on its own egress, User-Agent and Server
 * (RFC 3261 §20.41, §20.35): an opaque string naming the stack that sent the
 * message, never a fact about the call.
 */
export const PRODUCT_IDENTITY = ["user-agent", "server"]

/** The session timer's interval and its floor, Session-Expires and Min-SE (RFC 4028 §4, §5). */
export const SESSION_TIMER = ["session-expires", "min-se"]

/**
 * The descriptors of a message body: Content-Disposition, Content-Encoding,
 * Content-Language and MIME-Version (RFC 3261 §20.11, §20.12, §20.13, §20.24).
 */
export const BODY_METADATA = ["content-disposition", "content-encoding", "content-language", "mime-version"]
