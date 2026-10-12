/** Walking a flow: every message step, in document order, blocks flattened. */
import type { FlowNode, Step } from "../generated/pivot-v3.ts"

/**
 * Every message step the flow holds, in document order. Nesting stops at one
 * level, so a branch's and a group's steps are the leaves.
 */
export const everyStep = (flow: readonly FlowNode[]): readonly Step[] => {
  const out: Step[] = []
  for (const node of flow) {
    switch (node.op) {
      case "alt":
        for (const branch of node.branches) out.push(...branch.steps)
        break
      case "unordered":
        out.push(...node.steps)
        break
      case "inject":
        break
      default:
        out.push(node)
        break
    }
  }
  return out
}
