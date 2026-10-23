/**
 * The deployment's command lines a view offers for copying, as the server's
 * `/api/config` states them: `triage` hands a call to an agent, `request`
 * creates a replay request, `job` drives the job queue. A view shows a
 * command only where the deployment states it, and states no command name of
 * its own.
 */
import { useEffect, useState } from "react"
import { api } from "./api.ts"

export interface Commands {
  readonly triage?: string | undefined
  readonly request?: string | undefined
  readonly job?: string | undefined
}

let loaded: Promise<Commands> | undefined

/** The commands, read once per page load; none where the server states none or does not answer. */
export const loadCommands = (): Promise<Commands> =>
  loaded ??= api.config().then((config) => config.commands ?? {}).catch(() => ({}))

export const useCommands = (): Commands => {
  const [commands, setCommands] = useState<Commands>({})
  useEffect(() => {
    let live = true
    void loadCommands().then((read) => {
      if (live) setCommands(read)
    })
    return () => {
      live = false
    }
  }, [])
  return commands
}
