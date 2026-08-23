import { defineConfig } from "vite";

export default defineConfig({
  // Tauri loads the built assets via a custom protocol, so asset paths must be relative.
  base: "./",
  // Tauri expects a fixed dev server port for `tauri dev`.
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2020",
    rollupOptions: {
      input: {
        main: "index.html",
        simulator: "simulator.html",
      },
    },
  },
});
