<script lang="ts">
  import Plot from "./Plot.svelte";
  import type { AnalysisEstimate, ArchiveReceipt, ArchiveReview, PlotSeries, Theme } from "./types";

  interface Props {
    archives: ArchiveReceipt[];
    primary: ArchiveReview | null;
    comparison: ArchiveReview | null;
    busy: boolean;
    onlist: () => void;
    onreplay: (run: string, compare: boolean) => void;
    theme: Theme;
  }

  let { archives, primary, comparison, busy, onlist, onreplay, theme }: Props = $props();
  let choice = $state("");
  let comparisonChoice = $state("");

  let run = $derived(archives.find((entry) => entry.run === choice)?.run ?? archives[0]?.run ?? "");
  let secondRuns = $derived(archives.filter((entry) => entry.run !== run));
  let secondRun = $derived(
    secondRuns.find((entry) => entry.run === comparisonChoice)?.run ?? secondRuns[0]?.run ?? "",
  );
  let mainResult = $derived(primary?.report.run === run ? primary : null);
  let secondResult = $derived(mainResult && comparison?.report.run === secondRun ? comparison : null);

  const count = new Intl.NumberFormat("ru-RU");
  const quantity = new Intl.NumberFormat("ru-RU", { maximumSignificantDigits: 5 });
  const seconds = new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 3 });
  const statuses: Record<string, string> = {
    matched: "Совпадает",
    insufficient: "Недостаточно данных",
    mismatch: "Обнаружено расхождение",
    incomplete_frame: "Неполный кадр",
    corrupt_frame: "Повреждённый кадр",
  };
  const tails: Record<string, string> = {
    frame_boundary: "Граница кадра",
    incomplete_frame: "Неполный кадр",
    corrupt_frame: "Повреждённый кадр",
  };

  function output(code: string): string {
    if (code === "off_verified") return "OFF подтверждён";
    if (code === "on_verified") return "ON подтверждён";
    return "Состояние неизвестно";
  }

  function temperatures(data: ArchiveReview): PlotSeries[] {
    return [
      { label: "Тело", color: "#0072b2", values: data.samples.map((sample) => sample.body_k) },
      { label: "Ступень 1", color: "#d55e00", values: data.samples.map((sample) => sample.temperatures_k[0]), dash: [6, 3] },
      { label: "Ступень 2", color: "#009e73", values: data.samples.map((sample) => sample.temperatures_k[1]), dash: [2, 3] },
      { label: "Насадка tf", color: "#b25293", values: data.samples.map((sample) => sample.temperatures_k[2]), dash: [8, 3, 2, 3] },
    ];
  }

  function powers(data: ArchiveReview): PlotSeries[] {
    return [
      { label: "Запрос", color: "#0072b2", values: data.samples.map((sample) => sample.requested_w), dash: [6, 3] },
      { label: "Ограничение", color: "#d55e00", values: data.samples.map((sample) => sample.limited_w), dash: [2, 3] },
      { label: "Измерено A", color: "#009e73", values: data.samples.map((sample) => sample.measured_w[0]) },
      { label: "Измерено B", color: "#b25293", values: data.samples.map((sample) => sample.measured_w[1]), dash: [8, 3, 2, 3] },
    ];
  }

  function flow(data: ArchiveReview): PlotSeries[] {
    return [
      { label: "Поток моста", color: "#0072b2", values: data.samples.map((sample) => sample.bridge_flow_w) },
      { label: "Ошибка слежения", color: "#d55e00", values: data.samples.map((sample) => sample.tracking_error_w), dash: [6, 3] },
    ];
  }
</script>

<section class="panel review-controls" aria-label="Выбор прогона">
  <div class="review-selection">
    <div class="field">
      <label for="review-run">Прогон</label>
      <select id="review-run" value={run} disabled={busy || archives.length === 0}
        onchange={(event) => choice = event.currentTarget.value}>
        {#if archives.length === 0}<option value="">Завершённых записей пока нет</option>{/if}
        {#each archives as entry (entry.run)}
          <option value={entry.run}>{entry.run}</option>
        {/each}
      </select>
    </div>
    <div class="field">
      <label for="review-compare">Второй прогон</label>
      <select id="review-compare" value={secondRun} disabled={busy || secondRuns.length === 0}
        onchange={(event) => comparisonChoice = event.currentTarget.value}>
        {#if secondRuns.length === 0}<option value="">Нет второго прогона</option>{/if}
        {#each secondRuns as entry (entry.run)}
          <option value={entry.run}>{entry.run}</option>
        {/each}
      </select>
    </div>
  </div>
  <div class="actions">
    <button type="button" onclick={() => onreplay(run, false)} disabled={busy || !run}>Проверить реплей</button>
    <button type="button" class="secondary" onclick={() => onreplay(secondRun, true)}
      disabled={busy || !mainResult || !secondRun}>Сравнить прогоны</button>
    <button type="button" class="secondary" onclick={onlist} disabled={busy}>Обновить список</button>
  </div>
  {#if busy}
    <p class="muted" role="status">Читаем записи…</p>
  {/if}
</section>

{#snippet interval(estimate: AnalysisEstimate, units: string)}
  <span class="analysis-note">5–95 % · {quantity.format(estimate.p05)} … {quantity.format(estimate.p95)}&nbsp;<span class="analysis-units">{units}</span></span>
{/snippet}

{#snippet derivative(estimate: AnalysisEstimate, point: string)}
  <div>
    <dt>Производная {point}</dt>
    <dd>
      {quantity.format(estimate.nominal)} К/с
      <span class="analysis-note" title="Стандартная неопределённость">σ {quantity.format(estimate.std_dev)} К/с</span>
      {@render interval(estimate, "К/с")}
    </dd>
  </div>
{/snippet}

{#snippet coolingAnalysis(data: ArchiveReview)}
  {@const analysis = data.analysis}
  {@const window = analysis.window}
  {@const estimate = analysis.q2_eff_w}
  <section class="analysis-result" data-testid="analysis-result" data-status={analysis.status} aria-label="Анализ холодопроизводительности">
    <h4>Холодопроизводительность Q2_eff</h4>
    {#if analysis.profile === "synthetic_test_only_a1"}
      <p class="muted">Синтетический профиль A1 · test_only</p>
    {/if}
    {#if estimate && window}
      <dl class="review-metrics">
        <div>
          <dt>Q2_eff · медиана</dt>
          <dd>
            <span data-testid="q2-estimate" data-value={estimate.median}>{quantity.format(estimate.median)} Вт</span>
            {@render interval(estimate, "Вт")}
          </dd>
        </div>
        <div>
          <dt>Окно модели</dt>
          <dd>
            {seconds.format(window.start_s)} … {seconds.format(window.end_s)} с
            <span class="analysis-note">Интервалов {count.format(window.intervals)}</span>
          </dd>
        </div>
        <div>
          <dt>Средняя мощность нагревателей</dt>
          <dd>{quantity.format(window.mean_heater_w)} Вт</dd>
        </div>
        <div>
          <dt>Средние температуры</dt>
          <dd class="analysis-temperatures">
            {#each ["stage1", "stage2", "tf", "jacket"] as point, index}
              <span>{point} {quantity.format(window.mean_temperatures_k[index])} К</span>
            {/each}
          </dd>
        </div>
        {@render derivative(window.derivative_stage2_k_per_s, "stage2")}
        {@render derivative(window.derivative_tf_k_per_s, "tf")}
      </dl>
      <p class="muted">Производные оценены при {seconds.format(window.derivative_at_s)} с модельного времени.</p>
    {:else}
      <p class="review-diagnostic"><span aria-hidden="true">!&nbsp;</span>{analysis.status}</p>
      {#if analysis.reason}<p>{analysis.reason}</p>{/if}
    {/if}
  </section>
{/snippet}

{#snippet runReport(data: ArchiveReview, heading: string, testid: string)}
  {@const report = data.report}
  {@const firstSample = data.samples[0]}
  {@const lastSample = data.samples.at(-1)}
  {@const time = data.samples.map((sample) => sample.measured_ns / 1e9)}
  <section class="panel replay-record" data-testid={testid} aria-label={heading}>
    <h3>{heading}</h3>
    <p class="muted run-id">{report.run}</p>
    <dl class="review-metrics">
      <div>
        <dt>Реплей</dt>
        <dd class:positive={report.status === "matched"} class:attention={report.status !== "matched"}>
          <span aria-hidden="true">{report.status === "matched" ? "✓" : "!"}</span>
          <span>{statuses[report.status] ?? "Требует проверки"}</span>
        </dd>
      </div>
      <div>
        <dt>Квитанция завершения</dt>
        <dd class:positive={data.receipt_verified} class:attention={!data.receipt_verified}>
          <span aria-hidden="true">{data.receipt_verified ? "✓" : "!"}</span>
          <span>{data.receipt_verified ? "Сверена" : "Расхождение"}</span>
        </dd>
      </div>
      <div>
        <dt>Выход на границе реплея</dt>
        <dd class:positive={report.output_state_at_boundary === "off_verified"} class:attention={report.output_state_at_boundary !== "off_verified"}>
          <span aria-hidden="true">{report.output_state_at_boundary === "off_verified" ? "✓" : "!"}</span>
          <span>{output(report.output_state_at_boundary)}</span>
        </dd>
      </div>
      <div>
        <dt>Выход по квитанции</dt>
        <dd>{output(data.receipt.output_state)}</dd>
      </div>
      <div>
        <dt>Хвост журнала</dt>
        <dd>{tails[report.raw_tail] ?? "Требует проверки"}</dd>
      </div>
      <div>
        <dt>Последняя сверенная запись</dt>
        <dd>{data.last_verified_seq ? `№ ${count.format(data.last_verified_seq)}` : "Нет данных"}</dd>
      </div>
      <div>
        <dt>Записей в доступных кадрах</dt>
        <dd>{count.format(report.raw_verified_records)}</dd>
      </div>
      <div>
        <dt>Последняя запись по квитанции</dt>
        <dd>{count.format(data.receipt.last_seq)}</dd>
      </div>
      <div>
        <dt>Такты графиков</dt>
        <dd>{firstSample && lastSample ? `${count.format(firstSample.tick)}–${count.format(lastSample.tick)}` : "Нет данных"}</dd>
      </div>
      <div>
        <dt>Ранних тактов вне окна</dt>
        <dd>
          {count.format(data.omitted_ticks)}
          {#if data.omitted_ticks > 0 && firstSample}до такта {count.format(firstSample.tick)}{/if}
        </dd>
      </div>
    </dl>
    {#if report.first_diagnostic}
      <p class="review-diagnostic"><span aria-hidden="true">! </span>{report.first_diagnostic.message}</p>
    {/if}
    {@render coolingAnalysis(data)}
    {#if time.length > 0}
      <p class="muted">Время от начала прогона, с</p>
      <div class="review-plots">
        <Plot {time} series={temperatures(data)} units="К" label="Температуры" {theme} height={200} />
        <Plot {time} series={powers(data)} units="Вт" label="Мощность" {theme} height={200} />
        <Plot {time} series={flow(data)} units="Вт" label="Поток и ошибка слежения" {theme} height={200} />
      </div>
    {/if}
  </section>
{/snippet}

{#if mainResult}
  <div class="review-results" class:with-comparison={Boolean(secondResult)}>
    {@render runReport(mainResult, "Основной прогон", "replay-result")}
    {#if secondResult}
      {@render runReport(secondResult, "Прогон для сравнения", "comparison-result")}
    {/if}
  </div>
{/if}

<style>
  .review-controls,
  .review-results,
  .review-plots {
    display: grid;
    gap: 1rem;
  }

  .review-selection,
  .review-metrics {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: .8rem;
  }

  .review-selection select,
  .replay-record,
  .review-plots {
    min-width: 0;
  }

  .review-selection select {
    width: 100%;
  }

  .run-id {
    overflow-wrap: anywhere;
  }

  .review-metrics {
    margin: 1rem 0;
  }

  .review-metrics div {
    background: var(--surface-muted);
    border: 1px solid var(--line);
    border-radius: .5rem;
    padding: .65rem .75rem;
  }

  .review-metrics dt {
    color: var(--muted);
    font-size: .85rem;
  }

  .review-metrics dd {
    margin: .3rem 0 0;
    color: var(--text);
    font-weight: 600;
  }

  .review-metrics dd.positive {
    color: var(--positive);
  }

  .review-metrics dd.attention,
  .review-diagnostic {
    color: var(--warning);
  }

  .analysis-result {
    margin: 1rem 0;
    padding-top: .85rem;
    border-top: 1px solid var(--line);
    overflow-wrap: anywhere;
  }

  .analysis-result h4 {
    margin: 0;
  }

  .analysis-note {
    display: block;
    margin-top: .2rem;
    color: var(--muted);
    font-size: .85rem;
    font-weight: 400;
  }

  .analysis-units {
    white-space: nowrap;
  }

  .analysis-temperatures {
    display: flex;
    flex-wrap: wrap;
    gap: .15rem .7rem;
  }

  @media (min-width: 1000px) {
    .with-comparison {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 520px) {
    .review-selection,
    .review-metrics {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
