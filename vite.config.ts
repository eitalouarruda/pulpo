import { defineConfig } from "vite";

const host = process.env.TAURI_DEV_HOST;
// `./dev.sh --test` usa otro puerto para convivir con otro `./dev.sh`.
const port = Number(process.env.PULPO_DEV_PORT) || 1440;

export default defineConfig({
  clearScreen: false,
  // WebKitGTK 2.38+ y WebView2 soportan top-level await.
  build: { target: "es2022" },
  server: {
    port,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: port + 1,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
