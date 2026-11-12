import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"
import { stampOf } from "./build-commit.ts"

/** The site's API origin the dev server proxies to: `SIPRIG_SITE_API`, else the site's default port. */
const apiTarget = process.env["SIPRIG_SITE_API"] ?? "http://localhost:5175"

export default defineConfig({
  plugins: [react()],
  // The core package is consumed as TypeScript source, so Vite transpiles it
  // and HMR reaches the layout engine as well as the app.
  optimizeDeps: { exclude: ["@sip/viewer-core"] },
  define: { __SIPRIG_BUILD__: JSON.stringify(stampOf(import.meta.dirname, process.env, new Date())) },
  server: {
    port: 5174,
    proxy: { "/api": { target: apiTarget, changeOrigin: true } }
  },
  build: { outDir: "dist", emptyOutDir: true }
})
