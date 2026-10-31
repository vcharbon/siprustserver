import "@sip/viewer-core/ladder.css"
import "./styles.css"

import { StrictMode } from "react"
import { createRoot } from "react-dom/client"
import { App } from "./App.tsx"
import { applyTheme, rememberedTheme } from "./theme.ts"

applyTheme(document.documentElement, rememberedTheme())

const root = document.getElementById("root")
if (root === null) throw new Error("no #root element")
createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>
)
