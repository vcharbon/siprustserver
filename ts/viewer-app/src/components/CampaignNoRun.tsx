/**
 * The page of a campaign the environment read has no run of: `no run yet`
 * where no environment ran it, and the start of a run of every case, open,
 * where the server runs the environment read (`CampaignStart`).
 */
import type { JSX } from "react"
import type { Campaign } from "../api.ts"
import { CampaignEnvPick } from "./CampaignEnvPick.tsx"
import { CampaignStart } from "./CampaignLaunch.tsx"

export const CampaignNoRun = ({ view }: { readonly view: Campaign }): JSX.Element => (
  <div className="page campaign">
    <header className="cp-head">
      <div>
        <h1>{view.title ?? "Campaign summary"}</h1>
        <p className="muted mono">{view.capture}{view.env === null ? "" : ` · ${view.env}`}</p>
      </div>
      <div className="cp-controls">
        <CampaignEnvPick view={view} />
        {view.launch !== null && (
          <CampaignStart
            key={view.capture}
            capture={view.capture}
            lanes={view.launch.lanes}
            stated={view.launch.defaultLanes}
            tags={view.launch.tags}
            cases={view.cases}
            collection={view.launch.collection}
            open
          />
        )}
      </div>
    </header>
    <p className="muted">{view.envs.length === 0 ? "no run yet" : "no run in this environment"}</p>
  </div>
)
