import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Build output (dist/) is what crates/api embeds via RustEmbed (ADR 0004):
// one artifact serves the daemon PWA, the desktop webview, and remote mode.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": new URL("./src", import.meta.url).pathname },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
