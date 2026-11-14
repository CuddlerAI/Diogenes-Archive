import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is served same-origin in every environment: the Worker proxies
// /console/* to the API so the session cookie needs no cross-site exemption.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5174,
    strictPort: true,
    proxy: {
      "/console": { target: "http://127.0.0.1:8090", changeOrigin: false },
      // The annotator surface (docs/34): /join、/claim-account、/account call
      // /app/v1 same-origin, exactly like the Worker proxies it in production.
      "/app": { target: "http://127.0.0.1:8090", changeOrigin: false },
    },
  },
  build: { outDir: "dist", sourcemap: false },
});
