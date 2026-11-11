import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { createInterface } from "node:readline";
import { promisify } from "node:util";
import { chromium } from "playwright";

const root = await mkdtemp(resolve(tmpdir(), "cvtl-m7-browser-"));
const binary = process.env.CVTL_DAEMON ?? resolve("../target/debug/cvtl-daemon");
const screenshotDir = process.env.CVTL_M7_SCREENSHOTS;
const execute = promisify(execFile);
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

async function cliAnalysis(run) {
  const { stdout } = await execute("cargo", [
    "run", "--locked", "--quiet", "-p", "cvtl-cli", "--", "analyze", resolve(root, "runs", run),
  ], { timeout: 30000 });
  return JSON.parse(stdout);
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

  async function checkHistoricalHead() {
    const panel = page.getByRole("region", { name: "Историческая характеристика головки", exact: true });
    const calculate = panel.getByRole("button", { name: "Рассчитать характеристику", exact: true });
    const historicalUrl = `${origin}/api/historical-head`;
    assert.equal((await snapshot()).run, null);
    assert.ok(await button("Подготовить прогон").isDisabled());

    const failure = "Учебный сбой расчёта характеристики";
    await page.route(historicalUrl, (route) => route.fulfill({ status: 503, json: { error: failure } }), { times: 1 });
    await calculate.click();
    const alert = panel.getByRole("alert");
    await alert.waitFor();
    assert.equal(await alert.textContent(), failure);
    assert.equal(await panel.getByRole("table").count(), 0);

    const hold = Promise.withResolvers();
    await page.route(historicalUrl, async (route) => {
      await hold.promise;
      await route.continue();
    }, { times: 1 });
    const [response] = await Promise.all([
      page.waitForResponse((result) => result.url() === historicalUrl),
      (async () => {
        try {
          await calculate.click();
          await panel.getByRole("status").waitFor();
          assert.ok(await calculate.isDisabled());
        } finally {
          hold.resolve();
        }
      })(),
    ]);
    assert.equal(response.request().method(), "GET");
    assert.ok(response.ok());
    const apiBytes = await response.body();
    const report = JSON.parse(apiBytes.toString("utf8"));
    assert.equal(report.synthetic, true);
    assert.equal(report.test_only, true);
    assert.deepEqual(report.nominal_rows, [1, 2, 3, 4, 5, 6]);
    assert.equal(report.liquid_nitrogen, false);
    assert.equal(report.radiation_shield, false);
    assert.equal(report.parasitics_removed, false);
    assert.equal(report.points.length, 15);
    assert.equal(report.continuation_to_k, 300);

    const table = panel.getByRole("table");
    await table.waitFor();
    assert.ok(await calculate.isEnabled());
    assert.equal(await alert.count(), 0);
    assert.deepEqual(await table.getByRole("columnheader").allTextContents(), ["stage2, К", "stage1, К", "P_net, Вт", "Происхождение"]);
    await panel.getByText(report.source_path, { exact: true }).waitFor();
    await panel.getByText("Опыт этапа I проходил без жидкого азота и экрана. P_net — подведённая мощность с невычтенными паразитными притоками.").waitFor();
    const number = new Intl.NumberFormat("ru-RU", { maximumSignificantDigits: 5 });
    const originLabel = {
      historical_anchor: "Опора этапа I",
      interpolated: "Интерполяция",
      synthetic: "Синтетическое продолжение",
    };
    const expectedRows = report.points.map((point) => [
      number.format(point.stage2_k),
      number.format(point.stage1_k),
      number.format(point.p_net_w),
      originLabel[point.origin] + (point.source_row === null ? "" : ` · строка ${point.source_row}`),
    ]);
    const rows = table.locator("tbody tr");
    const renderedRows = () => rows.evaluateAll((elements) => elements.map((row) =>
      Array.from(row.cells, (cell) => cell.textContent.trim())));
    assert.deepEqual(await renderedRows(), expectedRows);
    await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1);

    const [download] = await Promise.all([
      page.waitForEvent("download"),
      panel.getByRole("link", { name: "Скачать JSON", exact: true }).click(),
    ]);
    assert.equal(download.suggestedFilename(), "cvtl-historical-head.json");
    const downloadedBytes = await readFile(await download.path());
    assert.deepEqual(downloadedBytes, apiBytes);
    if (screenshotDir) {
      await writeFile(resolve(screenshotDir, "historical-head.json"), downloadedBytes);
      await panel.screenshot({ path: resolve(screenshotDir, "historical-head.png") });
    }

    await page.setViewportSize({ width: 360, height: 820 });
    await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1);
    assert.ok(await panel.locator(".table-wrap").evaluate((wrapper) => wrapper.scrollWidth <= wrapper.clientWidth + 1));
    assert.ok(await rows.locator("td:nth-child(-n+3)").evaluateAll((cells) => cells.every((cell) => {
      const contents = document.createRange();
      contents.selectNodeContents(cell);
      return contents.getClientRects().length === 1;
    })));
    assert.deepEqual(await renderedRows(), expectedRows);
    const mobileWidth = await page.evaluate(() => innerWidth);
    if (screenshotDir) await panel.screenshot({ path: resolve(screenshotDir, "historical-head-mobile.png") });
    await page.setViewportSize({ width: 1280, height: 920 });
    assert.ok(await button("Подготовить прогон").isDisabled());
    assert.equal((await snapshot()).run, null);
    return {
      points: report.points.length,
      continuation_to_k: report.continuation_to_k,
      source_sha256: report.source_sha256,
      download_matches: true,
      mobile_width: mobileWidth,
    };
  }

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
  const historicalHead = await checkHistoricalHead();
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
  await review.getByTestId("q2-estimate").waitFor();
  const primaryAnalysis = await page.evaluate(() => fetch("/api/job")
    .then((response) => response.json()).then((job) => job.value.analysis));
  assert.equal(primaryAnalysis.status, "определяется");
  assert.ok(primaryAnalysis.q2_eff_w.p05 <= 4 && primaryAnalysis.q2_eff_w.p95 >= 4);
  assert.deepEqual(primaryAnalysis, await cliAnalysis(first));
  await page.getByLabel("Второй прогон", { exact: true }).selectOption(second);
  await button("Сравнить прогоны").click();
  const comparison = page.getByTestId("comparison-result");
  await comparison.getByText("Совпадает", { exact: true }).waitFor();
  await comparison.locator('[data-testid="analysis-result"][data-status="не определяется"]').waitFor();
  const shortAnalysis = await page.evaluate(() => fetch("/api/job")
    .then((response) => response.json()).then((job) => job.value.analysis));
  assert.equal(shortAnalysis.q2_eff_w, null);
  assert.deepEqual(shortAnalysis, await cliAnalysis(second));
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
    historical_head: historicalHead,
    first_run_ticks: live.samples_seen,
    live_window: live.samples.length,
    stop_to_off_ms: Math.round(stopMs),
    completed_nominal_runs: [first, second],
    analysis_q2_w: primaryAnalysis.q2_eff_w,
    short_analysis_status: shortAnalysis.status,
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
