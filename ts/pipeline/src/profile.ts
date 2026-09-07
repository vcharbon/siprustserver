/**
 * The network profile: the CIDR list of addresses that COULD be the system
 * under test in any capture off one deployment.
 *
 * It is a candidate filter, nothing more. Which of those addresses is the SUT of
 * one capture is `./sut.ts`'s decision, and the boundary of a hop is then read
 * off that capture's own SUT set — never off the subnet list, which names both
 * sides of an internal hop.
 *
 * The profile DOCUMENT is a deployment's, so it arrives decoded: this module
 * owns the matcher, never a path.
 */

/** A profile document as a deployment writes it. */
export interface ProfileDoc {
  readonly mode: string
  readonly sut: ReadonlyArray<string>
}

export class SutProfile {
  private readonly matchers: ReadonlyArray<(ip: number) => boolean>

  constructor(sut: ReadonlyArray<string>) {
    this.matchers = sut.map(matcher)
  }

  /** Whether `sock` (`ip` or `ip:port`) sits in the deployment's address space. */
  matches(sock: string): boolean {
    const ip = parseIpv4(ipOf(sock))
    return ip !== undefined && this.matchers.some((m) => m(ip))
  }
}

/** The host part of an `ip:port` socket string. */
export const ipOf = (sock: string): string => {
  const i = sock.lastIndexOf(":")
  return i < 0 ? sock : sock.slice(0, i)
}

const matcher = (entry: string): ((ip: number) => boolean) => {
  const slash = entry.indexOf("/")
  if (slash < 0) {
    const exact = parseIpv4(entry)
    return (ip) => exact !== undefined && ip === exact
  }
  const base = parseIpv4(entry.slice(0, slash))
  const bits = Number(entry.slice(slash + 1))
  return (ip) => {
    if (base === undefined || !Number.isInteger(bits) || bits < 0 || bits > 32) return false
    if (bits === 0) return true
    const mask = (0xffffffff << (32 - bits)) >>> 0
    return ((ip & mask) >>> 0) === ((base & mask) >>> 0)
  }
}

const parseIpv4 = (s: string): number | undefined => {
  const parts = s.split(".")
  if (parts.length !== 4) return undefined
  let out = 0
  for (const p of parts) {
    const v = Number(p)
    if (!Number.isInteger(v) || v < 0 || v > 255 || p === "") return undefined
    out = ((out << 8) | v) >>> 0
  }
  return out
}
