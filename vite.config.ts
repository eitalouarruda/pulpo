import { defineConfig } from "vite";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  // WebKitGTK 2.38+ y WebView2 soportan top-level await.
  build: { target: "es2022" },
  server: {
    port: 1440,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1441,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
