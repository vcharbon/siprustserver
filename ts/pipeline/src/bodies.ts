/**
 * Message-body decomposition into a pivot `body` plus the sibling resource files
 * the runner replays, or — on an expect — confronts the received body with.
 *
 * Registry policy: `application/sdp` carries the `c=` / `m=` rewrites (ports
 * rebooked at replay); a known non-SDP type is frozen byte-exact under its
 * captured media type; anything unrecognized is frozen and flagged when it
 * carries number-like digits, so a new handler is a decision and not an
 * omission. The expect side reads the same registry: a frozen TEXT body is
 * stored and asserted by content, SDP and multipart by shape, absence as its
 * own claim, and a binary payload stays undeclared — the recording it would be
 * confronted with is text.
 *
 * A multipart body is decomposed only where extraction handed the parts over
 * (`./parts.ts`). This module owns the per-part HANDLING, never the split.
 *
 * A content type is STORED verbatim, parameters included, and the registry is
 * looked up on the bare type: the stored value is what emission writes back, so
 * a `charset=` the capture carried survives the round trip.
 */
import type { Body, Case, Flows } from "@sip/contracts"
import type { Decomposed, Part } from "./parts.js"
import { body as wireBody, headersInOrder } from "./wire.js"

export interface ResourceFile {
  /** Path relative to the case dir (`resources/<name>`). */
  readonly relPath: string
  readonly text: string
  /** `text` holds latin1-encoded bytes and must be written back that way. */
  readonly binary?: true
}

/** What a step states about its body, and the files that statement references. */
export interface StoredBody {
  /** The body the step states, and `undefined` where it states none. */
  readonly body: Body.Body | undefined
  readonly resources: ReadonlyArray<ResourceFile>
  readonly flags: ReadonlyArray<Case.Flag>
}

export interface BodyResult extends StoredBody {
  /** A multipart body extraction did not decompose — nothing was emitted. */
  readonly undecomposed: boolean
}

/** Per-content-type handling: what the pivot says about a body or a part. */
interface Handling {
  readonly rewrite?: Array<string>
  readonly mode?: Body.BodyMode
  /** Frozen or unrecognized: a flag is due when the payload looks numeric. */
  readonly unrecognized?: true
}

const handlerFor = (contentType: string): Handling => {
  const head = mimeKey(contentType)
  if (head === "application/sdp") return { rewrite: ["c=addr", "m=port"] }
  if (head === "application/emergencycalldata.ecall.msd") return { mode: "frozen-binary" }
  if (
    head.endsWith("+xml") ||
    head.startsWith("application/emergencycalldata.") ||
    head.startsWith("text/")
  ) {
    return { mode: "frozen" }
  }
  return { mode: "frozen", unrecognized: true }
}

const NOTHING: BodyResult = { body: undefined, resources: [], flags: [], undecomposed: false }

export const decompose = (m: Flows.Msg, slug: string, parts?: Decomposed): BodyResult => {
  const payload = wireBody(m)
  if (!payload) return NOTHING
  if (payload.boundary !== undefined) {
    if (!parts) return { ...NOTHING, undecomposed: true }
    return multipart(m, slug, parts, payload.containerType ?? parts.contentType)
  }
  const relPath = resourceName(slug, 0, payload.mediaType)
  const h = handlerFor(payload.mediaType)
  return {
    // The content type rides VERBATIM, parameters included, because emission
    // puts it back as the message's own `Content-Type` (§8.3). Bare
    // `application/sdp` is the one value left unstated: render derives exactly
    // it, so the stored and derived values cannot drift.
    body: {
      ref: relPath,
      ...(h.rewrite ? { rewrite: h.rewrite } : {}),
      ...(payload.contentType === "application/sdp"
        ? {}
        : { "content-type": payload.contentType }),
      ...(h.mode ? { mode: h.mode } : {})
    },
    resources: [{ relPath, text: payload.text, ...(payload.binary ? { binary: true as const } : {}) }],
    flags: numericFlag(h, payload.text, `${slug} body is ${payload.contentType}`),
    undecomposed: false
  }
}

const multipart = (m: Flows.Msg, slug: string, d: Decomposed, container: string): BodyResult => {
  const cids = referencedCids(m)
  const out: Array<Body.Part> = []
  const resources: Array<ResourceFile> = []
  const flags: Array<Case.Flag> = []
  d.parts.forEach((p, n) => {
    const relPath = resourceName(slug, n, p.contentType)
    const h = handlerFor(p.contentType)
    out.push({
      // Verbatim, parameters included: the composer writes this line back as
      // the part's own `Content-Type`, so a `charset=` the capture carried is
      // a byte the replay owes (§8.3).
      "content-type": p.contentType,
      ref: relPath,
      ...(h.rewrite ? { rewrite: h.rewrite } : {}),
      ...(h.mode ? { mode: h.mode } : {}),
      // The part replays under its own id and its own entity headers (§8.3):
      // both are stated where the capture carried them, and omitted where it
      // did not.
      ...(p.contentId === undefined ? {} : { "content-id": p.contentId }),
      ...(p.headers === undefined || p.headers.length === 0
        ? {}
        : { headers: p.headers.map((x) => ({ name: x.name, value: x.value })) }),
      ...(cidLinks(p, cids).length > 0 ? { "cid-linked": cidLinks(p, cids) } : {})
    })
    resources.push({ relPath, text: p.text, binary: true })
    flags.push(...numericFlag(h, p.text, `part ${n} of ${slug} is ${p.contentType}`))
  })
  return {
    // The container keeps every parameter it carried EXCEPT `boundary`, which
    // emission derives from the parts (§8.3).
    body: { multipart: { "content-type": container, parts: out } },
    resources,
    flags,
    undecomposed: false
  }
}

/**
 * Header names (lowercased) whose `cid:` reference names this part — the links
 * the runner keeps tier-1 when it renumbers the body.
 */
const cidLinks = (p: Part, cids: ReadonlyArray<readonly [string, string]>): Array<string> => {
  const id = (p.contentId ?? "").replace(/^</, "").replace(/>$/, "")
  if (id === "") return []
  return [...new Set(cids.filter(([, cid]) => cid === id).map(([name]) => name))]
}

/** `(header-name-lower, cid)` for every `cid:` reference the message makes. */
const referencedCids = (m: Flows.Msg): Array<readonly [string, string]> =>
  headersInOrder(m).flatMap((h) => {
    const at = h.value.indexOf("cid:")
    if (at < 0) return []
    const cid = (h.value.slice(at + 4).split(/[>;,\s]/)[0] ?? "").trim()
    return cid === "" ? [] : [[h.name.toLowerCase(), cid] as const]
  })

/**
 * An unrecognized payload carrying number-like digits is a DECISION owed —
 * freeze it or write a handler — never a silent freeze.
 */
const numericFlag = (h: Handling, text: string, what: string): Array<Case.Flag> => {
  if (!h.unrecognized) return []
  const digits = [...text].filter((c) => c >= "0" && c <= "9").length
  return digits >= 5
    ? [
        {
          kind: "unrecognized-body-part",
          detail: `${what} (not in the rewrite registry) and carries number-like digits — decide freeze vs a new handler`
        }
      ]
    : []
}

/** Whether the captured datagram carried a body at all. */
export const carriesBody = (m: Flows.Msg): boolean => wireBody(m) !== undefined

const NO_BODY: StoredBody = { body: { mode: "absent" }, resources: [], flags: [] }

/**
 * The expect-side body assertion. A frozen TEXT body is stored as a resource
 * and asserted by content (`compare` left absent: exact); SDP and multipart
 * are asserted by shape, absence as its own claim. A binary payload — one
 * extraction handed over as `head` + `body_b64`, whatever its type — stays
 * undeclared: the recording it would be confronted with is text.
 */
export const expectBody = (m: Flows.Msg, slug: string): StoredBody => {
  const payload = wireBody(m)
  if (!payload) return NO_BODY
  if (payload.boundary !== undefined) return { ...NO_BODY, body: { mode: "multipart-present" } }
  if (mimeKey(payload.mediaType) === "application/sdp") return { ...NO_BODY, body: { mode: "sdp-present" } }
  const h = handlerFor(payload.mediaType)
  if (h.mode !== "frozen" || payload.binary) return { ...NO_BODY, body: undefined }
  const relPath = resourceName(slug, 0, payload.mediaType)
  return {
    body: { ref: relPath, mode: "frozen", "content-type": payload.contentType },
    resources: [{ relPath, text: payload.text }],
    flags: numericFlag(h, payload.text, `${slug} body is ${payload.contentType}`)
  }
}

/**
 * The body registry's LOOKUP KEY: `type/subtype` lowercased, parameters stripped
 * (§8.2). Never what the pivot stores — the stored value is verbatim.
 */
export const mimeKey = (contentType: string): string =>
  (contentType.split(";")[0] ?? "").trim().toLowerCase()

const resourceName = (slug: string, n: number, contentType: string): string => {
  const head = mimeKey(contentType)
  const ext = head === "application/sdp"
    ? "sdp"
    : head.endsWith("+xml") || head.endsWith("/xml")
      ? "xml"
      : head.startsWith("text/")
        ? "txt"
        : "bin"
  return `resources/${slug}_${n}.${ext}`
}
