import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { createInterface } from "node:readline";
import { chromium } from "playwright";

const root = await mkdtemp(resolve(tmpdir(), "cvtl-m7-browser-"));
const binary = process.env.CVTL_DAEMON ?? resolve("../target/debug/cvtl-daemon");
const screenshotDir = process.env.CVTL_M7_SCREENSHOTS;
const args = ["serve", "--assets", resolve("dist/operator"), "--runs", resolve(root, "runs"), "--port", "0", "--test-faults"];
const daemon = spawn(binary, args, { stdio: ["ignore", "pipe", "pipe"] });
let diagnostics = "";
daemon.stderr.on("data", (chunk) => diagnostics += String(chunk));
const lines = createInterface({ input: daemon.stdout });

function startup() {
  return new Promise((resolveUrl, reject) => {
    const timeout = setTimeout(() => reject(new Error("Демон не открыл локальный порт")), 5000);
    daemon.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
    daemon.once("exit", (code) => {
      clearTimeout(timeout);
      reject(new Error(`Демон завершился (${code}). ${diagnostics}`));
    });
    lines.once("line", (line) => {
      clearTimeout(timeout);
      assert.match(line, /^http:\/\/127\.0\.0\.1:\d+$/);
      resolveUrl(line);
    });
  });
}

let browser;
try {
  const origin = await startup();
  if (screenshotDir) await mkdir(screenshotDir, { recursive: true });
  browser = await chromium.launch({
    headless: true,
    ...(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {}),
  });
  const context = await browser.newContext({ locale: "ru-RU", viewport: { width: 1280, height: 920 } });
  await context.addInitScript(() => {
    Object.defineProperty(navigator, "language", { configurable: true, get: () => "en-US@posix" });
  });
  const page = await context.newPage();
  page.setDefaultTimeout(8000);
  const pageErrors = [];
  const foreignRequests = [];
  page.on("pageerror", (error) => pageErrors.push(String(error)));
  context.on("request", (request) => {
    if (new URL(request.url()).origin !== origin) foreignRequests.push(request.url());
  });

  const button = (name) => page.getByRole("button", { name, exact: true });
  const screen = (name) => button(name).click();
  const output = (value) => page.locator(`[data-testid="output-state"][data-state="${value}"]`).waitFor();
  const journal = (value) => page.locator(`[data-testid="journal-state"][data-state="${value}"]`).waitFor();
  const snapshot = () => page.evaluate(() => fetch("/api/state").then((response) => response.json()));
  const shot = async (name) => {
    if (screenshotDir) await page.screenshot({ path: resolve(screenshotDir, name + ".png"), fullPage: true });
  };

  async function prepare(capture = false) {
    await screen("Сценарий");
    await button("Рассчитать прогноз").click();
    await page.waitForFunction(() => document.querySelectorAll("canvas").length >= 1);
    if (capture) await shot("scenario");
    await button("Подготовить прогон").click();
    await page.getByRole("heading", { name: "Проверка перед запуском", exact: true }).waitFor();
    await output("off_verified");
    await page.waitForFunction(() => [...document.querySelectorAll("button")].some((button) => {
      return button.textContent === "Допустить" && !button.disabled;
    }));
    if (capture) await shot("preflight");
    return (await snapshot()).run.id;
  }

  async function start(checkCancel = false) {
    await button("Допустить").click();
    await button("Пуск").click();
    const dialog = page.getByRole("dialog");
    await dialog.waitFor();
    if (checkCancel) {
      await page.keyboard.press("Escape");
      await dialog.waitFor({ state: "hidden" });
      await output("off_verified");
      await button("Пуск").click();
    }
    await dialog.getByRole("button", { name: "Подтвердить пуск", exact: true }).click();
    await output("on_verified");
    await page.getByTestId("live-svg").waitFor();
  }

  async function stop() {
    const started = performance.now();
    await button("Стоп").click();
    await output("off_verified");
    const latency = performance.now() - started;
    await journal("completed");
    return latency;
  }

  await page.goto(origin);
  await page.getByText("Симуляция", { exact: true }).waitFor();
  const first = await prepare(true);
  await start(true);
  await page.getByLabel("Новая масса, кг", { exact: true }).fill("16");
  await page.getByLabel("Новый scale", { exact: true }).fill("0.15");
  await page.getByLabel("Новый G, Вт/К", { exact: true }).fill("0.04");
  await button("Применить параметры").click();
  await page.locator('[data-testid="applied-change"][data-seq]').waitFor();
  const change = (await snapshot()).run.last_change;
  assert.equal(change.change.mass_kg, 16);
  assert.equal(change.change.scale, 0.15);
  assert.equal(change.change.bridge_w_per_k, 0.04);
  assert.ok(change.seq > 0);
  await page.waitForFunction(() => {
    const points = document.querySelector('[data-testid="live-points"]');
    return Number(points?.dataset.total) >= 250 && Number(points?.dataset.window) === 240;
  }, undefined, { timeout: 65000 });
  assert.ok(await page.locator("canvas").count() >= 4);
  const live = (await snapshot()).run;
  assert.ok(live.samples_seen >= 250);
  assert.equal(live.samples.length, 240);
  await shot("live");
  const stopMs = await stop();

  const second = await prepare();
  assert.notEqual(first, second);
  await start();
  await page.waitForFunction(() => Number(document.querySelector('[data-testid="live-points"]')?.dataset.total) >= 10);
  await stop();

  await screen("Разбор");
  await button("Обновить список").click();
  await page.getByLabel("Прогон", { exact: true }).selectOption(first);
  await button("Проверить реплей").click();
  const review = page.getByTestId("replay-result");
  await review.getByText("Совпадает", { exact: true }).waitFor();
  await page.getByLabel("Второй прогон", { exact: true }).selectOption(second);
  await button("Сравнить прогоны").click();
  await page.getByTestId("comparison-result").getByText("Совпадает", { exact: true }).waitFor();
  await shot("replay");

  await screen("Оборудование и настройки");
  await button("Тёмная тема").click();
  await page.locator('html[data-theme="dark"]').waitFor();
  await shot("settings");

  await page.setViewportSize({ width: 360, height: 820 });
  for (const name of ["Сценарий", "Проверка перед запуском", "Ход опыта", "Разбор", "Оборудование и настройки"]) {
    await screen(name);
    await page.getByRole("heading", { name, exact: true }).waitFor();
    await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1);
  }
  await page.setViewportSize({ width: 1280, height: 920 });

  const faultRun = await prepare();
  await start();
  await screen("Оборудование и настройки");
  await button("Учебный отказ").click();
  await output("unknown");
  await screen("Проверка перед запуском");
  await button("Восстановить связь").click();
  await output("off_verified");
  await button("Сбросить аварию").click();
  await page.getByRole("dialog").getByRole("button", { name: "Подтвердить сброс", exact: true }).click();
  await button("Допустить").waitFor();
  await page.waitForFunction(async () => {
    const result = await fetch("/api/state").then((response) => response.json());
    return result.run?.state === "output_off_verified" && result.run.fault === null;
  });
  await context.setOffline(true);
  await page.getByText("Связь потеряна. Последние данные устарели", { exact: false }).waitFor();
  await context.setOffline(false);
  await page.reload();
  await output("off_verified");
  assert.equal((await snapshot()).run.state, "output_off_verified");
  await mkdir(resolve(root, "runs", faultRun, "completed.json"));
  await stop();
  await page.getByTestId("archive-error").waitFor();

  assert.deepEqual(pageErrors, []);
  assert.deepEqual(foreignRequests, []);
  console.log(JSON.stringify({
    node: process.version,
    browser: browser.version(),
    platform: process.platform,
    architecture: process.arch,
    configured_locale: "en-US@posix",
    first_run_ticks: live.samples_seen,
    live_window: live.samples.length,
    stop_to_off_ms: Math.round(stopMs),
    completed_nominal_runs: [first, second],
    fault_recovery: "unknown → off_verified → reset",
  }, null, 2));
} finally {
  if (browser) await browser.close();
  lines.close();
  if (daemon.exitCode === null && daemon.signalCode === null && daemon.pid) {
    daemon.kill();
    await once(daemon, "exit");
  }
  await rm(root, { recursive: true, force: true });
}
