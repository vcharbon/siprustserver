/**
 * The environment a campaign page reads, picked among every environment that
 * wrote the campaign's runs; the one read stays listed though it wrote none.
 * Shown only where another environment can be picked.
 */
import type { JSX } from "react"
import type { Campaign } from "../api.ts"
import { href, navigate } from "../router.ts"

export const CampaignEnvPick = ({ view }: { readonly view: Campaign }): JSX.Element | null => {
  const envs = view.env === null || view.envs.includes(view.env) ? view.envs : [view.env, ...view.envs]
  if (!envs.some((env) => env !== view.env)) return null
  return (
    <label>
      env{" "}
      <select value={view.env ?? ""} onChange={(event) => navigate(href.campaign(view.capture, { env: event.target.value }))}>
        {envs.map((env) => <option key={env} value={env}>{env}</option>)}
      </select>
    </label>
  )
}
