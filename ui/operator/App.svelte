<script lang="ts">
  import { onMount } from "svelte";
  import Live from "./Live.svelte";
  import Review from "./Review.svelte";
  import Scenario from "./Scenario.svelte";
  import { modeChoice, modeLabels, resultReasons } from "./types";
  import type { ArchiveReceipt, ArchiveReview, ChangePayload, DaemonState, Estimate, OutputState, Setup, Theme } from "./types";

  const pages = [
    { id: "scenario", label: "Сценарий" },
    { id: "preflight", label: "Проверка перед запуском" },
    { id: "live", label: "Ход опыта" },
    { id: "review", label: "Разбор" },
    { id: "settings", label: "Оборудование и настройки" },
  ] as const;
  type Screen = (typeof pages)[number]["id"];
  type Command = "create" | "arm" | "start" | "stop" | "recover" | "reset" | "change" | "fault";
  type JobKind = "preview" | "list" | "primary" | "comparison";
  type Job = { id: number; kind: JobKind; fingerprint?: string };
  type JobResult = { id: number; value: unknown; error: string | null };

  const outputLabel: Record<OutputState, string> = {
    off_verified: "OFF подтверждён",
    on_verified: "Нагрев включён",
    unknown: "Состояние выхода неизвестно",
  };
  const journalLabel = {
    opening: "Подготовка журнала",
    recording: "Запись открыта",
    closing: "Закрытие журнала",
    completed: "Запись завершена",
    failed: "Ошибка записи",
    timed_out: "Истёк срок закрытия",
  };
  const format = new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 4 });

  let screen = $state<Screen>("scenario");
  let theme = $state<Theme>(localStorage.getItem("cvtl-theme") === "dark" ? "dark" : "light");
  let setup = $state<Setup | null>(null);
  let daemon = $state<DaemonState>({ run: null, last_action: null, test_faults: false });
  let connection = $state(false);
  let forecast = $state<{ id: number; fingerprint: string; estimate: Estimate } | null>(null);
  let job = $state<Job | null>(null);
  let sendingJob = $state(false);
  let action = $state<{ id: number; kind: Command } | null>(null);
  let sending = $state(false);
  let waitingChangeSeq = $state<number | null>(null);
  let notice = $state<string | null>(null);
  let error = $state<string | null>(null);
  let archives = $state<ArchiveReceipt[]>([]);
  let primary = $state<ArchiveReview | null>(null);
  let comparison = $state<ArchiveReview | null>(null);
  let confirmKind = $state<"start" | "reset" | null>(null);
  let confirmation: HTMLDialogElement;
  let commandRevision = 0;

  const run = $derived(daemon.run);
  const currentForecast = $derived(
    setup && forecast?.fingerprint === JSON.stringify(setup) ? forecast : null,
  );
  const jobBusy = $derived(sendingJob || job !== null);
  const actionBusy = $derived(sending || action !== null);
  const active = $derived(run?.journal === "opening" || run?.journal === "recording");
  const observing = $derived(run?.mode?.kind === "observe");
  const canPrepare = $derived(!!currentForecast && connection && !active && run?.journal !== "closing" && !actionBusy);

  $effect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("cvtl-theme", theme);
  });

  async function request<T>(path: string, body?: unknown): Promise<T> {
    const response = await fetch(path, {
      method: body === undefined ? "GET" : "POST",
      ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
      cache: "no-store",
      signal: AbortSignal.timeout(1500),
    });
    const value = await response.json();
    if (!response.ok) throw new Error(value.error ?? "Запрос отклонён");
    return value as T;
  }

  function message(reason: unknown): string {
    return reason instanceof Error ? reason.message : "Действие завершилось ошибкой";
  }

  function update(value: DaemonState) {
    daemon = value;
    const received = value.last_action;
    if (action && received?.id === action.id) {
      const kind = action.kind;
      action = null;
      if (received.ok) {
        if (kind === "create") screen = value.run?.mode?.kind === "observe" ? "live" : "preflight";
        if (kind === "start") screen = "live";
        notice = kind === "change" ? "Заявка принята. Ожидается запись такта" : null;
      } else {
        error = received.message ?? "Действие отклонено владельцем контура";
        if (kind === "change") waitingChangeSeq = null;
      }
    } else if (action && received && received.id > action.id) {
      action = null;
      waitingChangeSeq = null;
      error = "Результат заявки уже сменился. Проверьте текущее состояние прогона";
    }
    if (waitingChangeSeq !== null) {
      const modeChange = value.run?.last_mode_change;
      if ((modeChange?.seq ?? value.run?.last_change?.seq ?? 0) > waitingChangeSeq) {
        waitingChangeSeq = null;
        if (modeChange?.change.outcome.status === "rejected") {
          notice = null;
          error = resultReasons[modeChange.reason ?? "body_edit_invalid_at_tick"] ?? "Параметры отклонены на такте";
        } else {
          notice = "Параметры применены и записаны";
        }
      } else if (!value.run || value.run.fault || value.run.journal !== "recording") {
        waitingChangeSeq = null;
      }
    }
  }

  function updateJob(result: JobResult | null) {
    if (job && result && result.id > job.id) {
      job = null;
      error = "Результат запроса уже сменился. Повторите расчёт или чтение";
      return;
    }
    if (!job || result?.id !== job.id) return;
    const requested = job;
    job = null;
    if (result.error) {
      error = result.error;
      return;
    }
    if (requested.kind === "preview" && requested.fingerprint !== undefined) {
      forecast = { id: requested.id, fingerprint: requested.fingerprint, estimate: result.value as Estimate };
    } else if (requested.kind === "list") {
      archives = result.value as ArchiveReceipt[];
    } else if (requested.kind === "primary") {
      primary = result.value as ArchiveReview;
      comparison = null;
    } else if (requested.kind === "comparison") {
      comparison = result.value as ArchiveReview;
    }
  }

  onMount(() => {
    let mounted = true;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        if (!setup) setup = await request<Setup>("/api/defaults");
        const state = await request<DaemonState>("/api/state");
        if (!mounted) return;
        connection = true;
        update(state);
        if (job) updateJob(await request<JobResult | null>("/api/job"));
      } catch {
        connection = false;
      } finally {
        if (mounted) timer = setTimeout(poll, 100);
      }
    }
    void poll();
    return () => {
      mounted = false;
      clearTimeout(timer);
    };
  });

  async function readJob(path: string, body: unknown, kind: JobKind, fingerprint?: string) {
    if (jobBusy || !connection) return;
    error = null;
    sendingJob = true;
    try {
      const { id } = await request<{ id: number }>(path, body);
      job = { id, kind, fingerprint };
    } catch (reason) {
      error = message(reason);
    } finally {
      sendingJob = false;
    }
  }

  function preview() {
    if (setup) void readJob("/api/preview", setup, "preview", JSON.stringify(setup));
  }

  async function send(path: string, body: unknown, kind: Command) {
    if ((actionBusy || !connection) && kind !== "stop") return;
    error = null;
    notice = null;
    const revision = ++commandRevision;
    const beforeChange = run?.last_mode_change?.seq ?? run?.last_change?.seq ?? 0;
    sending = true;
    try {
      const { id } = await request<{ id: number }>(path, body);
      if (revision !== commandRevision) return;
      action = { id, kind };
      if (kind === "change") waitingChangeSeq = beforeChange;
    } catch (reason) {
      if (revision === commandRevision) error = message(reason);
    } finally {
      if (revision === commandRevision) sending = false;
    }
  }

  function prepare() {
    if (canPrepare && currentForecast) {
      void send("/api/run", { preview_id: currentForecast.id }, "create");
    }
  }

  function command(kind: Exclude<Command, "create">, fields: ChangePayload | { confirmed: true } = {}) {
    if (run) void send("/api/action", { run: run.id, kind, ...fields }, kind);
  }

  function ask(kind: "start" | "reset") {
    confirmKind = kind;
    confirmation.showModal();
  }

  function confirmAction() {
    const kind = confirmKind;
    confirmation.close();
    if (kind) command(kind, { confirmed: true });
  }

  function display(value: number | null | undefined, units = "") {
    return value == null ? "Нет данных" : format.format(value) + (units ? " " + units : "");
  }

  function choose(next: Screen) {
    screen = next;
    if (next === "review" && archives.length === 0 && connection) {
      void readJob("/api/archive", { kind: "list" }, "list");
    }
  }
</script>

<header class="topbar">
  <div class="topbar-inner">
    <div class="brand">
      <span class="wordmark">CVTL</span>
      <span class="mode">Симуляция</span>
    </div>
    <div class="run-status" aria-live="polite">
      {#if !connection}<span class="status warning">⚠ {run ? "Связь потеряна. Последние данные устарели" : "Связь с демоном недоступна"}</span>{/if}
      {#if run}
        <span class:positive={run.output_state === "off_verified"} class:warning={run.output_state === "unknown"} class="status"
          data-testid="output-state" data-state={run.output_state} data-stale={!connection}>
          <span aria-hidden="true">{run.output_state === "off_verified" ? "✓" : run.output_state === "unknown" ? "⚠" : "●"}</span>
          {outputLabel[run.output_state]}
        </span>
        <span class="muted" data-testid="journal-state" data-state={run.journal}>{journalLabel[run.journal]}</span>
      {/if}
      {#if active}
        <button class="stop" onclick={() => command("stop")} disabled={action?.kind === "stop"}>Стоп</button>
      {/if}
    </div>
  </div>
</header>

<nav class="navigation" aria-label="Экраны оператора">
  <div class="navigation-inner">
    {#each pages as page}
      <button class="tab" aria-current={screen === page.id ? "page" : undefined} onclick={() => choose(page.id)}>{page.label}</button>
    {/each}
  </div>
</nav>

<main class="workspace">
  {#if run?.archive_error}
    <p class="notice error" role="status" data-testid="archive-error">⚠ {run.archive_error}</p>
  {/if}
  {#if error}
    <div class="notice error" role="alert">
      <span aria-hidden="true">⚠</span> {error}
      <button type="button" class="secondary" aria-label="Закрыть сообщение" onclick={() => error = null}>×</button>
    </div>
  {:else if notice}
    <p class="notice" role="status">{notice}</p>
  {/if}

  {#if screen === "scenario"}
    {#if setup}
      <Scenario bind:setup estimate={currentForecast?.estimate ?? null} busy={jobBusy || actionBusy}
        {canPrepare} onpreview={preview} onprepare={prepare} {theme} />
    {:else}
      <h1>Сценарий</h1>
      <p class="panel">Ожидается конфигурация от демона.</p>
    {/if}
  {:else if screen === "preflight"}
    <h1>Проверка перед запуском</h1>
    {#if run}
      <div class="grid-two">
        <section class="panel">
          <h2>Готовность контура</h2>
          {#if run.mode}<p>{modeLabels[modeChoice(run.mode)]}</p>{/if}
          <ul class="checks">
            <li><span class:positive={run.source_ready}>{run.source_ready ? "✓ Готов" : "○ Ожидание"}</span> Keithley 2604B, эмулятор</li>
            <li><span class:positive={run.thermometer_ready}>{run.thermometer_ready ? "✓ Готов" : "○ Ожидание"}</span> Lake Shore 218, эмулятор</li>
            <li><span class:positive={run.writer_ready}>{run.writer_ready ? "✓ Готов" : "○ Ожидание"}</span> Первичный журнал</li>
            <li><span class:positive={run.observation_clear}>{run.observation_clear ? "✓ Штатно" : "⚠ Проверить"}</span> Наблюдение M5</li>
            {#if run.compensation_ready !== undefined}
              <li data-testid="compensation-ready" data-ready={run.compensation_ready}>
                <span class:positive={run.compensation_ready}>{run.compensation_ready ? "✓ Готова" : "○ Ожидание"}</span>
                Пара свежих измерений tf
              </li>
            {/if}
          </ul>
          {#if run.fault}<p class="warning" role="status">⚠ Авария контура</p>{/if}
          <div class="actions">
            {#if observing}
              <button onclick={() => choose("live")}>К наблюдениям</button>
            {:else}
              <button onclick={() => command("arm")} disabled={!connection || !run.ready || run.compensation_ready === false || actionBusy}>Допустить</button>
              <button onclick={() => ask("start")} disabled={!connection || run.state !== "armed" || run.compensation_ready === false || actionBusy}>Пуск</button>
            {/if}
            {#if run.state === "fault_latched" && active}
              <button class="secondary" onclick={() => command("recover")} disabled={!connection || actionBusy}>Восстановить связь</button>
              <button class="secondary" onclick={() => ask("reset")} disabled={!connection || run.output_state !== "off_verified" || actionBusy}>Сбросить аварию</button>
            {/if}
          </div>
        </section>
        <section class="panel">
          <h2>Нагрузки и расчётная область</h2>
          <div class="table-wrap">
            <table>
              <thead><tr><th>Ветвь</th><th>Эталон, Ом</th><th>Из I/V, Ом</th><th>Номинал, Вт</th><th>С лимитом, Вт</th></tr></thead>
              <tbody>
                {#each ["A", "B"] as branch, index}
                  <tr><th>{branch}</th><td>{display(run.resistance_ohms[index])}</td><td>{display(run.measured_resistance_ohms[index])}</td><td>{display(run.estimate.limits.envelope_source_w[index])}</td><td>{display(run.estimate.limits.envelope_heater_w[index])}</td></tr>
                {/each}
              </tbody>
            </table>
          </div>
          <p class="muted">Эталон взят из нагрузки эмулятора. Номинал рассчитан по области источника и диапазону сопротивлений.</p>
          <p>Общий расчётный предел {display(run.estimate.limits.envelope_total_w, "Вт")}.</p>
          <p>Теплоприток проводки на stage2 {display(run.estimate.wire_heat_by_node_w[1], "Вт")} при нулевом токе.</p>
          <p>Теплоёмкость насадки при начальном tf {display(run.estimate.fixture_capacity_j_per_k, "Дж/К")}.</p>
        </section>
      </div>
    {:else}
      <p class="panel">Задайте сценарий и подготовьте прогон.</p>
    {/if}
  {:else if screen === "live"}
    {#if run}
      {#key run.id}
        <Live {run} busy={!connection || actionBusy || waitingChangeSeq !== null || !active}
          onapply={(change) => command("change", change)} {theme} />
      {/key}
    {:else}
      <h1>Ход опыта</h1>
      <p class="panel">После подготовки здесь появятся наблюдения контура.</p>
    {/if}
  {:else if screen === "review"}
    <h1>Разбор</h1>
    <Review {archives} {primary} {comparison} busy={jobBusy}
      onlist={() => void readJob("/api/archive", { kind: "list" }, "list")}
      onreplay={(id, compare) => void readJob("/api/archive", { kind: "replay", run: id }, compare ? "comparison" : "primary")}
      {theme} />
  {:else}
    <h1>Оборудование и настройки</h1>
    <div class="grid-two">
      <section class="panel">
        <h2>Эмуляторы</h2>
        <p>Keithley 2604B · две ветви A и B</p>
        <p>Lake Shore 218 · stage1, stage2, tf и jacket</p>
        <p class="warning">⚠ Аппаратный допуск закрыт</p>
        {#if daemon.test_faults && run?.output_state === "on_verified" && active}
          <button class="secondary" onclick={() => command("fault")} disabled={actionBusy}>Учебный отказ</button>
        {/if}
      </section>
      <section class="panel">
        <h2>Отображение</h2>
        <div class="actions">
          <button class="secondary" aria-pressed={theme === "light"} onclick={() => theme = "light"}>Светлая тема</button>
          <button class="secondary" aria-pressed={theme === "dark"} onclick={() => theme = "dark"}>Тёмная тема</button>
        </div>
        {#if run}
          <h2>Ограничения тестового профиля</h2>
          <dl class="settings-list">
            <dt>Источник на ветвь</dt><dd>{display(run.estimate.limits.source_voltage_v, "В")} / {display(run.estimate.limits.source_current_a, "А")} / {display(run.estimate.limits.source_power_w, "Вт")}</dd>
            <dt>Общий предел M5</dt><dd>{display(run.estimate.limits.protection_total_w, "Вт")}</dd>
            <dt>Возраст термометрии</dt><dd>{display(run.estimate.limits.max_temperature_age_ms, "мс")}</dd>
            <dt>Инверсия M5</dt><dd>{run.estimate.limits.inversion_k.map((value) => display(value, "К")).join(" / ")}</dd>
            <dt>Верхние пороги M5</dt><dd>{run.estimate.limits.temperature_k.map((value) => display(value, "К")).join(" / ")}</dd>
          </dl>
        {/if}
      </section>
    </div>
  {/if}
</main>

<dialog bind:this={confirmation} onclose={() => confirmKind = null} aria-labelledby="confirmation-title">
  <h2 id="confirmation-title">{confirmKind === "reset" ? "Сброс аварии" : "Включение нагрева"}</h2>
  <p>{confirmKind === "reset" ? "Подтвердите сброс защёлкнутой аварии." : "Подтвердите включение нагрева на эмуляторах."}</p>
  <form method="dialog" class="actions">
    <button class="secondary" value="cancel">Отмена</button>
    <button type="button" onclick={confirmAction}>{confirmKind === "reset" ? "Подтвердить сброс" : "Подтвердить пуск"}</button>
  </form>
</dialog>
