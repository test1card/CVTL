import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
const framework = process.env.CVTL_FRAMEWORK;
const chart = process.env.CVTL_CHART;
export default defineConfig({
  base: "./",
  plugins: [framework === "svelte" ? svelte() : react()],
  resolve: {
    alias: {
      "@framework": resolve(
        `src/${framework}.jsx`.replace("svelte.jsx", "svelte.js"),
      ),
      "@chart": resolve(`src/${chart}.js`),
    },
  },
  build: { outDir: `dist/${framework}-${chart}`, emptyOutDir: true },
});
