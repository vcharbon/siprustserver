/**
 * Sample operations for the setup tests: shell executables that leave a line
 * in `trace` at the root of their rig path for each step they run, so a test
 * reads which steps ran and in what order. Each name states its behaviour.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import type * as Lanes from "../src/lanes.js"
import type { Entry } from "./tree.js"

const sh = (body: string): Entry => ({ text: `#!/bin/sh\n${body}\n`, executable: true })

/** A step that appends `<step> <name>` to the trace two levels above the operation's directory. */
const traced = (line: string, then = "exit 0"): Entry => sh(`echo "${line}" >> ../../trace\n${then}`)

const manifest = (name: string, extra: string, probe: boolean): string =>
  `schema = "operation.v1"\nname = "${name}"\n${extra}\n[run]\ninject = "./inject"\nheal = "./heal"\n${probe ? `probe = "./probe"\n` : ""}`

/** The files of operation `name`: its manifest (`extra` lines before `[run]`) and its steps. */
const operation = (
  name: string,
  steps: { readonly inject?: Entry; readonly heal?: Entry; readonly probe?: Entry },
  extra = ""
): Record<string, Entry> => ({
  [`operations/${name}/operation.toml`]: manifest(name, extra, steps.probe !== undefined),
  [`operations/${name}/inject`]: steps.inject ?? traced(`inject ${name}`),
  [`operations/${name}/heal`]: steps.heal ?? traced(`heal ${name}`),
  ...(steps.probe === undefined ? {} : { [`operations/${name}/probe`]: steps.probe })
})

/** Every sample operation, as one rig path's files. */
export const SAMPLES: Readonly<Record<string, Entry>> = {
  ...operation("first", {}),
  ...operation("second", {}),
  ...operation("stateful", {
    inject: traced("inject stateful", `cp "$SIPRIG_CONTEXT" ../../context-seen.json\necho token > "$SIPRIG_STATE/token"`),
    heal: traced("heal stateful $(cat $SIPRIG_STATE/token)")
  }, `needs = ["records-store"]\n[params]\ngrace_s = 5`),
  ...operation("failing-inject", { inject: traced("inject failing-inject", "exit 3") }),
  ...operation("hanging-inject", {
    inject: traced("inject hanging-inject", `sleep 30 &\necho $! > "$SIPRIG_STATE/sleeper"\nwait`)
  }, `[timeouts]\ninject_s = 0.5`),
  ...operation("failing-heal", { heal: traced("heal failing-heal", "exit 1") }),
  ...operation("flaky-heal", {
    heal: traced("heal flaky-heal", `[ -e "$SIPRIG_STATE/tried" ] && exit 0\ntouch "$SIPRIG_STATE/tried"\nexit 1`)
  }),
  ...operation("late-recovery", {
    probe: traced("probe late-recovery", `n=$(cat "$SIPRIG_STATE/probes" 2>/dev/null || echo 0)\necho $((n+1)) > "$SIPRIG_STATE/probes"\n[ "$n" -ge 2 ] && exit 0\nexit 75`)
  }, `[timeouts]\nprobe_s = 10\nprobe_interval_s = 0.05`),
  ...operation("never-recovers", { probe: traced("probe never-recovers", "exit 75") }, `[timeouts]\nprobe_s = 0.3\nprobe_interval_s = 0.05`),
  ...operation("slow-heal", { heal: traced("heal slow-heal", `sleep 0.5\necho "healed slow-heal" >> ../../trace`) }),
  ...operation("needs-records", {}, `needs = ["records-store"]`)
}

/** The trace lines under the rig path `root`, in the order they were written. */
export const traceOf = (root: string): ReadonlyArray<string> => {
  const file = path.join(root, "trace")
  return fs.existsSync(file) ? fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "") : []
}

/** A simulated lane `id` naming the parts `parts` (kind to file), loaded as `Lanes.load` would. */
export const laneOf = (id: string, parts: Readonly<Record<string, string>> = {}): Lanes.Loaded => ({
  id,
  file: `lanes/${id}.toml`,
  lane: { schema: "lane.v1", clock: "simulated" },
  parts: new Map(Object.entries(parts).map(([kind, file]) => [kind, { kind, name: path.basename(file, ".toml"), file, value: { file } }])),
  operations: new Map(),
  capabilities: new Set()
})
