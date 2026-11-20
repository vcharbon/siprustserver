# CANCEL waits for the first provisional, within a grace window

**Status:** superseded by [ADR-0058](0058-cancel-waits-for-a-provisional.md). The CANCEL
waits for a provisional without bound, the front proxy relays a callee's `100 Trying`, and
`cancel-after-1xx` is a hard gate. Nothing of this ADR's grace window remains in the code.
