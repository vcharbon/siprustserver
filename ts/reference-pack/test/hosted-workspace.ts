/**
 * A scratch workspace holding the pack's reference lane on loopback addresses
 * free on this box and below the kernel's ephemeral range, so no other
 * process's bind on port 0 nor outbound connect takes one before the runner
 * binds it, and the files its runner writes where the test says.
 */
import * as dgram from "node:dgram"
import * as fs from "node:fs"
import * as net from "node:net"
import * as path from "node:path"

/** The lowest port a free address is picked from, where the ephemeral range leaves room above it. */
const FLOOR = 10_000

/** The lowest unprivileged port, the floor where the ephemeral range starts at or below {@link FLOOR}. */
const UNPRIVILEGED = 1024

/** The fewest ports a pick needs below the ephemeral range. */
const SPAN = 1000

/** The ephemeral range's low bound where `/proc` does not state it, the IANA dynamic range's being higher. */
const EPHEMERAL_LOW = 32_768

/** How many taken ports a pick skips before it gives up. */
const TRIES = 100

/**
 * The kernel's ephemeral range's low bound: the ports from it up are the
 * ones it hands to a bind on port 0 and to an outbound connect.
 */
const ephemeralLow = (): number => {
  let stated: string
  try {
    stated = fs.readFileSync("/proc/sys/net/ipv4/ip_local_port_range", "utf8")
  } catch {
    return EPHEMERAL_LOW
  }
  const low = Number(stated.trim().split(/\s+/)[0])
  if (!Number.isInteger(low)) throw new Error(`ip_local_port_range states no low bound: ${JSON.stringify(stated)}`)
  return low
}

/**
 * The ports `[floor, ceiling)` a free address is picked from: below the
 * ephemeral range, from {@link FLOOR} where it leaves {@link SPAN} ports
 * there, else from {@link UNPRIVILEGED}. Fails where fewer remain.
 */
const pickRange = (): { readonly floor: number; readonly ceiling: number } => {
  const ceiling = ephemeralLow()
  const floor = ceiling - FLOOR >= SPAN ? FLOOR : UNPRIVILEGED
  if (ceiling - floor < SPAN) {
    throw new Error(`the ephemeral range starts at ${ceiling}: fewer than ${SPAN} unprivileged ports lie below it`)
  }
  return { floor, ceiling }
}

/** A socket of `kind` bound to loopback `port`, its release, or `undefined` where the port is taken. */
const hold = async (kind: "udp" | "tcp", port: number): Promise<(() => Promise<void>) | undefined> => {
  if (kind === "tcp") {
    const server = net.createServer()
    const bound = await new Promise<boolean>((resolve) => {
      server.once("error", () => resolve(false))
      server.listen(port, "127.0.0.1", () => resolve(true))
    })
    return bound ? () => new Promise<void>((resolve) => server.close(() => resolve())) : undefined
  }
  const socket = dgram.createSocket("udp4")
  const bound = await new Promise<boolean>((resolve) => {
    socket.once("error", () => resolve(false))
    socket.bind(port, "127.0.0.1", () => resolve(true))
  })
  if (!bound) socket.close()
  return bound ? () => new Promise<void>((resolve) => socket.close(() => resolve())) : undefined
}

/**
 * A loopback address for each of `kinds`, each free on this box as it was
 * picked and none another's. Each port is picked at random below the
 * ephemeral range, so no bind on port 0 nor outbound connect of another
 * process takes it before the runner binds it; each is held until all are
 * picked.
 */
export const freeAddresses = async (kinds: ReadonlyArray<"udp" | "tcp">): Promise<ReadonlyArray<string>> => {
  const { floor, ceiling } = pickRange()
  const held: Array<() => Promise<void>> = []
  const picked: Array<number> = []
  try {
    for (const kind of kinds) {
      let port: number | undefined
      for (let tried = 0; port === undefined && tried < TRIES; tried++) {
        const candidate = floor + Math.floor(Math.random() * (ceiling - floor))
        if (picked.includes(candidate)) continue
        const release = await hold(kind, candidate)
        if (release === undefined) continue
        held.push(release)
        port = candidate
      }
      if (port === undefined) throw new Error(`no free ${kind} port below ${ceiling} after ${TRIES} tries`)
      picked.push(port)
    }
  } finally {
    await Promise.all(held.map((release) => release()))
  }
  return picked.map((port) => `127.0.0.1:${port}`)
}

/** What a hosted workspace's runner writes: its log, its pid file, its packet capture and its call records. */
export interface Written {
  readonly log: string
  readonly pid: string
  readonly capture: string
  readonly cdr: string
}

/**
 * A job collector failing the cell of `bob is busy` (`fail-busy`), so the
 * job's `on-failure` collectors run over that cell alone.
 */
const FAIL_BUSY = {
  "collector.toml": [
    `schema = "collector.v1"`,
    `name = "fail-busy"`,
    `scope = "job"`,
    `[run]`,
    `collect = "./run"`,
    ``
  ].join("\n"),
  run: [
    `#!/usr/bin/env node`,
    `const fs = require("node:fs")`,
    `if (process.env.SIPRIG_STEP === "collect") {`,
    `  const context = JSON.parse(fs.readFileSync(process.env.SIPRIG_CONTEXT, "utf8"))`,
    `  for (const cell of context.cells.filter((one) => one.case.includes("busy"))) {`,
    `    console.log(JSON.stringify({ verification: "fail-busy", cell: cell.id, answer: "fail", message: "failed by the test" }))`,
    `  }`,
    `}`,
    ``
  ].join("\n")
}

/**
 * How long after each call its `calls-ended` reads the runner's live calls,
 * in seconds: the runner of a loaded box has let go of the call by then. The
 * last call of a batch waits it out in full.
 */
export const CALLS_ENDED_DELAY_S = 5

/**
 * The workspace `dir` holding the reference lane of the pack at `pack` on
 * free addresses: its runner logging, naming its pid, capturing and writing
 * its records where `written` says, `calls-ended` read
 * {@link CALLS_ENDED_DELAY_S} after each call, and the `fail-busy` collector
 * where `failing` is set.
 */
export const hostedWorkspace = async (pack: string, dir: string, written: Written, failing = false): Promise<string> => {
  const [ingress, metrics, admin] = await freeAddresses(["udp", "tcp", "tcp"])
  const lane = fs.readFileSync(path.join(pack, "lanes", "reference.toml"), "utf8")
    .replace(`ingress = "127.0.0.1:5160"`, `ingress = "${ingress}"`) +
    `\n[collectors.calls-ended]\ndelay_s = ${CALLS_ENDED_DELAY_S}\n`
  fs.mkdirSync(path.join(dir, "lanes"), { recursive: true })
  fs.writeFileSync(path.join(dir, "lanes", "reference.toml"), lane)
  const target = (kind: string, content: string) => {
    fs.mkdirSync(path.join(dir, "targets", kind), { recursive: true })
    fs.writeFileSync(path.join(dir, "targets", kind, "reference.toml"), content)
  }
  target("b2bua-runner", `metrics = "${metrics}"\nadmin = "${admin}"\n`)
  target("runner-log", `file = "${written.log}"\n`)
  target("runner-pid", `file = "${written.pid}"\n`)
  target("sip-capture", `file = "${written.capture}"\n`)
  target("cdr-file", `file = "${written.cdr}"\n`)
  if (failing) {
    const collector = path.join(dir, "collectors", "fail-busy")
    fs.mkdirSync(collector, { recursive: true })
    fs.writeFileSync(path.join(collector, "collector.toml"), FAIL_BUSY["collector.toml"])
    fs.writeFileSync(path.join(collector, "run"), FAIL_BUSY.run, { mode: 0o755 })
  }
  return dir
}

/** The `written` files under `dir`, named `name`. */
export const writtenUnder = (dir: string, name: string): Written => ({
  log: path.join(dir, `${name}.log`),
  pid: path.join(dir, `${name}.pid`),
  capture: path.join(dir, `${name}.pcapng`),
  cdr: path.join(dir, `${name}.cdr.jsonl`)
})
