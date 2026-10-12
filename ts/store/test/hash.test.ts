/**
 * Content naming, on its own: the hash a document gets must not move when a
 * writer emits its keys in a different order, because that is the whole reason
 * a re-cut of unchanged content re-stores nothing — and a CASE's hash must move
 * when anything it carries does, because that is what keeps a re-cut of a
 * changed ledger row from conflicting onto the case already stored.
 */
import { describe, expect, it } from "vitest"
import { hashBytes, hashCase, hashDocument } from "../src/hash.js"

describe("hashDocument", () => {
  it("names the same document the same whatever order its keys arrive in", () => {
    const one = hashDocument({ b: [1, { y: 2, x: 1 }], a: "s" })
    const other = hashDocument({ a: "s", b: [1, { x: 1, y: 2 }] })
    expect(one.hash).toEqual(other.hash)
    expect(one.text).toEqual(`{"a":"s","b":[1,{"x":1,"y":2}]}`)
  })

  it("drops undefined values rather than naming two documents apart by one", () => {
    expect(hashDocument({ a: 1, b: undefined }).hash).toEqual(hashDocument({ a: 1 }).hash)
  })

  it("keeps array order, which is content", () => {
    expect(hashDocument([1, 2]).hash).not.toEqual(hashDocument([2, 1]).hash)
  })
})

describe("hashBytes", () => {
  it("hashes the bytes themselves, so a blob round-trips under its own name", () => {
    // sha256("") — the raw digest, not a digest of some JSON rendering of it.
    expect(hashBytes(new Uint8Array())).toEqual(
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    )
  })
})

describe("hashCase", () => {
  const bytes = (text: string): Uint8Array => new TextEncoder().encode(text)
  const parts = {
    document: { case: { id: "one" } },
    assets: [{ path: "resources/a.sdp", bytes: bytes("v=0") }],
    ledger: [{ number: "4242000000000001", document: { route: "site" } }]
  }

  it("names the document apart, so two cases of one document share its row", () => {
    expect(hashCase(parts).documentHash).toEqual(hashDocument(parts.document).hash)
    expect(hashCase(parts).documentText).toEqual(hashDocument(parts.document).text)
  })

  it("moves when the ledger row moves, though the document did not", () => {
    const moved = {
      ...parts,
      ledger: [{ number: "4242000000000001", document: { route: "mrf" } }]
    }
    expect(hashCase(moved).hash).not.toEqual(hashCase(parts).hash)
    expect(hashCase(moved).documentHash).toEqual(hashCase(parts).documentHash)
  })

  it("moves when an asset's bytes move", () => {
    const moved = { ...parts, assets: [{ path: "resources/a.sdp", bytes: bytes("v=1") }] }
    expect(hashCase(moved).hash).not.toEqual(hashCase(parts).hash)
  })

  it("moves when an asset is renamed, added or dropped", () => {
    const renamed = { ...parts, assets: [{ path: "resources/b.sdp", bytes: bytes("v=0") }] }
    expect(hashCase(renamed).hash).not.toEqual(hashCase(parts).hash)
    expect(hashCase({ ...parts, assets: [] }).hash).not.toEqual(hashCase(parts).hash)
  })

  it("does not move when the cut read the assets and rows in another order", () => {
    const two = {
      ...parts,
      assets: [
        { path: "resources/a.sdp", bytes: bytes("v=0") },
        { path: "resources/b.sdp", bytes: bytes("v=1") }
      ],
      ledger: [
        { number: "4242000000000001", document: { route: "site" } },
        { number: "4242000000000002", document: { route: "mrf" } }
      ]
    }
    const reversed = {
      ...two,
      assets: [...two.assets].reverse(),
      ledger: [...two.ledger].reverse()
    }
    expect(hashCase(reversed).hash).toEqual(hashCase(two).hash)
  })

  it("is not the document's own name, so the two never collide in one column", () => {
    expect(hashCase(parts).hash).not.toEqual(hashCase(parts).documentHash)
  })
})
