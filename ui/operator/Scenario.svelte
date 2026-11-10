<script lang="ts">
  import Plot from "./Plot.svelte";
  import type { Estimate, PlotSeries, Setup, Theme } from "./types";

  type Bridge = Setup["body"]["bridge"];
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

  const metals = [
    ["ofhc_rrr100", "Медь OFHC, RRR 100"],
    ["al6061_t6", "Алюминий 6061-T6"],
    ["ss304", "Сталь 304"],
  ] as const;
  const bodyMaterials = [...metals, ["g10_cr", "Стеклотекстолит G-10 CR"]] as const;
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

  function nodeLabel(id: string): string {
    return nodes.find((node) => node.id === id)?.label ?? id;
  }

  function changeBridge(kind: Bridge["kind"]) {
    const link = { material: "ofhc_rrr100", area_m2: 0.00000012, length_m: 0.3 };
    switch (kind) {
      case "constant":
        setup.body.bridge = { kind, conductance_w_per_k: 0.02 };
        break;
      case "conductor":
        setup.body.bridge = { kind, ...link };
        break;
      case "table":
        setup.body.bridge = { kind, points: [[4, 0.02], [300, 0.02]] };
        break;
      case "series":
        setup.body.bridge = { kind, ...link, contact_w_per_k: 0.02 };
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

  function addBridgePoint() {
    const bridge = setup.body.bridge;
    if (bridge.kind !== "table") return;
    const [temperature, conductance] = bridge.points[bridge.points.length - 1];
    bridge.points.push([temperature + 1, conductance]);
  }

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

<h2>Сценарий</h2>
<div class="scenario-layout">
  <form class="panel" onsubmit={(event) => { event.preventDefault(); onpreview(); }}>
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

    <fieldset>
      <legend>Тепловой мост</legend>
      <div class="form-grid">
        <label class="field">
          Тип теплового моста
          <select value={setup.body.bridge.kind} onchange={(event) => changeBridge(event.currentTarget.value as Bridge["kind"])}>
            <option value="constant">Постоянный G</option>
            <option value="conductor">Проводник</option>
            <option value="table">Таблица G(T)</option>
            <option value="series">Контакт и проводник</option>
          </select>
        </label>
        {#if setup.body.bridge.kind === "constant"}
          <label class="field">
            Проводимость моста G, Вт/К
            <input type="number" bind:value={setup.body.bridge.conductance_w_per_k} min="0" step="any" required />
          </label>
        {:else if setup.body.bridge.kind === "conductor" || setup.body.bridge.kind === "series"}
          <label class="field">
            Материал проводника моста
            <select bind:value={setup.body.bridge.material}>
              {#each conductors as [id, name] (id)}
                <option value={id}>{name}</option>
              {/each}
            </select>
          </label>
          <label class="field">
            Сечение проводника, м²
            <input type="number" bind:value={setup.body.bridge.area_m2} min="0" step="any" required />
          </label>
          <label class="field">
            Длина проводника, м
            <input type="number" bind:value={setup.body.bridge.length_m} min="0" step="any" required />
          </label>
          {#if setup.body.bridge.kind === "series"}
            <label class="field">
              Проводимость контакта, Вт/К
              <input type="number" bind:value={setup.body.bridge.contact_w_per_k} min="0" step="any" required />
            </label>
          {/if}
        {/if}
      </div>
      {#if setup.body.bridge.kind === "table"}
        <p class="muted">Температуры точек должны возрастать.</p>
        {#each setup.body.bridge.points as point, index (point)}
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
              disabled={setup.body.bridge.points.length <= 2}
              onclick={() => { if (setup.body.bridge.kind === "table") setup.body.bridge.points.splice(index, 1); }}>
              Удалить точку
            </button>
          </fieldset>
        {/each}
        <button class="secondary" type="button" onclick={addBridgePoint}>
          Добавить точку
        </button>
      {/if}
    </fieldset>

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
    </details>

    <div class="actions">
      <button type="submit" disabled={busy}>Рассчитать прогноз</button>
      <button type="button" class="secondary" disabled={busy || !canPrepare} onclick={onprepare}>
        Подготовить прогон
      </button>
    </div>
  </form>

  <section class="panel" aria-label="Синтетический прогноз" aria-busy={busy}>
    <h3>Синтетический прогноз</h3>
    <p class="muted">Горизонт 3 с, шаг 0,1 с. Нагрев идеальный в расчётной области.</p>
    {#if estimate}
      <Plot {time} series={temperatureSeries} units="К" label="Прогноз температуры" {theme} height={240} />
      <Plot {time} series={powerSeries} units="Вт" label="Прогноз теплового потока и нагрева" {theme} height={190} />
      <h3>Проводка и насадка</h3>
      <p class="muted">В режиме AdditionalLoad мощность задаёт поток теплового моста. Притоки проводки рассчитаны при нулевом токе.</p>
      <dl class="diagnostics">
        {#each nodes as node, index (node.id)}
          <dt>{node.label}</dt>
          <dd>{signed.format(estimate.wire_heat_by_node_w[index])} Вт</dd>
        {/each}
        <dt>Теплоёмкость насадки при начальной tf</dt>
        <dd>{format.format(estimate.fixture_capacity_j_per_k)} Дж/К</dd>
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
    {:else if busy}
      <p role="status">Выполняется расчёт</p>
    {/if}
  </section>
</div>

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
  @media (max-width: 860px) {
    .scenario-layout { grid-template-columns: minmax(0, 1fr); }
  }
</style>
