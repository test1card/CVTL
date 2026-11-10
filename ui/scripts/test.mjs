import { chromium } from "playwright";
import { createServer } from "node:http";
import { readFile, readdir, stat, writeFile } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import assert from "node:assert/strict";
const root = resolve("dist");
const server = createServer(async (req, res) => {
  try {
    const path = resolve(
      root,
      "." + decodeURIComponent(new URL(req.url, "http://localhost").pathname),
    );
    if (!path.startsWith(root + sep)) throw Error("path");
    const file = (await stat(path)).isDirectory()
      ? resolve(path, "index.html")
      : path;
    const type =
      { ".html": "text/html", ".js": "text/javascript", ".css": "text/css" }[
        extname(file)
      ] || "application/octet-stream";
    const content = await readFile(file);
    res.writeHead(200, { "Content-Type": type });
    res.end(content);
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((resolveListen, rejectListen) => {
  server.once("error", rejectListen);
  server.listen(0, "127.0.0.1", () => {
    server.off("error", rejectListen);
    resolveListen();
  });
});
let browser;
try {
  const origin = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({
    headless: true,
    ...(process.env.CHROMIUM_PATH
      ? { executablePath: process.env.CHROMIUM_PATH }
      : {}),
  });
  const results = {
    environment: {
      node: process.version,
      browser: browser.version(),
      platform: process.platform,
      architecture: process.arch,
      samples: 30,
      metric: "click handler through second requestAnimationFrame, ms",
    },
    variants: {},
  };
  for (const framework of ["svelte", "react"]) {
    for (const library of ["uplot", "echarts"]) {
      const name = `${framework}-${library}`;
      console.log(`Testing ${name}`);
      const page = await browser.newPage({
        viewport: { width: 1100, height: 900 },
      });
      const errors = [];
      page.on("pageerror", (error) => {
        const message = `page error: ${error.stack || error.message}`;
        errors.push(message);
        console.error(`[${name}] ${message}`);
      });
      page.on("response", (response) => {
        if (!response.ok()) {
          const message = `HTTP ${response.status()}: ${response.url()}`;
          errors.push(message);
          console.error(`[${name}] ${message}`);
        }
      });
      page.on("requestfailed", (request) => {
        const message = `request failed: ${request.url()} ${request.failure()?.errorText}`;
        errors.push(message);
        console.error(`[${name}] ${message}`);
      });
      page.on("request", (r) => {
        if (!r.url().startsWith(origin + "/"))
          errors.push(`external request: ${r.url()}`);
      });
      await page.goto(`${origin}/${name}/`, { waitUntil: "networkidle" });
      try {
        await page.locator("canvas").first().waitFor();
      } catch (error) {
        throw new Error(
          `${name}: graph canvas unavailable; page errors: ${JSON.stringify(errors)}`,
          { cause: error },
        );
      }
      assert.equal(await page.locator("svg").count(), 1);
      const before = await page.locator(".plot").screenshot();
      await page.getByRole("button", { name: "Мощность" }).click();
      await page.getByRole("heading", { name: "Мощность, Вт" }).waitFor();
      assert.equal(
        await page
          .getByRole("button", { name: "Мощность" })
          .getAttribute("aria-pressed"),
        "true",
      );
      const after = await page.locator(".plot").screenshot();
      assert.notDeepEqual(before, after, "graph must change");
      const timings = [];
      for (let i = 0; i < 30; i++) {
        timings.push(
          await page.evaluate(async (i) => {
            const button = [...document.querySelectorAll("button")][i % 2];
            const t = performance.now();
            button.click();
            await new Promise((r) =>
              requestAnimationFrame(() => requestAnimationFrame(r)),
            );
            return performance.now() - t;
          }, i),
        );
      }
      assert.deepEqual(errors, []);
      const assets = await readdir(resolve(root, name, "assets"));
      let bytes = (await stat(resolve(root, name, "index.html"))).size;
      for (const asset of assets)
        bytes += (await stat(resolve(root, name, "assets", asset))).size;
      timings.sort((a, b) => a - b);
      results.variants[name] = {
        bytes,
        median_ms: (timings[14] + timings[15]) / 2,
        p95_ms: timings[28],
      };
      await page.close();
    }
  }
  console.log(JSON.stringify(results, null, 2));
  await writeFile("results.json", JSON.stringify(results, null, 2) + "\n");
} finally {
  if (browser) await browser.close();
  await new Promise((resolveClose, rejectClose) =>
    server.close((error) => (error ? rejectClose(error) : resolveClose())),
  );
}
