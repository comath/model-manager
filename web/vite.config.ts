import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const API = process.env.MODEL_MANAGER_API ?? "http://127.0.0.1:8087";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    // In dev the UI runs on Vite and talks to the Rust API through this proxy.
    proxy: {
      "/api": { target: API, changeOrigin: true },
    },
  },
  build: { outDir: "dist", sourcemap: true },
});
