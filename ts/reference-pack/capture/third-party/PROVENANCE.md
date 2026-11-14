# Provenance of the vendored captures

The two captures in this directory were produced by another SIP stack's test harness and are
copied here unchanged. `test/third-party-captures.test.ts` asserts that each file still has
the SHA-256 stated below.

- **Source repository**: <https://github.com/NormB/sipnab> (sipnab)
- **Commit**: `bb9bbf70e7386e5fec6818cf74373f1044c35ab3`, the last commit to change either file
- **Licence chosen**: MIT. sipnab is offered under the MIT License or the Apache License,
  Version 2.0, at the licensee's option (`SPDX-License-Identifier: MIT OR Apache-2.0`). These
  copies are taken under the MIT License, whose notice follows.

| File here | Path in the source repository | SHA-256 |
|---|---|---|
| `asterisk-22-xcid.pcap` | `tests/fixtures/xcid/asterisk-22-xcid.pcap` | `6e43618794c9e5d1b908831abe658c1b4f2494a886431edce9cf1ffc2a3b74cd` |
| `refer-transfer.pcap` | `tests/fixtures/refer/refer-transfer.pcap` | `d98fb2f4be3ce43dac3b6decb8b059c47f92114dfa3a3432e6618cd1e93cc6a2` |

The source repository's `tests/PROVENANCE.md` lists both as synthetic, each made by a generator
script it tracks (`harness/asterisk-xcid/capture.sh`, `harness/refer/capture.sh`) on a private
network using RFC 5737 addresses and `example.com` hosts:

- `asterisk-22-xcid.pcap`: three calls placed by SIPp through Asterisk 22.11.0, a back-to-back
  user agent that gives the callee's leg a Call-ID of its own and adds `X-CID`, carrying the
  caller's Call-ID, to the callee's INVITE.
- `refer-transfer.pcap`: two calls placed by SIPp through an OpenSIPS 4.0.2 proxy that
  record-routes, each with a blind transfer (RFC 3515: REFER, 202, NOTIFY). The first transfer
  completes and the caller hangs up; the capture ends with the second transfer and its call
  still open.

## MIT License

```
MIT License

Copyright (c) 2024-2026 Norm Brandinger

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
