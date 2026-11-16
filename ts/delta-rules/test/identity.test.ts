/**
 * Identity headers compared: URI equality as RFC 3261 §19.1.4 states it, an
 * address apart from its dialog tag, and the identity readings a From, To or
 * Contact difference is named by.
 */
import { Fold } from "@sip/pipeline"
import type { Probe } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import {
  addressKeptWhole,
  addrPair,
  fromToRewritten,
  identityButTheNumber,
  numberingFormSwapped,
  paramsButTag,
  sameAddressButTag,
  sameButTheNumber,
  sameParams,
  sameSchemeIdentity,
  uriReplaced,
  urisEqual
} from "../src/identity.js"
import { headerProbe } from "./probes.js"

const pair = (name: string, captured: string, replayed: string): Probe.HeaderProbe =>
  headerProbe({ name, captured: [captured], replayed: [replayed] }) as Probe.HeaderProbe

const nameAddr = (text: string): Fold.NameAddr => Fold.parseNameAddr(text)!

describe("URI equality (RFC 3261 §19.1.4)", () => {
  it("holds two URIs equal part for part", () => {
    expect(urisEqual("<sip:alice@example.com:5070;transport=tcp>", "<sip:alice@example.com:5070;transport=tcp>")).toBe(true)
  })

  it("reads the scheme, the host and a parameter value without case, the userinfo as written", () => {
    expect(urisEqual("<SIP:alice@EXAMPLE.com;transport=TCP>", "<sip:alice@example.com;transport=tcp>")).toBe(true)
    expect(urisEqual("<sip:ALICE@example.com>", "<sip:alice@example.com>")).toBe(false)
  })

  it("never holds a URI equal to one that omits its default port, or a sips URI to a sip one", () => {
    expect(urisEqual("<sip:bob@example.com>", "<sip:bob@example.com:5060>")).toBe(false)
    expect(urisEqual("<sips:bob@example.com>", "<sip:bob@example.com>")).toBe(false)
  })

  it("ignores a parameter only one URI carries, transport included", () => {
    expect(urisEqual("<sip:carol@example.com>", "<sip:carol@example.com;newparam=5>")).toBe(true)
    expect(urisEqual("<sip:carol@example.com;transport=udp>", "<sip:carol@example.com>")).toBe(true)
  })

  it("compares a parameter both URIs carry, transport included", () => {
    expect(urisEqual("<sip:carol@example.com;transport=udp>", "<sip:carol@example.com;transport=tcp>")).toBe(false)
  })

  it.each(["user=phone", "ttl=15", "method=INVITE", "maddr=192.0.2.1"])(
    "never ignores %s where only one URI carries it",
    (param) => {
      expect(urisEqual("<sip:carol@example.com>", `<sip:carol@example.com;${param}>`)).toBe(false)
      expect(urisEqual(`<sip:carol@example.com;${param}>`, "<sip:carol@example.com>")).toBe(false)
    }
  )

  it("compares user parameters and `?headers`", () => {
    expect(urisEqual("<sip:5550100;isub=1@example.com>", "<sip:5550100;isub=2@example.com>")).toBe(false)
    expect(urisEqual("<sip:alice@example.com?subject=a>", "<sip:alice@example.com?subject=b>")).toBe(false)
  })

  it("holds nothing equal to a value that is no address", () => {
    expect(urisEqual("not an address", "<sip:alice@example.com>")).toBe(false)
  })
})

describe("an address apart from its tag", () => {
  it("holds two addresses that differ in the dialog tag alone", () => {
    expect(sameAddressButTag('"Alice" <sip:alice@example.com>;tag=1928301774', '"Alice" <sip:alice@example.com>;tag=a6c85cf'))
      .toBe(true)
  })

  it("keeps the display name, the URI and every other header parameter", () => {
    expect(sameAddressButTag('"Alice" <sip:alice@example.com>;tag=1', "<sip:alice@example.com>;tag=2")).toBe(false)
    expect(sameAddressButTag("<sip:alice@example.com>;tag=1", "<sip:alice@example.com;maddr=192.0.2.1>;tag=2")).toBe(false)
    expect(sameAddressButTag("<sip:alice@example.com>;tag=1", "<sip:alice@example.com>;tag=2;x=1")).toBe(false)
  })

  it("drops the tag and nothing else from header parameters", () => {
    const params = paramsButTag(nameAddr("<sip:alice@example.com>;tag=1;x=y").params)
    expect([...params]).toEqual([["x", "y"]])
  })

  it("holds parameter maps equal entry for entry", () => {
    expect(sameParams(new Map([["a", "1"]]), new Map([["a", "1"]]))).toBe(true)
    expect(sameParams(new Map([["a", "1"]]), new Map([["a", "2"]]))).toBe(false)
    expect(sameParams(new Map([["a", "1"]]), new Map([["a", "1"], ["b", ""]]))).toBe(false)
  })
})

describe("a single-valued identity header's pair", () => {
  it("parses each side's one name-addr", () => {
    const [a, b] = addrPair(pair("From", "<sip:alice@example.com>;tag=1", '"Bob" <sips:bob@example.org>'))!
    expect(a.uri.user).toBe("alice")
    expect(b.display).toBe("Bob")
    expect(b.uri.scheme).toBe("sips")
  })

  it("is undefined where a side carries no single address", () => {
    expect(addrPair(headerProbe({ name: "From", captured: ["<sip:alice@example.com>"], replayed: [] }) as Probe.HeaderProbe))
      .toBeUndefined()
    expect(addrPair(pair("From", "not an address", "<sip:alice@example.com>"))).toBeUndefined()
  })

  it("holds two name-addrs equal but for the URI's user", () => {
    expect(sameButTheNumber(nameAddr("<sip:+15555550100@example.com>"), nameAddr("<sip:+15555550101@example.com>"))).toBe(true)
    expect(sameButTheNumber(nameAddr("<sip:+15555550100@example.com>"), nameAddr("<sip:+15555550101@example.org>"))).toBe(false)
  })

  it("reads a URI replaced where its scheme, user, host or port moved, not its parameters", () => {
    expect(uriReplaced(pair("To", "<sip:bob@example.com>", "<sip:bob@example.com:5070>"))).toBe(true)
    expect(uriReplaced(pair("To", "<sip:bob@example.com>", "<sip:bob@example.com;transport=tcp>"))).toBe(false)
  })
})

describe("an identity apart from its number", () => {
  const from = (captured: string, replayed: string) => pair("From", captured, replayed)

  it("holds two addresses that differ in the number and the tag alone", () => {
    const h = from("<sip:+15555550100@example.com>;tag=1", "<sip:+15555550101@example.com>;tag=2")
    expect(identityButTheNumber(h, { hostMayMove: false })).toBe(true)
  })

  it("lets a capture host that is an IP literal give way, and only where the host may move", () => {
    const literal = from("<sip:+15555550100@192.0.2.10>", "<sip:+15555550101@example.com>")
    expect(identityButTheNumber(literal, { hostMayMove: true })).toBe(true)
    expect(identityButTheNumber(literal, { hostMayMove: false })).toBe(false)
    const named = from("<sip:+15555550100@example.org>", "<sip:+15555550101@example.com>")
    expect(identityButTheNumber(named, { hostMayMove: true })).toBe(false)
  })

  it("holds the display name, unless each spells its own number", () => {
    const spelt = from('"+15555550100" <sip:+15555550100@example.com>', '"+15555550101" <sip:+15555550101@example.com>')
    expect(identityButTheNumber(spelt, { hostMayMove: false })).toBe(true)
    const renamed = from('"Alice" <sip:+15555550100@example.com>', '"Bob" <sip:+15555550101@example.com>')
    expect(identityButTheNumber(renamed, { hostMayMove: false })).toBe(false)
  })

  it("reads `phonecontext` as RFC 3966's `phone-context`, and holds every other user parameter", () => {
    const context = from("<sip:5550100;phonecontext=+1@example.com>", "<sip:5550101;phone-context=+1@example.com>")
    expect(identityButTheNumber(context, { hostMayMove: false })).toBe(true)
    const isub = from("<sip:5550100;isub=1@example.com>", "<sip:5550101;isub=2@example.com>")
    expect(identityButTheNumber(isub, { hostMayMove: false })).toBe(false)
  })

  it("holds `?headers`, URI parameters and header parameters but the tag", () => {
    const opts = { hostMayMove: false }
    expect(identityButTheNumber(from("<sip:5550100@example.com?subject=a>", "<sip:5550101@example.com>"), opts)).toBe(false)
    expect(identityButTheNumber(from("<sip:5550100@example.com;user=phone>", "<sip:5550101@example.com>"), opts)).toBe(false)
    expect(identityButTheNumber(from("<sip:5550100@example.com>;x=1", "<sip:5550101@example.com>"), opts)).toBe(false)
  })

  it("lets the scheme, the host and `user=phone` change only where the form may change", () => {
    const swapped = from("<tel:+15555550100>", "<sip:+15555550101@example.com;user=phone>")
    expect(identityButTheNumber(swapped, { hostMayMove: false, formMayChange: true })).toBe(true)
    expect(identityButTheNumber(swapped, { hostMayMove: false })).toBe(false)
  })
})

describe("an address the replay states whole", () => {
  const to = (captured: string, replayed: string) => addressKeptWhole(pair("To", captured, replayed))

  it("holds a replay that adds a display name or a header parameter to the capture's address", () => {
    expect(to("<sip:+15555550100@example.com>;tag=1", '"Bob" <sip:+15555550101@example.com>;tag=2')).toBe(true)
    expect(to("<sip:bob@example.com>", "<sip:bob@example.com>;x=1")).toBe(true)
  })

  it("never holds a replay that adds nothing, renames the display or drops a parameter", () => {
    expect(to("<sip:bob@example.com>;tag=1", "<sip:bob@example.com>;tag=2")).toBe(false)
    expect(to('"Bob" <sip:bob@example.com>', '"Robert" <sip:bob@example.com>')).toBe(false)
    expect(to("<sip:bob@example.com>;x=1", '"Bob" <sip:bob@example.com>')).toBe(false)
  })

  it("holds the URI, two users that spell a number counting as one", () => {
    expect(to("<sip:bob@example.com>", '"Bob" <sip:carol@example.com>')).toBe(false)
    expect(to("<sip:+15555550100@example.com>", '"Bob" <sip:+15555550100@example.org>')).toBe(false)
  })
})

describe("the scheme of a From or To", () => {
  it("reads a same-scheme identity with no security downgrade, on From, To and Contact only", () => {
    expect(sameSchemeIdentity(pair("Contact", "<sip:a@192.0.2.10>", "<sip:b@198.51.100.20>"))).toBe(true)
    expect(sameSchemeIdentity(pair("From", "<sips:a@example.com>", "<sip:a@example.com>"))).toBe(false)
    expect(sameSchemeIdentity(pair("From", "<sip:a@example.com;transport=tls>", "<sip:a@example.com>"))).toBe(false)
    expect(sameSchemeIdentity(pair("From", "<sip:a@example.com>", "<tel:+15555550100>"))).toBe(false)
    expect(sameSchemeIdentity(pair("Reply-To", "<sip:a@example.com>", "<sip:b@example.com>"))).toBe(false)
  })

  it("reads a From or To carried as tel: on one side and sip: on the other, never a downgrade", () => {
    expect(numberingFormSwapped(pair("From", "<tel:+15555550100>", "<sip:+15555550100@example.com>"))).toBe(true)
    expect(numberingFormSwapped(pair("To", "<sip:+15555550100@example.com>", "<tel:+15555550100>"))).toBe(true)
    expect(numberingFormSwapped(pair("To", "<sips:+15555550100@example.com>", "<tel:+15555550100>"))).toBe(false)
    expect(numberingFormSwapped(pair("Contact", "<tel:+15555550100>", "<sip:+15555550100@example.com>"))).toBe(false)
  })

  it("reads a From or To rewritten where it moved beyond the number and an IP-literal host", () => {
    expect(fromToRewritten(pair("From", '"Alice" <sip:+15555550100@example.com>', '"Carol" <sip:+15555550100@example.com>')))
      .toBe(true)
    expect(fromToRewritten(pair("From", "<sip:+15555550100@192.0.2.10>", "<sip:+15555550101@example.com>"))).toBe(false)
    expect(fromToRewritten(pair("Contact", '"Alice" <sip:a@example.com>', '"Carol" <sip:a@example.com>'))).toBe(false)
  })
})
