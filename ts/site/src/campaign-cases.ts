/**
 * A campaign case as its case file states it: the file's text — the comment
 * above its `export default` is the case's intention — and the document this
 * checkout compiles its sub-case into, among the case files a run reads, read
 * as they stand at each request; the pack finds them (`layerOf`). A case id
 * is `<ref> [<sub-case>]` (`Results.caseNameOf`); an id no case file states
 * is a 404.
 */
import { Effect, Layer } from "effect"
import * as Context from "effect/Context"
import { NotFound } from "./catalog.ts"

/** What the endpoints and the page overview read of one case. */
export interface Case {
  /** The case file, as a run names it. */
  readonly file: string
  readonly text: string
  /** The comment lines above the case's `export default`, unmarked. */
  readonly intention: ReadonlyArray<string>
  /** The compiled pivot document of the case id's sub-case. */
  readonly pivot: unknown
}

/** The case file a case id names, as the pack finds it: the file, its text, its sub-case's compiled document. */
export interface Found {
  readonly file: string
  readonly text: string
  readonly document: unknown
}

export interface Interface {
  readonly read: (caseId: string) => Effect.Effect<Case, NotFound>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/CampaignCases") {}

const COMMENT = /^\s*(\/\/|\/\*\*?|\*\/?)\s?/

/** The comment block right above `export default` in `text`, its markers stripped. */
export const intentionOf = (text: string): ReadonlyArray<string> => {
  const lines = text.split("\n")
  const at = lines.findIndex((line) => line.startsWith("export default"))
  const block: string[] = []
  for (let i = at - 1; i >= 0; i--) {
    const line = lines[i] ?? ""
    if (!COMMENT.test(line)) break
    block.unshift(line.replace(COMMENT, "").replace(/\s*\*\/\s*$/, "").trimEnd())
  }
  return block.filter((line) => line !== "")
}

const caseOf = (found: Found): Case => ({
  file: found.file,
  text: found.text,
  intention: intentionOf(found.text),
  pivot: found.document
})

/** The cases `find` answers at each request; one it does not find, or whose files do not read, is a 404 saying why. */
export const layerOf = (find: (caseId: string) => Effect.Effect<Found | undefined, { readonly reason: string }>) =>
  Layer.succeed(
    Service,
    Service.of({
      read: (caseId) =>
        find(caseId).pipe(
          Effect.mapError((e) => new NotFound({ id: caseId, note: e.reason })),
          Effect.flatMap((found) =>
            found === undefined
              ? Effect.fail(new NotFound({ id: caseId, note: "no case file of this checkout states this case" }))
              : Effect.succeed(caseOf(found))
          )
        )
    })
  )

export * as CampaignCases from "./campaign-cases.ts"
