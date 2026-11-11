<script module lang="ts">
  import { bridgeLabels } from "./types";
  import type { BridgeSetup } from "./types";
  export { bridgeFields };
  const metals = [
    ["ofhc_rrr100", "Медь OFHC, RRR 100"],
    ["al6061_t6", "Алюминий 6061-T6"],
    ["ss304", "Сталь 304"],
  ] as const;
  export const bodyMaterials = [...metals, ["g10_cr", "Стеклотекстолит G-10 CR"]] as const;
  const conductors = [
    ...metals,
    ["al1100", "Алюминий 1100"],
    ["g10_cr_normal", "G-10 CR поперёк слоёв"],
    ["g10_cr_warp", "G-10 CR вдоль основы"],
  ] as const;
  const wireMaterials = [
    ["lakeshore_phosphor_bronze", "Фосфористая бронза, Lake Shore"],
    ["lakeshore_copper", "Медь, Lake Shore"],
    ["lakeshore_nichrome", "Нихром, Lake Shore"],
    ["lakeshore_manganin", "Манганин, Lake Shore"],
    ...conductors,
  ] as const;

  function defaultBridge(kind: BridgeSetup["kind"]): BridgeSetup {
    const link = { material: "ofhc_rrr100", area_m2: 0.00000012, length_m: 0.3 };
    switch (kind) {
      case "constant": return { kind, conductance_w_per_k: 0.02 };
      case "conductor": return { kind, ...link };
      case "table": return { kind, points: [[4, 0.02], [300, 0.02]] };
      case "series": return { kind, ...link, contact_w_per_k: 0.02 };
    }
  }
</script>

<script lang="ts">
  import Plot from "./Plot.svelte";
  import { modeChoice, modeLabels } from "./types";
  import type { Estimate, HistoricalReport, ModeChoice, PlotSeries, Setup, Theme } from "./types";

  type Node = Setup["wiring"][number]["from"];
  type Props = {
    setup: Setup;
    estimate: Estimate | null;
    busy: boolean;
    canPrepare: boolean;
    onpreview: () => void;
    onprepare: () => void;
    theme: Theme;
  };

  let {
    setup = $bindable(),
    estimate,
    busy,
    canPrepare,
    onpreview,
    onprepare,
    theme,
  }: Props = $props();

  const nodes: { id: Node; label: string }[] = [
    { id: "stage1", label: "Тёплая ступень (stage1)" },
    { id: "stage2", label: "Холодная ступень (stage2)" },
    { id: "tf", label: "Насадка (tf)" },
    { id: "jacket", label: "Азотный перехват (jacket)" },
  ];
  const format = new Intl.NumberFormat("ru-RU", { maximumSignificantDigits: 5 });
  const signed = new Intl.NumberFormat("ru-RU", {
    maximumSignificantDigits: 5,
    signDisplay: "exceptZero",
  });
  const historicalPath = "/api/historical-head";
  const originLabels = {
    historical_anchor: "Опора этапа I",
    interpolated: "Интерполяция",
    synthetic: "Синтетическое продолжение",
  } as const;
  let historicalReport = $state<HistoricalReport | null>(null);
  let historicalBusy = $state(false);
  let historicalError = $state<string | null>(null);

  async function calculateHistoricalHead() {
    historicalBusy = true;
    historicalError = null;
    historicalReport = null;
    try {
      const response = await fetch(historicalPath, {
        cache: "no-store",
        signal: AbortSignal.timeout(1500),
      });
      if (response.ok) {
        historicalReport = await response.json();
      } else {
        const failure = await response.json();
        historicalError = failure.error ?? "Расчёт исторической характеристики недоступен";
      }
    } catch {
      historicalError = "Не удалось получить историческую характеристику";
    } finally {
      historicalBusy = false;
    }
  }

  function nodeLabel(id: string): string {
    return nodes.find((node) => node.id === id)?.label ?? id;
  }

  function chooseMode(choice: ModeChoice) {
    switch (choice) {
      case "a1": setup.mode = undefined; break;
      case "observe": setup.mode = { kind: choice }; break;
      case "constant": setup.mode = { kind: choice, power_w: 0 }; break;
      case "power_profile":
        setup.mode = { kind: choice, points: [{ elapsed_ms: 0, power_w: 0 }, { elapsed_ms: 1000, power_w: 0 }] };
        break;
      case "temperature_hold": setup.mode = { kind: choice, setpoint_k: 15.5 }; break;
      case "virtual_body": setup.mode = { kind: choice, port: "tf", body_heat_w: 0 }; break;
      case "compensation":
        setup.mode = { kind: "virtual_body", port: "tf", body_heat_w: 0, compensation: {} };
        break;
    }
  }

  function addWire() {
    setup.wiring.push({
      from: "stage2",
      to: "tf",
      material: "lakeshore_phosphor_bronze",
      length_m: 0.3,
      area_mm2: 0.12,
      strands: 2,
    });
  }

  const choice = $derived(modeChoice(setup.mode));
  const hasBody = $derived(!setup.mode || setup.mode.kind === "virtual_body");
  let preview = $derived(estimate?.preview ?? []);
  let time = $derived(preview.map((point) => point.time_s));
  let colors = $derived(
    theme === "dark"
      ? ["#efb44c", "#69b5e3", "#72c8a3", "#dc97c3"]
      : ["#9f5600", "#006fa3", "#087657", "#933f7e"],
  );
  let temperatureSeries: PlotSeries[] = $derived([
    { label: "Виртуальное тело", color: colors[0], values: preview.map((p) => p.body_k) },
    { label: "stage1", color: colors[1], values: preview.map((p) => p.temperatures_k[0]), dash: [9, 4] },
    { label: "stage2", color: colors[2], values: preview.map((p) => p.temperatures_k[1]), dash: [4, 3] },
    { label: "tf", color: colors[3], values: preview.map((p) => p.temperatures_k[2]), dash: [2, 4] },
  ]);
  let powerSeries: PlotSeries[] = $derived([
    { label: "Идеальный нагрев", color: colors[1], values: preview.map((p) => p.heater_w) },
    { label: "Поток моста", color: colors[0], values: preview.map((p) => p.bridge_w), dash: [5, 3] },
  ]);
</script>

{#snippet bridgeFields(bridge: BridgeSetup, setBridge: (value: BridgeSetup) => void, disabled: boolean)}
  <fieldset {disabled}>
    <legend>Тепловой мост</legend>
    <div class="form-grid">
      <label class="field">
        Тип теплового моста
        <select value={bridge.kind} onchange={(event) => setBridge(defaultBridge(event.currentTarget.value as BridgeSetup["kind"]))}>
          {#each Object.entries(bridgeLabels) as [kind, label] (kind)}
            <option value={kind}>{label}</option>
          {/each}
        </select>
      </label>
      {#if bridge.kind === "constant"}
        <label class="field">
          Проводимость моста G, Вт/К
          <input type="number" bind:value={bridge.conductance_w_per_k} min="0" step="any" required />
        </label>
      {:else if bridge.kind === "conductor" || bridge.kind === "series"}
        <label class="field">
          Материал проводника моста
          <select bind:value={bridge.material}>
            {#each conductors as [id, name] (id)}
              <option value={id}>{name}</option>
            {/each}
          </select>
        </label>
        <label class="field">
          Сечение проводника, м²
          <input type="number" bind:value={bridge.area_m2} min="0" step="any" required />
        </label>
        <label class="field">
          Длина проводника, м
          <input type="number" bind:value={bridge.length_m} min="0" step="any" required />
        </label>
        {#if bridge.kind === "series"}
          <label class="field">
            Проводимость контакта, Вт/К
            <input type="number" bind:value={bridge.contact_w_per_k} min="0" step="any" required />
          </label>
        {/if}
      {/if}
    </div>
    {#if bridge.kind === "table"}
      <p class="muted">Температуры точек должны возрастать.</p>
      {#each bridge.points as point, index (point)}
        <fieldset class="row">
          <legend>Точка {index + 1}</legend>
          <div class="form-grid">
            <label class="field">
              Температура точки, К
              <input type="number" bind:value={point[0]} min="0" step="any" required />
            </label>
            <label class="field">
              Проводимость точки, Вт/К
              <input type="number" bind:value={point[1]} min="0" step="any" required />
            </label>
          </div>
          <button class="secondary" type="button" aria-label={"Удалить точку " + (index + 1)}
            disabled={bridge.points.length <= 2}
            onclick={() => { if (bridge.kind === "table") bridge.points.splice(index, 1); }}>
            Удалить точку
          </button>
        </fieldset>
      {/each}
      <button class="secondary" type="button" onclick={() => {
        if (bridge.kind === "table") {
          const [temperature, conductance] = bridge.points.at(-1) ?? [0, 0];
          bridge.points.push([(temperature ?? 0) + 1, conductance ?? 0]);
        }
      }}>
        Добавить точку
      </button>
    {/if}
  </fieldset>
{/snippet}

<h2>Сценарий</h2>
<div class="scenario-layout">
  <form class="panel" onsubmit={(event) => { event.preventDefault(); onpreview(); }}>
    <fieldset>
      <legend>Режим</legend>
      <label class="field">
        Режим прогона
        <select aria-label="Режим прогона" value={choice} disabled={busy}
          onchange={(event) => chooseMode(event.currentTarget.value as ModeChoice)}>
          {#each Object.entries(modeLabels) as [id, label] (id)}
            <option value={id}>{label}</option>
          {/each}
        </select>
      </label>
      {#if setup.mode?.kind === "observe"}
        <p class="muted">Измерения и запись выполняются при выключенном нагреве.</p>
      {:else if setup.mode?.kind === "constant"}
        <label class="field">
          Задание мощности, Вт
          <input type="number" min="0" step="any" required bind:value={setup.mode.power_w} />
        </label>
      {:else if setup.mode?.kind === "temperature_hold"}
        <label class="field">
          Уставка stage2, К
          <input type="number" min="0" step="any" required bind:value={setup.mode.setpoint_k} />
        </label>
      {:else if setup.mode?.kind === "power_profile"}
        <p class="muted">Время отсчитывается от подтверждённого пуска.</p>
        {#each setup.mode.points as point, index (point)}
          <fieldset class="row">
            <legend>Точка {index + 1}</legend>
            <div class="form-grid">
              <label class="field">
                Время от пуска, мс
                <input type="number" step="1" required readonly={index === 0}
                  min={index === 0 ? 0 : (setup.mode.points[index - 1].elapsed_ms ?? 0) + 1}
                  bind:value={point.elapsed_ms} />
              </label>
              <label class="field">
                Мощность точки, Вт
                <input type="number" min="0" step="any" required bind:value={point.power_w} />
              </label>
            </div>
            {#if index > 0}
              <button class="secondary" type="button" aria-label={"Удалить точку мощности " + (index + 1)}
                onclick={() => { if (setup.mode?.kind === "power_profile") setup.mode.points.splice(index, 1); }}>
                Удалить точку
              </button>
            {/if}
          </fieldset>
        {/each}
        <button class="secondary" type="button" onclick={() => {
          if (setup.mode?.kind === "power_profile") {
            const last = setup.mode.points.at(-1);
            setup.mode.points.push({ elapsed_ms: (last?.elapsed_ms ?? 0) + 1000, power_w: last?.power_w ?? 0 });
          }
        }}>Добавить точку мощности</button>
      {:else if setup.mode?.kind === "virtual_body"}
        <div class="form-grid">
          <label class="field">
            Порт теплового моста
            <select bind:value={setup.mode.port}>
              <option value="tf">Насадка (tf)</option>
              <option value="stage2">Холодная ступень (stage2)</option>
            </select>
          </label>
          <label class="field">
            Паразитный приток к телу, Вт
            <input type="number" min="0" step="any" required bind:value={setup.mode.body_heat_w} />
          </label>
          {#if setup.mode.compensation}
            <label class="field">
              Паразитный приток насадки, Вт
              <input type="number" min="0" step="any" required bind:value={setup.mode.compensation.fixture_parasitic_w} />
            </label>
          {/if}
        </div>
      {/if}
    </fieldset>

    {#if hasBody}
      <fieldset>
        <legend>Виртуальное тело</legend>
        <div class="form-grid">
          <label class="field">
            Материал тела
            <select bind:value={setup.body.material}>
              {#each bodyMaterials as [id, name] (id)}
                <option value={id}>{name}</option>
              {/each}
            </select>
          </label>
          <label class="field">
            Масса тела, кг
            <input type="number" bind:value={setup.body.mass_kg} min="0" step="any" required />
          </label>
          <label class="field">
            Начальная температура тела, К
            <input type="number" bind:value={setup.body.temperature_k} min="0" step="any" required />
          </label>
          <label class="field">
            Масштаб теплоёмкости (scale)
            <input type="number" bind:value={setup.body.scale} min="0" max="1" step="any" required />
          </label>
          <label class="field full-width">
            Ползунок масштаба теплоёмкости
            <input type="range" bind:value={setup.body.scale} min="0.01" max="1" step="0.01" />
          </label>
        </div>
      </fieldset>

      {@render bridgeFields(setup.body.bridge, (bridge) => setup.body.bridge = bridge, busy)}

      <details open>
        <summary>Проводка</summary>
        {#each setup.wiring as wire, index (wire)}
          <fieldset class="row">
            <legend>Участок {index + 1}</legend>
            <div class="form-grid">
              <label class="field">
                Начальный узел
                <select bind:value={wire.from}>
                  {#each nodes as node (node.id)}
                    <option value={node.id}>{node.label}</option>
                  {/each}
                </select>
              </label>
              <label class="field">
                Конечный узел
                <select bind:value={wire.to}>
                  {#each nodes as node (node.id)}
                    <option value={node.id}>{node.label}</option>
                  {/each}
                </select>
              </label>
              <label class="field">
                Материал жил
                <select bind:value={wire.material}>
                  {#each wireMaterials as [id, name] (id)}
                    <option value={id}>{name}</option>
                  {/each}
                </select>
              </label>
              <label class="field">
                Число жил
                <input type="number" bind:value={wire.strands} min="1" max="4294967295" step="1" required />
              </label>
              <label class="field">
                Длина жилы, м
                <input type="number" bind:value={wire.length_m} min="0" step="any" required />
              </label>
              <label class="field">
                Сечение жилы, мм²
                <input type="number" bind:value={wire.area_mm2} min="0" step="any" required />
              </label>
            </div>
            <button class="secondary" type="button" aria-label={"Удалить участок " + (index + 1)}
              onclick={() => setup.wiring.splice(index, 1)}>
              Удалить участок
            </button>
          </fieldset>
        {/each}
        <button class="secondary" type="button" onclick={addWire}>Добавить участок</button>
      </details>

      <details open>
        <summary>Состав насадки</summary>
        {#each setup.fixture as part, index (part)}
          <fieldset class="row">
            <legend>Деталь {index + 1}</legend>
            <div class="form-grid">
              <label class="field">
                Материал детали
                <select bind:value={part.material}>
                  {#each bodyMaterials as [id, name] (id)}
                    <option value={id}>{name}</option>
                  {/each}
                </select>
              </label>
              <label class="field">
                Масса детали, кг
                <input type="number" bind:value={part.mass_kg} min="0" step="any" required />
              </label>
            </div>
            <button class="secondary" type="button" aria-label={"Удалить деталь " + (index + 1)}
              onclick={() => setup.fixture.splice(index, 1)}>
              Удалить деталь
            </button>
          </fieldset>
        {/each}
        <button class="secondary" type="button" onclick={() => setup.fixture.push({ material: "ofhc_rrr100", mass_kg: 0.2 })}>
          Добавить деталь
        </button>
        {#if choice === "compensation" && setup.fixture.length === 0}
          <p class="muted">Добавьте хотя бы одну деталь насадки.</p>
        {/if}
      </details>

    {/if}
    <div class="actions">
      <button type="submit" disabled={busy || (choice === "compensation" && setup.fixture.length === 0)}>Рассчитать прогноз</button>
      <button type="button" class="secondary" disabled={busy || !canPrepare} onclick={onprepare}>
        Подготовить прогон
      </button>
    </div>
  </form>

  <section class="panel" aria-label={hasBody ? "Синтетический прогноз" : "Расчётная область"} aria-busy={busy}>
    <h3>{choice === "compensation" ? "Условия компенсации" : hasBody ? "Синтетический прогноз" : "Расчётная область"}</h3>
    {#if estimate}
      {#if estimate.compensation}
        <p class="muted">Для допуска нужны два свежих измерения tf. Производная определяется во время прогона.</p>
        <dl class="diagnostics">
          <dt>Паразитный приток насадки</dt><dd>{format.format(estimate.compensation.fixture_parasitic_w)} Вт</dd>
          <dt>Теплоёмкость при начальном tf</dt><dd>{format.format(estimate.compensation.fixture_capacity_j_per_k)} Дж/К</dd>
        </dl>
      {:else if preview.length}
        <p class="muted">Горизонт 3 с, шаг 0,1 с. Нагрев идеальный в расчётной области.</p>
        <Plot {time} series={temperatureSeries} units="К" label="Прогноз температуры" {theme} height={240} />
        <Plot {time} series={powerSeries} units="Вт" label="Прогноз теплового потока и нагрева" {theme} height={190} />
      {:else}
        <p>Суммарный расчётный предел {format.format(estimate.limits.envelope_total_w)} Вт.</p>
      {/if}
      {#if hasBody}
        <h3>Проводка и насадка</h3>
        <p class="muted">Притоки проводки рассчитаны при нулевом токе.</p>
        <dl class="diagnostics">
          {#each nodes as node, index (node.id)}
            <dt>{node.label}</dt>
            <dd>{signed.format(estimate.wire_heat_by_node_w[index])} Вт</dd>
          {/each}
          {#if !estimate.compensation}
            <dt>Теплоёмкость насадки при начальной tf</dt>
            <dd>{format.format(estimate.fixture_capacity_j_per_k)} Дж/К</dd>
          {/if}
        </dl>
        {#if estimate.wiring.length > 0}
          <ul class="wire-results" aria-label="Потоки по участкам проводки">
            {#each estimate.wiring as wire}
              <li>
                <span>{nodeLabel(wire.from)} {signed.format(wire.heat_from_w)} Вт</span>
                <span>{nodeLabel(wire.to)} {signed.format(wire.heat_to_w)} Вт</span>
                <small class="muted">{wire.source}</small>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}
    {:else if busy}
      <p role="status">Выполняется расчёт</p>
    {/if}
  </section>
</div>

<section class="panel historical-head" aria-labelledby="historical-head-title">
  <h3 id="historical-head-title">Историческая характеристика головки</h3>
  <div class="actions">
    <button type="button" class="secondary" disabled={historicalBusy} onclick={calculateHistoricalHead}>
      Рассчитать характеристику
    </button>
    {#if historicalReport}
      <a href={historicalPath} download="cvtl-historical-head.json">Скачать JSON</a>
    {/if}
  </div>
  {#if historicalBusy}
    <p role="status">Выполняется расчёт характеристики</p>
  {:else if historicalError}
    <p class="notice error" role="alert">{historicalError}</p>
  {:else if historicalReport}
    <p class="muted">Опыт этапа I проходил без жидкого азота и экрана. P_net — подведённая мощность с невычтенными паразитными притоками.</p>
    <p class="muted">Источник <code>{historicalReport.source_path}</code></p>
    <p class="muted">
      Выше {format.format(historicalReport.continuation_from_k)} К характеристика синтетически продолжена до {format.format(historicalReport.continuation_to_k)} К.
    </p>
    <div class="table-wrap">
      <table aria-label="Точки исторической характеристики головки">
        <thead>
          <tr><th scope="col">stage2, К</th><th scope="col">stage1, К</th><th scope="col">P_net, Вт</th><th scope="col">Происхождение</th></tr>
        </thead>
        <tbody>
          {#each historicalReport.points as point (point.stage2_k)}
            <tr>
              <td>{format.format(point.stage2_k)}</td>
              <td>{format.format(point.stage1_k)}</td>
              <td>{format.format(point.p_net_w)}</td>
              <td>{originLabels[point.origin]}{point.source_row === null ? "" : ` · строка ${point.source_row}`}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</section>

<style>
  .scenario-layout {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 1rem;
    align-items: start;
  }
  .scenario-layout > * { min-width: 0; }
  fieldset {
    min-width: 0;
    margin: 0 0 1rem;
    padding: 0.75rem;
    border: 1px solid var(--line);
    border-radius: 0.5rem;
  }
  legend, summary { font-weight: 600; }
  details { margin-bottom: 1rem; }
  summary { margin-bottom: 0.65rem; cursor: pointer; }
  .full-width { grid-column: 1 / -1; }
  .row > button { margin-top: 0.65rem; }
  .diagnostics {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 0.35rem 0.75rem;
  }
  .diagnostics dd { margin: 0; text-align: right; }
  .wire-results {
    padding: 0;
    margin: 0;
    list-style: none;
  }
  .wire-results li {
    display: grid;
    gap: 0.3rem;
    padding: 0.6rem 0;
    border-top: 1px solid var(--line);
  }
  .wire-results small { overflow-wrap: anywhere; }
  .historical-head { margin-top: 1rem; }
  .historical-head a { color: var(--accent); }
  .historical-head code { overflow-wrap: anywhere; }
  .historical-head .notice { margin-top: 0.85rem; }
  @media (max-width: 860px) {
    .scenario-layout { grid-template-columns: minmax(0, 1fr); }
  }
  @media (max-width: 480px) {
    .historical-head th, .historical-head td { padding-inline: 0.32rem; }
    .historical-head td:nth-child(-n + 3) { white-space: nowrap; }
    .historical-head th:last-child, .historical-head td:last-child { overflow-wrap: anywhere; }
  }
</style>
