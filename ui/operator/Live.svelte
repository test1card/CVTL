<script lang="ts">
  import Plot from "./Plot.svelte";
  import { bodyMaterials, bridgeFields } from "./Scenario.svelte";
  import { bridgeLabels, modeChoice, modeLabels, resultReasons } from "./types";
  import type { BridgeSetup, ChangePayload, PlotSeries, RunView, Theme } from "./types";

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
  let newMaterial = $state("");
  let newBridge = $state<BridgeSetup | null>(null);

  const latest = $derived(run.samples.at(-1));
  const mode = $derived(modeChoice(run.mode));
  const hasBody = $derived(!!run.parameters);
  const port = $derived(run.mode?.kind === "virtual_body" ? run.mode.port ?? "tf" : "tf");
  const compensation = $derived(latest?.mode_v1?.compensation);
  const modeChange = $derived(run.last_mode_change);
  const appliedChangeSeq = $derived(modeChange?.change.outcome.status === "applied" ? modeChange.seq : null);
  const measuredTotal = $derived(latest ? sum(latest.measured_w) : null);
  const flow = $derived(latest?.bridge_flow_w ?? null);
  const flowDirection = $derived(flow === null ? "Поток недоступен" : flow > 0 ? `Тело → ${port}` : flow < 0 ? `${port} → тело` : "Нулевой поток");
  const outputText = $derived(run.output_state === "on_verified" ? "ON подтверждён" :
    run.output_state === "off_verified" ? "OFF подтверждён" : "Выход не подтверждён");
  const outputIcon = $derived(run.output_state === "unknown" ? "△" : "✓");
  const radius = $derived(Math.min(62, Math.max(30, 34 + 4 * Math.sqrt(run.parameters?.mass_kg ?? 0))));
  const bridgePoints = $derived.by(() => {
    const { body, tf, stage2 } = coordinates;
    const points = port === "tf" ? (narrow
      ? [[body[0], body[1] + radius], [tf[0], tf[1] - 25]]
      : [[body[0] + radius, body[1]], [tf[0] - 53, tf[1]]]) : (narrow
      ? [[body[0] - radius, body[1]], [19, body[1]], [19, stage2[1]], [stage2[0] - 53, stage2[1]]]
      : [[body[0] + radius, body[1]], [245, 36], [455, 36], [stage2[0] - 53, stage2[1]]]);
    if (flow !== null && flow < 0) points.reverse();
    return points.map((point) => point.join(",")).join(" ");
  });
  const bridgeStroke = $derived(flow === null ? 2 : Math.min(10, 2 + Math.abs(flow) * 2));
  const bodyShade = $derived(latest?.body_k == null ? "var(--surface-muted, #eef3f7)" :
    latest.body_k > (latest.temperatures_k[port === "stage2" ? 1 : 2] ?? latest.body_k)
      ? "color-mix(in srgb, #db9f44 22%, var(--surface, #fff))"
      : "color-mix(in srgb, #4599c8 24%, var(--surface, #fff))");

  const editable = $derived(hasBody && run.state === "running" && run.output_state === "on_verified" && run.journal === "recording");
  const supportsG = $derived(mode === "a1" && run.parameters?.g_w_per_k != null);
  const validChange = $derived(
    (newMass !== undefined || newScale !== undefined || (supportsG && newG !== undefined) || newMaterial !== "" || newBridge !== null) &&
    (newMass === undefined || (Number.isFinite(newMass) && newMass > 0)) &&
    (newScale === undefined || (Number.isFinite(newScale) && newScale > 0 && newScale <= 1)) &&
    (!supportsG || newG === undefined || (Number.isFinite(newG) && newG >= 0))
  );

  $effect(() => {
    if (appliedChangeSeq !== null) {
      newMass = undefined;
      newScale = undefined;
      newMaterial = "";
      newBridge = null;
    }
  });

  function toggleBridge(checked: boolean) {
    const current = run.parameters?.bridge;
    newBridge = checked && current ? structuredClone($state.snapshot(current)) : null;
  }

  function apply(event: SubmitEvent) {
    event.preventDefault();
    if (!editable || busy || !validChange) return;
    const change: ChangePayload = {};
    if (newMass !== undefined) change.mass_kg = newMass;
    if (newScale !== undefined) change.scale = newScale;
    if (supportsG && newG !== undefined) change.g_w_per_k = newG;
    if (newMaterial) change.material = newMaterial;
    if (newBridge) change.bridge = newBridge;
    onapply(change);
  }

  const reasons = $derived([
    latest?.power_limited ? "по мощности" : null,
    latest?.current_limited ? "по току" : null,
    latest?.slew_limited ? "по скорости изменения" : null,
  ].filter((item) => item !== null));
  const fault = $derived(run.fault ? eventLabels[run.fault] ?? run.fault : null);
  const offReason = $derived(run.off_reason ? resultReasons[run.off_reason] ?? eventLabels[run.off_reason] ?? run.off_reason : null);

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
    ...(hasBody ? [line("Виртуальное тело", 3, run.samples.map((row) => row.body_k))] : []),
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
    ...(hasBody ? [line("Знаковый поток моста", 0, run.samples.map((row) => row.bridge_flow_w))] : []),
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
    {#if run.mode}<p>{modeLabels[mode]}</p>{/if}
    <svg data-testid="live-svg" role="img" aria-label={hasBody ? "Виртуальное тело, мост, головка и нагреватели" : "Головка и нагреватели"}
      viewBox={hasBody ? coordinates.viewBox : narrow ? "0 152 350 267" : "275 0 505 248"} class="thermal-scheme">
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
      {#if hasBody}
        <polyline class="bridge-link" points={bridgePoints} fill="none"
          stroke-width={bridgeStroke} stroke-dasharray={flow === null ? "5 5" : undefined}
          marker-end={flow !== null && flow !== 0 ? "url(#live-flow-arrow)" : undefined} />
        <circle cx={coordinates.body[0]} cy={coordinates.body[1]} r={radius}
          fill={bodyShade} class="body-node" />
        <text x={coordinates.body[0]} y={coordinates.body[1] - 5} class="node-label">Тело</text>
        <text x={coordinates.body[0]} y={coordinates.body[1] + 17} class="node-value">
          {format(latest?.body_k, "К")}
        </text>
        <text x={coordinates.body[0]} y={coordinates.body[1] + 35} class="detail-label">
          {format(run.parameters?.mass_kg, "кг")}
        </text>
      {/if}

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
    {#if hasBody}
      <p class="scheme-caption">
        <strong>{flowDirection}</strong>
        {#if flow !== null}<span>{format(flow, "Вт")}</span>{/if}
        {#if run.mode?.kind === "virtual_body"}
          <span class="muted">Порт {port}{latest?.mode_v1?.held_age_ns == null ? "" : ` · возраст ${format(latest.mode_v1.held_age_ns / 1e6, "мс")}`}</span>
        {/if}
      </p>
    {/if}
  </figure>

  <section class="panel power-panel">
    <h2>Мощность</h2>
    <dl class="power-summary" class:with-flow={hasBody && !!run.mode}>
      {#if hasBody && run.mode}<div><dt>Поток моста q<sub>v</sub></dt><dd>{format(flow, "Вт")}</dd></div>{/if}
      <div><dt>Запрос P<sub>req</sub></dt><dd>{format(latest?.requested_w, "Вт")}</dd></div>
      <div><dt>После ограничений P<sub>limited</sub></dt><dd>{format(latest?.limited_w, "Вт")}</dd></div>
      <div><dt>Измерено P<sub>actual</sub></dt><dd>{format(measuredTotal, "Вт")}</dd></div>
    </dl>
    {#if run.mode?.kind === "temperature_hold"}<p>Уставка stage2 {format(run.mode.setpoint_k, "К")}</p>{/if}
    {#if compensation?.status === "ready"}
      <p class="muted">dTf/dt {format(compensation.d_tf_k_per_s, "К/с")} · Cf {format(compensation.fixture_capacity_j_per_k, "Дж/К")} · Q<sub>par,f</sub> {format(compensation.fixture_parasitic_w, "Вт")}</p>
    {:else if run.compensation_ready !== undefined && !run.compensation_ready}
      <p class="muted">{compensation?.status === "unavailable" ? resultReasons[`compensation_${compensation.reason}`] : "Ожидается пара свежих измерений tf"}</p>
    {/if}
    <p class="limit-reason">
      {#if fault}<strong class="fault" role="status">△ {fault}</strong>
      {:else if reasons.length}<strong>Ограничение {reasons.join(", ")}</strong>
      {:else if latest?.equivalence === "limited"}<strong>Эквивалентность ограничена</strong>
      {:else if latest?.requested_w != null}<span class="muted">Ограничений нет</span>
      {:else}<span class="muted">Запрос мощности отсутствует</span>{/if}
    </p>
    {#if offReason && run.output_state !== "on_verified"}
      <p data-testid="off-reason">Причина остановки <strong>{offReason}</strong></p>
    {/if}
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

  {#if run.parameters}
    <section class="panel parameter-panel" aria-label="Параметры тела и моста">
      <h2>Параметры тела и моста</h2>
      <dl class="parameter-values">
        {#if run.parameters.material}
          <div><dt>Материал</dt><dd>{bodyMaterials.find(([id]) => id === run.parameters?.material)?.[1] ?? run.parameters.material}</dd></div>
        {/if}
        <div><dt>Масса</dt><dd>{format(run.parameters.mass_kg, "кг")}</dd></div>
        <div><dt>scale</dt><dd>{format(run.parameters.scale)}</dd></div>
        {#if run.parameters.bridge}
          <div><dt>Мост</dt><dd>{bridgeLabels[run.parameters.bridge.kind]}</dd></div>
        {/if}
        {#if run.parameters.g_w_per_k != null}
          <div><dt>G</dt><dd>{format(run.parameters.g_w_per_k, "Вт/К")}</dd></div>
        {:else if mode === "a1"}
          <div><dt>Мост</dt><dd>Переменная проводимость</dd></div>
        {/if}
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
          {#if mode === "a1"}
            <label class="field">
              Новый G, Вт/К
              <input type="number" min="0" step="any" bind:value={newG} disabled={!editable || busy || !supportsG} />
            </label>
          {:else}
            <label class="field">
              Новый материал
              <select bind:value={newMaterial} disabled={!editable || busy}>
                <option value="">Оставить текущий</option>
                {#each bodyMaterials as [id, name] (id)}<option value={id}>{name}</option>{/each}
              </select>
            </label>
            <label>
              <input type="checkbox" checked={newBridge !== null} disabled={!editable || busy}
                onchange={(event) => toggleBridge(event.currentTarget.checked)} /> Изменить мост
            </label>
          {/if}
        </div>
        {#if newBridge}{@render bridgeFields(newBridge, (bridge) => newBridge = bridge, !editable || busy)}{/if}
        <div class="actions">
          <button type="submit" disabled={!editable || busy || !validChange}>Применить параметры</button>
          <p data-testid={modeChange?.change.outcome.status === "rejected" ? "rejected-change" : "applied-change"}
            data-seq={modeChange?.seq ?? run.last_change?.seq} aria-live="polite">
            {#if modeChange}
              {modeChange.change.outcome.status === "applied" ? "Применено" : "Отклонено"} на такте {modeChange.tick}, запись №{modeChange.seq}
              {#if modeChange.reason}<span> · {resultReasons[modeChange.reason] ?? modeChange.reason}</span>{/if}
            {:else if run.last_change}
              Применено на такте {run.last_change.tick}, запись №{run.last_change.seq}
            {:else}
              Журнал пока не подтверждал смену параметров
            {/if}
          </p>
        </div>
      </form>
    </section>
  {/if}

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
      <Plot {time} series={bridgeAndError} units="Вт" label={hasBody ? "Поток по мосту и ошибка" : "Ошибка слежения"} {theme} />
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
              <tr><td>№{event.seq}</td><td>{clock.format(event.at_ns / 1e9)}</td><td>{eventLabels[event.code] ?? resultReasons[event.code] ?? event.code}</td></tr>
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
    flex-wrap: wrap;
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
  .power-summary.with-flow { grid-template-columns: repeat(4, minmax(0, 1fr)); }
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
    .power-summary.with-flow { grid-template-columns: repeat(2, minmax(0, 1fr)); }
    .power-summary div { padding: .45rem; }
    .power-summary dt { font-size: .72rem; }
    .parameter-values { grid-template-columns: repeat(2, minmax(0, 1fr)); }
    .branch-table th, .branch-table td { padding: .35rem .4rem; }
    .branch-table th:last-child, .branch-table td:last-child { min-width: 0; }
  }
</style>
