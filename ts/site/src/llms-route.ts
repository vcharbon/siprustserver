/**
 * `/llms.txt`: what this viewer is, how a test reference reaches its page,
 * that a capture is read one call at a time, and the route table's map, its
 * links absolute under the origin the request was addressed to. The site is
 * named by its name (`site-name.ts`), as its top bar names it; what it says of
 * its campaigns and its sample ids are the pack's.
 */
import { llmsText, LLMS_PATH, type Site } from "@sip/viewer-core/routes"
import { Effect, Layer } from "effect"
import { HttpRouter, HttpServerRequest, HttpServerResponse } from "effect/unstable/http"
import { Campaign } from "./campaign.ts"
import { laneRoles } from "./lane-roles.ts"
import { originOf } from "./origin.ts"
import { SitePack } from "./site-pack.ts"
import { siteName } from "./site-name.ts"

/** What `/llms.txt` states of this viewer; `campaigns` are the end-to-end campaigns this server knows (`Campaign.listOf`). */
export const siteOf = (
  campaigns: ReadonlyArray<string>,
  pack: Pick<SitePack.Pack, "title" | "lanes" | "simulatedSut" | "llms">
): Site => {
  const { llms } = pack
  const study = `/triage/${llms.sampleCapture}?case=${llms.sampleCall}&lane=${llms.sampleLane}`
  return {
    name: siteName(pack.title),
    summary: [
      "SIP callflow ladders of a B2BUA under test, replayed on lanes, each lane one SUT:",
      ...laneRoles(pack).map(([lane, sut]) => `- ${lane}: ${sut}`),
      ...llms.summary,
      "A capture is read one call at a time: its study page lists its calls, each linked to its own overview;",
      "a call's overview, digest, message reads, text exports and log cuts read that call's data alone.",
      `Campaign names: ${campaigns.length === 0 ? "none" : campaigns.join(", ")}.`
    ],
    refs: [
      {
        ref: `a campaign case id \`<ref> [<sub-case>]\` (\`${llms.sampleCase}\`)`,
        page: "/campaign/<campaign>?case=<case id, URL-encoded>"
      },
      {
        ref: "that case's result on campaign run `#<N>` and lane `<lane>`",
        page: "/campaign/<campaign>?case=<case id, URL-encoded>&delta=<N>:<lane>"
      },
      {
        ref: `a capture basename \`<capture>\` (\`${llms.sampleCapture}\`): its calls, each linked`,
        page: "/triage/<capture>"
      },
      {
        ref: `a call id \`<capture>-k<hash>\` (\`${llms.sampleCall}\`)`,
        page: "/triage/<capture>?case=<call id>&lane=<lane>"
      },
      { ref: "a replay run id `<N>` (`runN`, `@run:N`)", page: "/report?run=@run:<N>" },
      { ref: "a job id `<N>`", page: "/jobs/<N>" }
    ],
    examples: {
      case: "/case/samples~noanswer-failover.v3",
      run: "/run/runs~alt-answered",
      compare: "/compare/case:samples~noanswer-failover.v3/run:runs~alt-answered",
      triage: `/triage?scope=red&lane=${llms.sampleLane}`,
      "triage-study": study,
      deltas: `/deltas?scope=red&lane=${llms.sampleLane}`,
      ...(campaigns[0] === undefined ? {} : { campaign: `/campaign/${encodeURIComponent(campaigns[0])}` })
    }
  }
}

/** `GET /llms.txt`. */
export const LlmsRoute = Layer.unwrap(
  Effect.gen(function*() {
    const campaign = yield* Campaign.Service
    const pack = yield* SitePack.Service
    return HttpRouter.add(
      "GET",
      LLMS_PATH,
      Effect.gen(function*() {
        const request = yield* HttpServerRequest.HttpServerRequest
        const campaigns = yield* campaign.list
        return HttpServerResponse.text(llmsText(siteOf(campaigns, pack), originOf(request)), {
          contentType: "text/plain; charset=utf-8"
        })
      })
    )
  })
)
