import { fileURLToPath, URL } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// The development server forwards the core's paths to a medicus server on port 8042
// (`MEDICUS_URL` overrides), so the application runs same-origin as in production.
const core = process.env.MEDICUS_URL ?? "http://127.0.0.1:8042";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: {
    proxy: Object.fromEntries(["/api", "/health", "/rs", "/nodes"].map((p) => [p, core])),
  },
  build: { sourcemap: false, chunkSizeWarningLimit: 1024 },
  test: { environment: "node" },
});
