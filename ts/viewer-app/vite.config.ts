import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

/** The API server's port. Keep in step with the server deployment's port. */
const apiTarget = process.env["FLOW_VIEWER_API"] ?? "http://localhost:5175"

export default defineConfig({
  plugins: [react()],
  // The core package is consumed as TypeScript source, so Vite transpiles it
  // and HMR reaches the layout engine as well as the app.
  optimizeDeps: { exclude: ["@sip/viewer-core"] },
  server: {
    port: 5174,
    proxy: { "/api": { target: apiTarget, changeOrigin: true } }
  },
  build: { outDir: "dist", emptyOutDir: true }
})
