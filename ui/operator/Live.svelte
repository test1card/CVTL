<script lang="ts">
  import Plot from "./Plot.svelte";
  import type { ChangePayload, PlotSeries, RunView, Theme } from "./types";

  let { run, busy, onapply, theme }: {
    run: RunView;
    busy: boolean;
    onapply: (change: ChangePayload) => void;
    theme: Theme;
  } = $props();

  const number = new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 3 });
  const clock = new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 2 });
  const format = (value: number | null | undefined, unit = "") =>
    value == null ? "—" : number.format(value) + (unit ? " " + unit : "");
  const sum = (values: (number | null)[]) =>
    values.every((value) => value !== null) ? values.reduce<number>((total, value) => total + (value ?? 0), 0) : null;

  const eventLabels: Record<string, string> = {
    applied: "Параметры применены",
    arm: "Команда допуска",
    start: "Команда запуска",
    stop: "Команда остановки",
    reset: "Команда сброса",
    off_verified: "OFF подтверждён",
    on_verified: "ON подтверждён",
    unknown: "Выход не подтверждён",
    source_link_lost: "Связь с источником потеряна",
    thermometer_link_lost: "Связь с термометром потеряна",
    measurement_unavailable: "Измерение недоступно",
    power_measurement_stale: "Измерение мощности устарело",
    measurement_stale: "Измерение устарело",
    temperature_stale: "Температура устарела",
    head_approaching: "Головка близка к порогу",
    head_inversion: "Инверсия температур",
    head_absolute_temperature: "Температурный предел головки",
    head_rate: "Предел скорости охлаждения",
    head_power: "Предел нагрузки головки",
    output_unconfirmed: "Выход не подтверждён",
    journal_failure: "Сбой записи журнала",
    controller_failure: "Сбой регулятора",
    branch_lost: "Потеря ветви",
    total_power: "Предел суммарной мощности",
    branch_current: "Предел тока ветви",
    branch_voltage: "Предел напряжения ветви",
    branch_power: "Предел мощности ветви",
    missed_tick: "Пропуск такта",
    model_failure: "Сбой тепловой модели",
  };

  let viewport = $state(1280);
  const narrow = $derived(viewport < 700);
  const coordinates = $derived(narrow ? {
    viewBox: "0 0 350 419",
    body: [103, 88],
    tf: [103, 218],
    stage2: [103, 298],
    stage1: [103, 378],
    branchA: [261, 192],
    branchB: [261, 244],
  } : {
    viewBox: "0 0 780 248",
    body: [105, 95],
    tf: [390, 95],
    stage2: [548, 95],
    stage1: [707, 95],
    branchA: [335, 196],
    branchB: [445, 196],
  });

  let newMass = $state<number | undefined>(undefined);
  let newScale = $state<number | undefined>(undefined);
  let newG = $state<number | undefined>(undefined);

  const latest = $derived(run.samples.at(-1));
  const measuredTotal = $derived(latest ? sum(latest.measured_w) : null);
  const flow = $derived(latest?.bridge_flow_w ?? null);
  const flowDirection = $derived(flow === null ? "Поток недоступен" : flow > 0 ? "Тело → tf" : flow < 0 ? "tf → тело" : "Нулевой поток");
  const outputText = $derived(run.output_state === "on_verified" ? "ON подтверждён" :
    run.output_state === "off_verified" ? "OFF подтверждён" : "Выход не подтверждён");
  const outputIcon = $derived(run.output_state === "unknown" ? "△" : "✓");
  const radius = $derived(Math.min(62, Math.max(30, 34 + 4 * Math.sqrt(run.parameters.mass_kg))));
  const bodyEdge = $derived(narrow
    ? [coordinates.body[0], coordinates.body[1] + radius]
    : [coordinates.body[0] + radius, coordinates.body[1]]);
  const tfEdge = $derived(narrow
    ? [coordinates.tf[0], coordinates.tf[1] - 25]
    : [coordinates.tf[0] - 53, coordinates.tf[1]]);
  const bridgeStroke = $derived(flow === null ? 2 : Math.min(10, 2 + Math.abs(flow) * 2));
  const bodyShade = $derived(latest?.body_k == null ? "var(--surface-muted, #eef3f7)" :
    latest.body_k > (latest.temperatures_k[2] ?? latest.body_k)
      ? "color-mix(in srgb, #db9f44 22%, var(--surface, #fff))"
      : "color-mix(in srgb, #4599c8 24%, var(--surface, #fff))");

  const editable = $derived(run.state === "running" && run.output_state === "on_verified" && run.journal === "recording");
  const supportsG = $derived(run.parameters.g_w_per_k !== null);
  const validChange = $derived(
    (newMass !== undefined || newScale !== undefined || (supportsG && newG !== undefined)) &&
    (newMass === undefined || (Number.isFinite(newMass) && newMass > 0)) &&
    (newScale === undefined || (Number.isFinite(newScale) && newScale > 0 && newScale <= 1)) &&
    (!supportsG || newG === undefined || (Number.isFinite(newG) && newG >= 0))
  );

  function apply(event: SubmitEvent) {
    event.preventDefault();
    if (!editable || busy || !validChange) return;
    const change: ChangePayload = {};
    if (newMass !== undefined) change.mass_kg = newMass;
    if (newScale !== undefined) change.scale = newScale;
    if (supportsG && newG !== undefined) change.g_w_per_k = newG;
    onapply(change);
  }

  const reasons = $derived([
    latest?.power_limited ? "по мощности" : null,
    latest?.current_limited ? "по току" : null,
    latest?.slew_limited ? "по скорости изменения" : null,
  ].filter((item) => item !== null));
  const fault = $derived(run.fault ? eventLabels[run.fault] ?? run.fault : null);

  const palette = $derived(theme === "dark"
    ? ["#56b4e9", "#e6ad50", "#5ec6a9", "#e894c6"]
    : ["#0072b2", "#a15f00", "#087d66", "#a34181"]);
  const dashes = [undefined, [8, 4], [3, 3], [10, 3, 2, 3]];
  const time = $derived(run.samples.map((row) => row.measured_ns / 1e9));
  function line(label: string, index: number, values: (number | null)[]): PlotSeries {
    return { label, color: palette[index], dash: dashes[index], values };
  }
  const temperatures = $derived([
    line("stage1", 0, run.samples.map((row) => row.temperatures_k[0])),
    line("stage2", 1, run.samples.map((row) => row.temperatures_k[1])),
    line("tf", 2, run.samples.map((row) => row.temperatures_k[2])),
    line("Виртуальное тело", 3, run.samples.map((row) => row.body_k)),
  ]);
  const inversion = $derived([
    line("stage2 − stage1", 0, run.samples.map((row) => row.inversion_delta_k)),
    line("Предупреждение", 1, time.map(() => run.estimate.limits.inversion_k[0])),
    line("Снижение", 2, time.map(() => run.estimate.limits.inversion_k[1])),
    line("Остановка", 3, time.map(() => run.estimate.limits.inversion_k[2])),
  ]);
  const totalPower = $derived([
    line("Запрос", 0, run.samples.map((row) => row.requested_w)),
    line("После ограничений", 1, run.samples.map((row) => row.limited_w)),
    line("Измерено", 2, run.samples.map((row) => sum(row.measured_w))),
  ]);
  const branchPower = $derived([
    line("A измерено", 0, run.samples.map((row) => row.measured_w[0])),
    line("B измерено", 1, run.samples.map((row) => row.measured_w[1])),
    line("A цель", 2, run.samples.map((row) => row.targets_w?.[0] ?? null)),
    line("B цель", 3, run.samples.map((row) => row.targets_w?.[1] ?? null)),
  ]);
  const bridgeAndError = $derived([
    line("Знаковый поток моста", 0, run.samples.map((row) => row.bridge_flow_w)),
    line("Измерено − запрос", 1, run.samples.map((row) => row.tracking_error_w)),
  ]);
</script>

<svelte:window bind:innerWidth={viewport} />

<div class="live-layout">
  <h1>Ход опыта</h1>
  <figure class="panel schematic">
    <figcaption>
      <h2>Тепловая схема</h2>
      <span class:unknown={run.output_state === "unknown"} class="output-state">
        <span aria-hidden="true">{outputIcon}</span> {outputText}
      </span>
    </figcaption>
    <svg data-testid="live-svg" role="img" aria-label="Виртуальное тело, мост, головка и нагреватели"
      viewBox={coordinates.viewBox} class="thermal-scheme">
      <title>Тепловая схема текущего прогона</title>
      <defs>
        <marker id="live-flow-arrow" viewBox="0 0 10 10" refX="9" refY="5"
          markerWidth="3" markerHeight="3" orient="auto-start-reverse">
          <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--accent, #096ca1)" />
        </marker>
      </defs>
      <line class="thermal-link" x1={coordinates.tf[0]} y1={coordinates.tf[1]}
        x2={coordinates.stage2[0]} y2={coordinates.stage2[1]} />
      <line class="thermal-link" x1={coordinates.stage2[0]} y1={coordinates.stage2[1]}
        x2={coordinates.stage1[0]} y2={coordinates.stage1[1]} />
      {#each [coordinates.branchA, coordinates.branchB] as branch}
        <line class="heater-link" x1={branch[0]} y1={branch[1]} x2={coordinates.tf[0]} y2={coordinates.tf[1]} />
      {/each}
      <line class="bridge-link" x1={flow !== null && flow < 0 ? tfEdge[0] : bodyEdge[0]}
        y1={flow !== null && flow < 0 ? tfEdge[1] : bodyEdge[1]}
        x2={flow !== null && flow < 0 ? bodyEdge[0] : tfEdge[0]}
        y2={flow !== null && flow < 0 ? bodyEdge[1] : tfEdge[1]}
        stroke-width={bridgeStroke} stroke-dasharray={flow === null ? "5 5" : undefined}
        marker-end={flow !== null && flow !== 0 ? "url(#live-flow-arrow)" : undefined} />
      <circle cx={coordinates.body[0]} cy={coordinates.body[1]} r={radius}
        fill={bodyShade} class="body-node" />
      <text x={coordinates.body[0]} y={coordinates.body[1] - 5} class="node-label">Тело</text>
      <text x={coordinates.body[0]} y={coordinates.body[1] + 17} class="node-value">
        {format(latest?.body_k, "К")}
      </text>
      <text x={coordinates.body[0]} y={coordinates.body[1] + 35} class="detail-label">
        {format(run.parameters.mass_kg, "кг")}
      </text>

      {#each [
        {name: "tf", pos: coordinates.tf, temperature: latest?.temperatures_k[2]},
        {name: "stage2", pos: coordinates.stage2, temperature: latest?.temperatures_k[1]},
        {name: "stage1", pos: coordinates.stage1, temperature: latest?.temperatures_k[0]},
      ] as node (node.name)}
        <rect x={node.pos[0] - 53} y={node.pos[1] - 25} width="106" height="50"
          rx="9" class="thermal-node" />
        <text x={node.pos[0]} y={node.pos[1] - 3} class="node-label">{node.name}</text>
        <text x={node.pos[0]} y={node.pos[1] + 16} class="node-value">{format(node.temperature, "К")}</text>
      {/each}
      {#each [{name: "A", pos: coordinates.branchA}, {name: "B", pos: coordinates.branchB}] as branch, index (branch.name)}
        <rect x={branch.pos[0] - 40} y={branch.pos[1] - 20} width="80" height="40"
          rx="8" class="heater-node" />
        <text x={branch.pos[0]} y={branch.pos[1] - 2} class="node-label">{branch.name}</text>
        <text x={branch.pos[0]} y={branch.pos[1] + 14} class="detail-label">{format(latest?.measured_w[index], "Вт")}</text>
      {/each}
    </svg>
    <p class="scheme-caption">
      <strong>{flowDirection}</strong>
      {#if flow !== null}<span>{format(flow, "Вт")}</span>{/if}
    </p>
  </figure>

  <section class="panel power-panel">
    <h2>Мощность</h2>
    <dl class="power-summary">
      <div><dt>Запрос</dt><dd>{format(latest?.requested_w, "Вт")}</dd></div>
      <div><dt>После ограничений</dt><dd>{format(latest?.limited_w, "Вт")}</dd></div>
      <div><dt>Измерено</dt><dd>{format(measuredTotal, "Вт")}</dd></div>
    </dl>
    <p class="limit-reason">
      {#if fault}<strong class="fault" role="status">△ {fault}</strong>
      {:else if reasons.length}<strong>Ограничение {reasons.join(", ")}</strong>
      {:else if latest?.requested_w != null}<span class="muted">Ограничений нет</span>
      {:else}<span class="muted">Запрос мощности отсутствует</span>{/if}
    </p>
    <div class="table-wrap">
      <table class="branch-table">
        <thead>
          <tr><th scope="col">Ветвь</th><th scope="col">Цель, Вт</th><th scope="col">Измерено, Вт</th><th scope="col">Выход</th></tr>
        </thead>
        <tbody>
          {#each ["A", "B"] as branch, index (branch)}
            <tr>
              <th scope="row">{branch}</th>
              <td>{format(latest?.targets_w?.[index])}</td>
              <td>{format(latest?.measured_w[index])}</td>
              <td><span aria-hidden="true">{outputIcon}</span> {outputText}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>

  <section class="panel parameter-panel">
    <h2>Параметры тела и моста</h2>
    <dl class="parameter-values">
      <div><dt>Масса</dt><dd>{format(run.parameters.mass_kg, "кг")}</dd></div>
      <div><dt>scale</dt><dd>{format(run.parameters.scale)}</dd></div>
      <div><dt>G</dt><dd>{supportsG ? format(run.parameters.g_w_per_k, "Вт/К") : "Мост с переменной проводимостью"}</dd></div>
    </dl>
    <form onsubmit={apply}>
      <div class="form-grid">
        <label class="field">
          Новая масса, кг
          <input type="number" min="0" step="any" bind:value={newMass} disabled={!editable || busy} />
        </label>
        <label class="field">
          Новый scale
          <input type="number" min="0" max="1" step="any" bind:value={newScale} disabled={!editable || busy} />
        </label>
        <label class="field">
          Новый G, Вт/К
          <input type="number" min="0" step="any" bind:value={newG} disabled={!editable || busy || !supportsG} />
        </label>
      </div>
      <div class="actions">
        <button type="submit" disabled={!editable || busy || !validChange}>Применить параметры</button>
        <p data-testid="applied-change" data-seq={run.last_change?.seq} aria-live="polite">
          {#if run.last_change}
            Применено на такте {run.last_change.tick}, запись №{run.last_change.seq}
          {:else}
            Журнал пока не подтверждал смену параметров
          {/if}
        </p>
      </div>
    </form>
  </section>

  <section class="live-plots" aria-label="Графики прогона">
    <header class="plot-heading">
      <h2>Измерения и расчёт</h2>
      <span data-testid="live-points" data-total={run.samples_seen} data-window={run.samples.length}>
        Тактов {run.samples_seen} · в окне {run.samples.length}
      </span>
    </header>
    <Plot {time} series={temperatures} units="К" label="Температуры" {theme} />
    <Plot {time} series={inversion} units="К" label="Разность температур и пороги M5" {theme} />
    <Plot {time} series={totalPower} units="Вт" label="Суммарная мощность" {theme} />
    <Plot {time} series={branchPower} units="Вт" label="Мощность по ветвям" {theme} />
    <div class="wide-plot">
      <Plot {time} series={bridgeAndError} units="Вт" label="Поток по мосту и ошибка" {theme} />
    </div>
  </section>

  <section class="panel event-panel">
    <h2>События журнала</h2>
    {#if run.events.length}
      <div class="table-wrap">
        <table>
          <thead><tr><th scope="col">Запись</th><th scope="col">Время, с</th><th scope="col">Событие</th></tr></thead>
          <tbody>
            {#each [...run.events].reverse() as event (event.seq)}
              <tr><td>№{event.seq}</td><td>{clock.format(event.at_ns / 1e9)}</td><td>{eventLabels[event.code] ?? event.code}</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else}
      <p class="muted">События появятся после первого подтверждения журнала.</p>
    {/if}
  </section>
</div>

<style>
  .live-layout { display: grid; gap: 1rem; min-width: 0; }
  h1 { margin: 0; }
  h2 { margin: 0; font-size: 1.12rem; }
  .schematic { margin: 0; }
  .schematic figcaption, .plot-heading {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    flex-wrap: wrap;
    gap: .5rem 1rem;
  }
  .output-state { font-weight: 650; color: var(--positive, #24684a); }
  .output-state.unknown, .fault { color: var(--danger, #a03234); }
  .thermal-scheme { display: block; width: 100%; max-height: 425px; margin: .55rem auto 0; }
  .thermal-scheme text { text-anchor: middle; fill: var(--text, #182d3b); }
  .thermal-node, .heater-node {
    fill: var(--surface, #fff);
    stroke: var(--line, #bfccd6);
    stroke-width: 1.5;
  }
  .heater-node { fill: var(--surface-muted, #eef3f7); }
  .body-node { stroke: var(--accent, #096ca1); stroke-width: 1.5; }
  .thermal-link, .heater-link { stroke: var(--muted, #516776); stroke-width: 1.5; }
  .heater-link { stroke-dasharray: 4 3; }
  .bridge-link { stroke: var(--accent, #096ca1); stroke-linecap: round; }
  .node-label { font-weight: 700; font-size: 16px; }
  .node-value { font-weight: 600; font-size: 15px; }
  .detail-label { font-size: 12px; }
  .scheme-caption {
    display: flex;
    justify-content: center;
    gap: .6rem;
    margin: .2rem 0 0;
    color: var(--muted, #516776);
    font-size: .85rem;
  }
  .scheme-caption strong { color: var(--text, #182d3b); }
  .power-summary, .parameter-values {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: .65rem;
    margin: .8rem 0;
  }
  .power-summary div {
    min-width: 0;
    padding: .65rem .75rem;
    border: 1px solid var(--line, #bfccd6);
    border-radius: .6rem;
    background: var(--surface-muted, #eef3f7);
  }
  dt { font-size: .8rem; color: var(--muted, #516776); }
  dd { margin: .2rem 0 0; font-variant-numeric: tabular-nums; font-weight: 700; }
  .power-summary dd { font-size: clamp(1rem, 2.5vw, 1.55rem); }
  .limit-reason { margin: .65rem 0; font-size: .9rem; }
  .live-plots { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1rem; min-width: 0; }
  .plot-heading, .wide-plot { grid-column: 1 / -1; }
  .plot-heading span { color: var(--muted, #516776); font-size: .86rem; }
  .parameter-panel form { margin-top: .8rem; }
  .parameter-panel .actions p { margin: 0; font-size: .85rem; }
  .event-panel .table-wrap { margin-top: .7rem; }
  .branch-table th:last-child, .branch-table td:last-child { min-width: 11rem; }
  @media (max-width: 850px) { .live-plots { grid-template-columns: minmax(0, 1fr); } }
  @media (max-width: 500px) {
    .power-summary { gap: .35rem; }
    .power-summary div { padding: .45rem; }
    .power-summary dt { font-size: .72rem; }
    .parameter-values { grid-template-columns: repeat(2, minmax(0, 1fr)); }
    .branch-table th, .branch-table td { padding: .35rem .4rem; }
    .branch-table th:last-child, .branch-table td:last-child { min-width: 0; }
  }
</style>
