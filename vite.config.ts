/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath, URL } from "node:url";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;
const xtermSourceRoot = fileURLToPath(new URL("./node_modules/@xterm/xterm/src", import.meta.url));

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [react()],
  esbuild: {
    tsconfigRaw: {
      compilerOptions: {
        experimentalDecorators: true,
      },
    },
  },

  // @xterm/addon-webgl 0.19.0 の公開 bundle には、長時間稼働後の texture atlas
  // merge で表示内容と選択位置が食い違う不具合がある。pnpm patch で取り込んだ
  // upstream 修正済み source を bundle 対象にする（正式リリース後は削除可能）。
  resolve: {
    alias: [
      {
        find: "@xterm/addon-webgl",
        replacement: fileURLToPath(new URL("./node_modules/@xterm/addon-webgl/src/WebglAddon.ts", import.meta.url)),
      },
      { find: /^browser\/(.*)$/, replacement: `${xtermSourceRoot}/browser/$1` },
      { find: /^common\/(.*)$/, replacement: `${xtermSourceRoot}/common/$1` },
      { find: /^vs\/(.*)$/, replacement: `${xtermSourceRoot}/vs/$1` },
    ],
  },

  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
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
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
