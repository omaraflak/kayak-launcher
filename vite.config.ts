import { defineConfig } from "vite";

// The Tauri dev server must be on a fixed port and must not fall back to
// another one, because tauri.conf.json points the launcher window at this exact
// URL during development.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "es2021",
    // Tauri ships its own webview, so there is no old-browser fallback to keep.
    minify: "esbuild",
    sourcemap: false,
  },
});
