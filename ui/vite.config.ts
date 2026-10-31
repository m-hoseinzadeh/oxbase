import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Tauri expects a fixed dev-server port (see crates/app-tauri/tauri.conf.json `build.devUrl`).
const DEV_PORT = 5173;
// Set by `tauri dev` when targeting a remote device; unused on desktop.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  // Do not let Vite clear the terminal: Rust compile errors from `tauri dev` stay visible.
  clearScreen: false,
  server: {
    port: DEV_PORT,
    strictPort: true,
    host: host ?? false,
    ...(host ? { hmr: { protocol: "ws", host, port: DEV_PORT + 1 } } : {}),
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    // Tauri webviews: WebView2 (Chromium) on Windows, WebKit on macOS/Linux.
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome120" : "safari16",
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: Boolean(process.env.TAURI_ENV_DEBUG),
    chunkSizeWarningLimit: 1500,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    css: false,
    restoreMocks: true,
  },
});
