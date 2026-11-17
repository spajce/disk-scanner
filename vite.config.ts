import { defineConfig } from "vite";

// Tauri expects a fixed port and does not want the dev server to clear the
// screen (Rust compiler output is far more useful in the terminal).
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    target: "chrome110",
    outDir: "dist",
    emptyOutDir: true,
  },
});
