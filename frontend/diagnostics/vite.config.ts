import preact from "@preact/preset-vite";
import { defineConfig } from "vite";


export default defineConfig({
  plugins: [preact()],
  base: "./",
  build: {
    target: "es2020",
    cssCodeSplit: false,
    sourcemap: false,
    modulePreload: false,
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    rollupOptions: {
      output: {
        inlineDynamicImports: true,
        entryFileNames: "assets/diagnostics-[hash].js",
        assetFileNames: "assets/diagnostics-[hash][extname]",
      },
    },
  },
});
