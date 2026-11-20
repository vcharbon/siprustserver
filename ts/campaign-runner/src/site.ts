/**
 * The triage site a run's report links to: the public URL the run's
 * environment states or derives under the pack's site variables
 * (`@sip/workspace` `public-url.ts`), or why the environment names none.
 */
import * as PublicUrl from "@sip/workspace/public-url"
import * as Effect from "effect/Effect"

/** The site's public URL off `env` under `site`'s variables, or the reason its setting is refused. */
export const siteOf = (site: PublicUrl.SiteVariables, env: NodeJS.ProcessEnv): Effect.Effect<string, string> =>
  PublicUrl.fromEnv(site, env).pipe(Effect.mapError((e) => e.reason))
