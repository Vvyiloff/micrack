export interface EngineStatus {
  running: boolean;
  sample_rate: number;
  block_size: number;
  latency_ms: number;
}

export interface HostInfo {
  id: string;
  name: string;
}

export interface DeviceInfo {
  id: string;
  name: string;
  is_default: boolean;
}

export interface EngineConfig {
  input: { host: string; device: string };
  output: { host: string; device: string };
  sample_rate: number;
  block_size: number;
}

export type PluginFormat = 'vst2' | 'vst3';

export interface PluginEntry {
  id: string;
  name: string;
  vendor: string;
  format: PluginFormat;
  path: string;
  has_gui: boolean;
}

export interface SlotState {
  index: number;
  name: string;
  format: PluginFormat;
  bypass: boolean;
  editor_open: boolean;
}

export interface Notice {
  kind: string;
  message: string;
}

// Assumed shape; reconcile with Rust ScanProgress at integration.
export interface ScanProgress {
  done: number;
  total: number;
  current: string;
}

export interface Telemetry {
  input_peak: number;
  output_peak: number;
  slot_peaks: number[];
  chain: SlotState[];
  underruns: number;
  overflows: number;
  callback_max_ns: number;
  callback_load_pct: number;
  latency_ms: number;
  scan: ScanProgress | null;
  notices: Notice[];
  notice_count: number;
}

export interface LoadReport {
  loaded: number;
  missing: string[];
}

export interface ConfigSnapshot {
  input?: { host: string; device: string };
  output?: { host: string; device: string };
  sample_rate?: number;
  block_size?: number;
  input_gain_db: number;
  master_gain_db: number;
  monitor_on?: boolean;
}
