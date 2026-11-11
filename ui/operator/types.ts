export type Theme = "light" | "dark";
export type ThermalNode = "stage1" | "stage2" | "tf" | "jacket";
export type OutputState = "unknown" | "off_verified" | "on_verified";
export type BodyPort = "stage2" | "tf";
export type BridgeSetup =
  | { kind: "constant"; conductance_w_per_k: number }
  | { kind: "conductor"; material: string; area_m2: number; length_m: number }
  | { kind: "table"; points: [number, number][] }
  | { kind: "series"; contact_w_per_k: number; material: string; area_m2: number; length_m: number };

export type ModeSetup =
  | { kind: "observe" }
  | { kind: "constant"; power_w: number }
  | { kind: "power_profile"; points: { elapsed_ms: number; power_w: number }[] }
  | { kind: "temperature_hold"; setpoint_k: number }
  | { kind: "virtual_body"; port: BodyPort; body_heat_w: number; compensation?: { fixture_parasitic_w?: number } };

export type ModeKind = ModeSetup["kind"];
export type ModeChoice = ModeKind | "a1" | "compensation";

export const modeLabels: Record<ModeChoice, string> = {
  a1: "Виртуальное тело (A1)",
  observe: "Наблюдение",
  constant: "Постоянная мощность",
  power_profile: "Профиль мощности P(t)",
  temperature_hold: "Удержание stage2",
  virtual_body: "Виртуальное тело",
  compensation: "Компенсация насадки (test_only)",
};

export function modeChoice(mode?: ModeSetup): ModeChoice {
  return mode?.kind === "virtual_body" && mode.compensation ? "compensation" : mode?.kind ?? "a1";
}

export const bridgeLabels: Record<BridgeSetup["kind"], string> = {
  constant: "Постоянный G",
  conductor: "Проводник",
  table: "Таблица G(T)",
  series: "Контакт и проводник",
};

export const resultReasons: Record<string, string> = {
  compensation_missing_tf: "Нет пригодного измерения tf",
  compensation_pair_not_ready: "Нет двух последовательных свежих измерений tf",
  compensation_fixture_invalid: "Теплоёмкость насадки не определяется при текущем tf",
  port_unavailable: "Нет пригодного измерения порта",
  body_edit_invalid_at_tick: "Параметры неприменимы при текущих температурах тела и порта",
  negative_request: "Запрошена отрицательная мощность",
  operator_stop: "Остановлено оператором",
};

export interface Setup {
  mode?: ModeSetup;
  body: {
    material: string;
    mass_kg: number;
    temperature_k: number;
    scale: number;
    bridge: BridgeSetup;
  };
  wiring: {
    from: ThermalNode;
    to: ThermalNode;
    material: string;
    length_m: number;
    area_mm2: number;
    strands: number;
  }[];
  fixture: { material: string; mass_kg: number }[];
}

export interface PreviewPoint {
  time_s: number;
  body_k: number;
  temperatures_k: [number, number, number, number];
  bridge_w: number;
  heater_w: number;
}

interface HistoricalPoint {
  stage1_k: number;
  stage2_k: number;
  p_net_w: number;
  origin: "historical_anchor" | "interpolated" | "synthetic";
  source_row: number | null;
}

export interface HistoricalReport {
  synthetic: boolean;
  test_only: boolean;
  source_path: string;
  source_sha256: string;
  nominal_rows: number[];
  liquid_nitrogen: boolean;
  radiation_shield: boolean;
  parasitics_removed: boolean;
  continuation_from_k: number;
  continuation_to_k: number;
  power_slope_w_per_k: number;
  stage_gap_at_start_k: number;
  points: HistoricalPoint[];
}

export interface Limits {
  envelope_source_w: [number, number];
  envelope_heater_w: [number, number];
  envelope_total_w: number;
  source_power_w: number;
  source_current_a: number;
  source_voltage_v: number;
  protection_total_w: number;
  protection_branch_w: [number, number];
  protection_current_a: [number, number];
  protection_voltage_v: [number, number];
  inversion_k: [number, number, number];
  temperature_k: [number, number, number];
  min_temperature_k: number;
  max_rate_k_per_s: number;
  max_temperature_age_ms: number;
}

export interface Estimate {
  preview: PreviewPoint[];
  wiring: { from: ThermalNode; to: ThermalNode; heat_from_w: number; heat_to_w: number; source: string }[];
  wire_heat_by_node_w: [number, number, number, number];
  fixture_capacity_j_per_k: number;
  compensation?: { fixture_parasitic_w: number; fixture_capacity_j_per_k: number };
  limits: Limits;
}

export interface ChangePayload {
  mass_kg?: number;
  scale?: number;
  g_w_per_k?: number;
  material?: string;
  bridge?: BridgeSetup;
}

export interface AppliedChange {
  mass_kg?: number | null;
  scale: number | null;
  bridge_w_per_k: number | null;
  dh_event_j: number | null;
  temperature_before_k: number;
  temperature_after_k: number;
}

export type ModeChange = {
  request: ChangePayload;
  outcome:
    | { status: "applied"; temperature_before_k: number; temperature_after_k: number; dh_event_j: number | null }
    | { status: "rejected" };
};

export type ModeCompensation =
  | {
      status: "ready";
      previous_tf_k: number;
      previous_read_started_ns: number;
      current_tf_k: number;
      current_read_started_ns: number;
      d_tf_k_per_s: number;
      fixture_capacity_j_per_k: number;
      fixture_parasitic_w: number;
    }
  | { status: "unavailable"; reason: "missing_tf" | "pair_not_ready" | "fixture_invalid" };

export interface TimelineSample {
  tick: number;
  seq: number;
  measured_ns: number;
  temperatures_k: [number | null, number | null, number | null, number | null];
  body_k: number | null;
  measured_w: [number | null, number | null];
  requested_w: number | null;
  bridge_flow_w: number | null;
  tracking_error_w: number | null;
  inversion_delta_k: number | null;
  limited_w: number | null;
  targets_w: [number, number] | null;
  slew_limited: boolean;
  power_limited: boolean;
  current_limited: boolean;
  equivalence: string | null;
  change: AppliedChange | null;
  mode_v1?: {
    kind: ModeKind;
    hold_setpoint_k?: number | null;
    port?: BodyPort | null;
    held_age_ns?: number | null;
    compensation?: ModeCompensation | null;
    change?: ModeChange | null;
  };
}

export interface RunView {
  id: string;
  state: string;
  output_state: OutputState;
  fault: string | null;
  fallback_requested: boolean;
  ready: boolean;
  writer_ready: boolean;
  source_ready: boolean;
  thermometer_ready: boolean;
  observation_clear: boolean;
  journal: "opening" | "recording" | "closing" | "completed" | "failed" | "timed_out";
  archive_error: string | null;
  mode?: ModeSetup;
  compensation_ready?: boolean;
  off_reason?: string | null;
  resistance_ohms: [number, number];
  measured_resistance_ohms: [number | null, number | null];
  estimate: Estimate;
  parameters: { mass_kg: number; scale: number; g_w_per_k: number | null; material?: string; bridge?: BridgeSetup } | null;
  samples: TimelineSample[];
  samples_seen: number;
  events: { seq: number; kind: string; at_ns: number; code: string }[];
  last_change: { tick: number; seq: number; change: AppliedChange } | null;
  last_mode_change?: { tick: number; seq: number; change: ModeChange; reason?: string };
}

export interface DaemonState {
  run: RunView | null;
  last_action: { id: number; ok: boolean; message: string | null; cancelled_id: number | null } | null;
  test_faults: boolean;
}

export interface PlotSeries {
  label: string;
  color: string;
  values: (number | null)[];
  dash?: number[];
}

export interface ArchiveReceipt {
  run: string;
  generation: number;
  last_seq: number;
  output_state: OutputState;
}

export interface AnalysisEstimate {
  nominal: number;
  std_dev: number;
  p05: number;
  median: number;
  p95: number;
}

export interface AnalysisWindow {
  start_s: number;
  end_s: number;
  intervals: number;
  mean_temperatures_k: [number, number, number, number];
  mean_heater_w: number;
  derivative_at_s: number;
  derivative_stage2_k_per_s: AnalysisEstimate;
  derivative_tf_k_per_s: AnalysisEstimate;
  fixture_compensation_delta_w: number;
}

export interface CoolingAnalysis {
  status: "определяется" | "не определяется";
  reason: string | null;
  profile: string;
  seed: number;
  draws: number;
  rejected_draws: number;
  window: AnalysisWindow | null;
  paired_window: AnalysisWindow | null;
  q2_eff_w: AnalysisEstimate | null;
  identified_cf_j_per_k: AnalysisEstimate | null;
}

export interface ArchiveReview {
  report: {
    run: string;
    generation: number;
    status: string;
    raw_verified_records: number;
    raw_verified_frames: number;
    raw_tail: string;
    replayed_ticks: number;
    last_replayed_tick: number | null;
    historical_completion: string;
    output_state_at_boundary: OutputState;
    first_diagnostic: { status: string; seq: number; message: string } | null;
    a1?: { thermal_steps: number; last_interval_end_ns: number | null };
    mode?: ModeKind;
    fixture_compensation?: true;
  };
  samples: TimelineSample[];
  last_verified_seq: number;
  omitted_ticks: number;
  receipt_verified: boolean;
  receipt: ArchiveReceipt;
  diagnostics: Pick<Setup, "wiring" | "fixture">;
  analysis: CoolingAnalysis;
}
