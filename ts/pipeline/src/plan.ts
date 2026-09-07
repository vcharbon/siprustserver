/**
 * Numbering-plan classification: one phone number under every composed form
 * (`+CC NSN`, `00CC NSN`, national `0SN`, trunk-composed `+<trunk>CCNSN`,
 * service-prefixed `+CC<svc>NSN`) reduces to one key, and each written form
 * carries a label. Tier-2 positional refs and `topology.*.forms` are built on
 * this.
 *
 * The plan DOCUMENT is a deployment's, so it arrives decoded: this module owns
 * the classification, never a path. Divergence from the same document the Rust
 * extractor embeds is a pivot that role-maps a different set of URIs than the
 * reference one.
 */
export type NumberClass =
  | { readonly kind: "nsn"; readonly cc: string; readonly nsn: string }
  | { readonly kind: "private"; readonly digits: string }

export const classKey = (c: NumberClass): string => (c.kind === "nsn" ? c.nsn : c.digits)

/** The canonical observed shape of a class: `+CCNSN`, or the bare digits. */
export const canonical = (c: NumberClass): string =>
  c.kind === "nsn" ? `+${c.cc}${c.nsn}` : c.digits

interface Country {
  readonly cc: string
  readonly nsn_len: number
  readonly national_prefix?: string | null
}

interface Trunk {
  readonly prefix: string
  readonly len: number
}

export interface PlanDoc {
  readonly default_cc: string
  readonly countries: ReadonlyArray<Country>
  readonly trunks?: ReadonlyArray<Trunk>
  readonly private_len?: { readonly min: number; readonly max: number } | null
  readonly max_composed_prefix: number
  readonly fake_prefix: string
}

export class Plan {
  private readonly byLongestCc: ReadonlyArray<Country>

  constructor(private readonly doc: PlanDoc) {
    this.byLongestCc = [...doc.countries].sort(
      (a, b) => b.cc.length - a.cc.length || a.cc.localeCompare(b.cc)
    )
  }

  /** Classify a URI user-part (or tel number); `undefined` for a non-number. */
  classify(user: string): NumberClass | undefined {
    const plus = user.startsWith("+")
    const rest = plus ? user.slice(1) : user
    const digitLen = leadingDigits(rest)
    if (digitLen === 0) return undefined
    const digits = rest.slice(0, digitLen)
    if (plus) return this.e164(digits)
    if (digits.startsWith("00")) {
      const c = this.e164(digits.slice(2))
      if (c) return c
    }
    const nat = this.national(digits)
    if (nat) return nat
    const e = this.e164(digits)
    if (e) return e
    const range = this.doc.private_len
    if (digitLen === rest.length && range && digitLen >= range.min && digitLen <= range.max) {
      return { kind: "private", digits }
    }
    return undefined
  }

  /** The composed form a recognized user-part is written in (`forms` labels). */
  formLabel(user: string): string | undefined {
    const cls = this.classify(user)
    if (!cls) return undefined
    if (cls.kind === "private") return "private"
    const plus = user.startsWith("+")
    const rest = plus ? user.slice(1) : user
    const digits = rest.slice(0, leadingDigits(rest))
    if (plus) {
      for (const t of this.doc.trunks ?? []) {
        if (digits.startsWith(t.prefix) && digits.length > t.len) return "trunk-composed"
      }
      for (const c of this.byLongestCc) {
        if (!digits.startsWith(c.cc)) continue
        const tail = digits.slice(c.cc.length)
        if (tail.length > c.nsn_len) return "service-composed"
        if (tail.length === c.nsn_len) return "e164"
      }
      return "e164"
    }
    if (digits.startsWith("00")) return "intl-00"
    if (this.national(digits)) return "national"
    return "e164"
  }

  private national(digits: string): NumberClass | undefined {
    const c = this.doc.countries.find((x) => x.cc === this.doc.default_cc)
    const p = c?.national_prefix
    if (!c || !p || !digits.startsWith(p)) return undefined
    const sn = digits.slice(p.length)
    return sn.length === c.nsn_len ? { kind: "nsn", cc: c.cc, nsn: sn } : undefined
  }

  private e164(digits: string): NumberClass | undefined {
    for (const t of this.doc.trunks ?? []) {
      if (digits.startsWith(t.prefix) && digits.length > t.len) {
        const c = this.ccNsnExact(digits.slice(t.len))
        if (c) return c
      }
    }
    for (const c of this.byLongestCc) {
      if (!digits.startsWith(c.cc)) continue
      const rest = digits.slice(c.cc.length)
      if (rest.length === c.nsn_len) return { kind: "nsn", cc: c.cc, nsn: rest }
      if (rest.startsWith("00")) {
        const found = this.ccNsnExact(rest.slice(2))
        if (found) return found
      }
      if (rest.length > c.nsn_len && rest.length - c.nsn_len <= this.doc.max_composed_prefix) {
        const nsn = rest.slice(rest.length - c.nsn_len)
        // A real NSN never starts with `0`; the exception is the reserved fake
        // space, which is self-identifying and must still role-map.
        if (!nsn.startsWith("0") || nsn.startsWith(this.doc.fake_prefix)) {
          return { kind: "nsn", cc: c.cc, nsn }
        }
      }
    }
    return undefined
  }

  private ccNsnExact(digits: string): NumberClass | undefined {
    for (const c of this.byLongestCc) {
      if (digits.startsWith(c.cc)) {
        const rest = digits.slice(c.cc.length)
        if (rest.length === c.nsn_len) return { kind: "nsn", cc: c.cc, nsn: rest }
      }
    }
    return undefined
  }
}

const leadingDigits = (s: string): number => {
  let n = 0
  while (n < s.length && s[n]! >= "0" && s[n]! <= "9") n++
  return n
}
