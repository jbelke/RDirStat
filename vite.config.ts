import { fileURLToPath, URL } from "node:url";
import process from "node:process";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// `tauri dev --host` sets this so a device on the LAN can reach the dev server.
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  // Tailwind v4 is a Vite plugin. There is no tailwind.config.js and no
  // PostCSS step: the theme is declared CSS-first with `@theme` in
  // src/index.css.
  plugins: [react(), tailwindcss()],

  resolve: {
    // `@/…` -> src/…  — the alias shadcn/ui generates imports against.
    // Keep this in sync with `compilerOptions.paths` in tsconfig.json.
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },

  // Vite options tailored for Tauri development, applied by `tauri dev` and
  // `tauri build`.
  //
  // 1. Do not let Vite clear the screen over Rust compiler errors.
  clearScreen: false,
  // 2. Tauri expects a fixed port and must fail rather than silently move.
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. Rust sources are watched by cargo, not by Vite.
      ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"],
    },
  },

  build: {
    // Safari/WKWebView is the only engine this app ships against, so there is
    // no reason to down-level for other browsers.
    target: "safari17",
    // Debug builds keep sourcemaps and readable output; release builds drop
    // both. `true` means "Vite's default minifier" — which is oxc in Vite 8.
    // Naming "esbuild" here fails: Vite 8 no longer bundles it.
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    minify: !process.env.TAURI_ENV_DEBUG,
  },
}));
