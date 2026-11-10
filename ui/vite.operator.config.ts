import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

const root = fileURLToPath(new URL("./operator", import.meta.url));
const outDir = fileURLToPath(new URL("./dist/operator", import.meta.url));

export default defineConfig({
  root,
  base: "/",
  build: { outDir, emptyOutDir: true },
  plugins: [
    svelte(),
    {
      name: "cvtl-uplot-number-locale",
      enforce: "pre",
      transform(source, id) {
        if (!id.endsWith("/uplot/dist/uPlot.esm.js")) return;
        const initialization = "new Intl.NumberFormat(domEnv ? nav.language : 'en-US')";
        if (!source.includes(initialization)) this.error("Проверьте инициализацию числовой локали uPlot");
        return source.replace(initialization, 'new Intl.NumberFormat("ru-RU")');
      },
    },
  ],
});
