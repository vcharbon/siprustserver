/**
 * A scratch workspace holding the pack's reference lane on loopback addresses
 * free on this box, so two runs of a suite never share a port, and the files
 * its runner writes where the test says.
 */
import * as dgram from "node:dgram"
import * as fs from "node:fs"
import * as net from "node:net"
import * as path from "node:path"

/** A loopback address no socket of `kind` held a moment ago. */
export const freeAddress = async (kind: "udp" | "tcp"): Promise<string> => {
  if (kind === "tcp") {
    const server = net.createServer()
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve))
    const { port } = server.address() as net.AddressInfo
    await new Promise((resolve) => server.close(resolve))
    return `127.0.0.1:${port}`
  }
  const socket = dgram.createSocket("udp4")
  await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", resolve))
  const { port } = socket.address()
  await new Promise<void>((resolve) => socket.close(resolve))
  return `127.0.0.1:${port}`
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
 * The workspace `dir` holding the reference lane of the pack at `pack` on
 * free addresses: its runner logging, naming its pid, capturing and writing
 * its records where `written` says, `calls-ended` read 0.1 s after each
 * call, so a batch stays under a second, and the `fail-busy` collector where
 * `failing` is set.
 */
export const hostedWorkspace = async (pack: string, dir: string, written: Written, failing = false): Promise<string> => {
  const lane = fs.readFileSync(path.join(pack, "lanes", "reference.toml"), "utf8")
    .replace(`ingress = "127.0.0.1:5160"`, `ingress = "${await freeAddress("udp")}"`) +
    `\n[collectors.calls-ended]\ndelay_s = 0.1\n`
  fs.mkdirSync(path.join(dir, "lanes"), { recursive: true })
  fs.writeFileSync(path.join(dir, "lanes", "reference.toml"), lane)
  const target = (kind: string, content: string) => {
    fs.mkdirSync(path.join(dir, "targets", kind), { recursive: true })
    fs.writeFileSync(path.join(dir, "targets", kind, "reference.toml"), content)
  }
  target("b2bua-runner", `metrics = "${await freeAddress("tcp")}"\nadmin = "${await freeAddress("tcp")}"\n`)
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
