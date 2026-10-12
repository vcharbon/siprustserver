/** An in-app link: a real href (so it can be copied) that routes without a reload. */
import type { JSX, ReactNode } from "react"
import { navigate } from "../router.ts"

export const Link = (
  { to, className, children }: { to: string; className?: string; children: ReactNode }
): JSX.Element => (
  <a
    href={to}
    className={className}
    onClick={(event) => {
      if (event.metaKey || event.ctrlKey || event.shiftKey) return
      event.preventDefault()
      navigate(to)
    }}
  >
    {children}
  </a>
)
