import { invoke } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import type {
  ConfigSnapshot,
  DeviceInfo,
  EngineConfig,
  EngineStatus,
  HostInfo,
  LoadReport,
  Notice,
  PluginEntry,
  ScanProgress,
  SlotState,
  Telemetry,
} from './types';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return isTauri ? invoke<T>(cmd, args) : (mockInvoke(cmd, args ?? {}) as Promise<T>);
}

export const getStatus = () => call<EngineStatus>('get_status');
export const listHosts = () => call<HostInfo[]>('list_hosts');
export const listDevices = (host: string, dir: 'input' | 'output') =>
  call<DeviceInfo[]>('list_devices', { host, dir });
export const startEngine = (cfg: EngineConfig) => call<EngineStatus>('start_engine', { cfg });
export const stopEngine = () => call<void>('stop_engine');
export const getTelemetry = () => call<Telemetry>('get_telemetry');
export const listPlugins = () => call<PluginEntry[]>('list_plugins');
export const scanPlugins = () => call<PluginEntry[]>('scan_plugins');
export const addPlugin = (id: string, index: number) => call<void>('add_plugin', { id, index });
export const removePlugin = (index: number) => call<void>('remove_plugin', { index });
export const movePlugin = (from: number, to: number) => call<void>('move_plugin', { from, to });
export const setBypass = (index: number, bypass: boolean) =>
  call<void>('set_bypass', { index, bypass });
export const setGains = (inputDb: number, masterDb: number) =>
  call<void>('set_gains', { inputDb, masterDb });
export const setMonitor = (enabled: boolean) => call<void>('set_monitor', { enabled });
export const getAutostart = () => call<boolean>('get_autostart');
export const setAutostart = (enabled: boolean) => call<boolean>('set_autostart', { enabled });
export const openEditor = (index: number) => call<void>('open_editor', { index });
export const closeEditor = (index: number) => call<void>('close_editor', { index });
export const savePreset = (path: string) => call<void>('save_preset', { path });
export const loadPreset = (path: string) => call<LoadReport>('load_preset', { path });
export const pickSavePath = () => call<string | null>('pick_save_path');
export const pickOpenPath = () => call<string | null>('pick_open_path');
export const listScanDirs = () => call<string[]>('list_scan_dirs');
export const addScanDir = (path: string) => call<string[]>('add_scan_dir', { path });
export const removeScanDir = (path: string) => call<string[]>('remove_scan_dir', { path });
export const getConfig = () => call<ConfigSnapshot>('get_config');
export async function pickFolder(): Promise<string | null> {
  if (!isTauri) return 'C:\\VSTPlugins';
  const r = await openDialog({ directory: true, multiple: false, title: 'Выберите папку плагинов' });
  return typeof r === 'string' ? r : null;
}

// ---------------------------------------------------------------------------
// Mock backend: used under plain `vite dev` / build preview (no Tauri shell).
// ---------------------------------------------------------------------------

const mockPlugins: PluginEntry[] = [
  { id: 'vst3:ivgi2', name: 'IVGI2', vendor: 'Klanghelm', format: 'vst3', path: 'C:\\Program Files\\Common Files\\VST3\\IVGI2.vst3', has_gui: true },
  { id: 'vst2:tdeesser2', name: 'T-De-Esser 2', vendor: 'Techivation', format: 'vst2', path: 'C:\\VSTPlugins\\T-De-Esser 2.dll', has_gui: true },
  { id: 'vst3:proq4', name: 'Pro-Q 4', vendor: 'FabFilter', format: 'vst3', path: 'C:\\Program Files\\Common Files\\VST3\\Pro-Q 4.vst3', has_gui: true },
  { id: 'vst3:proc2', name: 'Pro-C 2', vendor: 'FabFilter', format: 'vst3', path: 'C:\\Program Files\\Common Files\\VST3\\Pro-C 2.vst3', has_gui: true },
  { id: 'vst3:prol2', name: 'Pro-L 2', vendor: 'FabFilter', format: 'vst3', path: 'C:\\Program Files\\Common Files\\VST3\\Pro-L 2.vst3', has_gui: true },
  { id: 'vst2:valveexciter', name: 'Valve Exciter', vendor: 'Tritik', format: 'vst2', path: 'C:\\VSTPlugins\\ValveExciter.dll', has_gui: false },
];

let mockScanDirs = ['C:\\Program Files\\Common Files\\VST3', 'C:\\Program Files\\VSTPlugins'];

const mockHosts: HostInfo[] = [
  { id: 'wasapi', name: 'WASAPI' },
  { id: 'asio', name: 'ASIO' },
  { id: 'ks', name: 'Kernel Streaming' },
];

const mockDevices: Record<string, { input: DeviceInfo[]; output: DeviceInfo[] }> = {
  wasapi: {
    input: [
      { id: 'fifine-sc3', name: 'fifine SC3', is_default: true },
      { id: 'vm-input', name: 'Voicemeeter Input (VB-Audio)', is_default: false },
      { id: 'realtek-mic', name: 'Microphone (Realtek Audio)', is_default: false },
    ],
    output: [
      { id: 'vm-out', name: 'Voicemeeter Input', is_default: true },
      { id: 'realtek-spkr', name: 'Speakers (Realtek Audio)', is_default: false },
      { id: 'fifine-mon', name: 'fifine SC3 Monitor', is_default: false },
    ],
  },
  asio: {
    input: [
      { id: 'vm-asio', name: 'Voicemeeter Virtual ASIO', is_default: true },
      { id: 'fifine-asio', name: 'fifine SC3 (ASIO)', is_default: false },
    ],
    output: [
      { id: 'vm-asio', name: 'Voicemeeter Virtual ASIO', is_default: true },
      { id: 'fifine-asio', name: 'fifine SC3 (ASIO)', is_default: false },
    ],
  },
  ks: {
    input: [{ id: 'fifine-ks', name: 'fifine SC3', is_default: true }],
    output: [{ id: 'realtek-ks', name: 'Speakers (Realtek Audio)', is_default: true }],
  },
};

const slot = (index: number, p: PluginEntry): SlotState => ({
  index,
  name: p.name,
  format: p.format,
  bypass: false,
  editor_open: false,
});

let mockChain: SlotState[] = [slot(0, mockPlugins[0]), slot(1, mockPlugins[1])];
let mockStatus: EngineStatus = { running: true, sample_rate: 48000, block_size: 128, latency_ms: 8.0 };
let mockInputDb = 0;
let mockMasterDb = 0;
let mockMonitorOn = false;
let mockAutostart = false;
let mockScan: ScanProgress | null = null;
let mockNotices: Notice[] = [];
let mockNoticeCount = 0;

const reindex = () => mockChain.forEach((s, i) => (s.index = i));
const clamp01 = (v: number) => Math.min(0.999, Math.max(0, v));
const gain = (db: number) => Math.pow(10, db / 20);
const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

function mockTelemetry(): Telemetry {
  const t = performance.now() / 1000;
  const running = mockStatus.running;
  const raw = running
    ? clamp01(0.38 + 0.22 * Math.sin(t * 1.7) + 0.14 * Math.sin(t * 5.3) + 0.06 * Math.sin(t * 13.1))
    : 0;
  const input = clamp01(raw * gain(mockInputDb));
  let lvl = input;
  const slotPeaks = mockChain.map((s, i) => {
    if (!s.bypass) lvl = clamp01(lvl * (0.88 + 0.05 * Math.sin(t * 2.1 + i)));
    return running ? lvl : 0;
  });
  const output = running ? clamp01(lvl * gain(mockMasterDb)) : 0;
  const notices = mockNotices;
  mockNotices = [];
  mockNoticeCount += notices.length;
  return {
    input_peak: input,
    output_peak: output,
    slot_peaks: slotPeaks,
    chain: mockChain.map((s) => ({ ...s })),
    underruns: 0,
    overflows: 0,
    callback_max_ns: running ? 1_450_000 + 300_000 * Math.sin(t * 0.7) : 0,
    callback_load_pct: running ? 14 + 5 * Math.sin(t * 0.9) : 0,
    latency_ms: mockStatus.latency_ms,
    scan: mockScan ? { ...mockScan } : null,
    notices,
    notice_count: mockNoticeCount,
  };
}

async function mockInvoke(cmd: string, args: Record<string, unknown>): Promise<unknown> {
  switch (cmd) {
    case 'get_status':
      return { ...mockStatus };
    case 'list_hosts':
      return mockHosts;
    case 'list_devices': {
      const host = mockDevices[args.host as string] ?? mockDevices.wasapi;
      return args.dir === 'output' ? host.output : host.input;
    }
    case 'start_engine': {
      const cfg = args.cfg as EngineConfig;
      await delay(120);
      mockStatus = {
        running: true,
        sample_rate: cfg.sample_rate,
        block_size: cfg.block_size,
        latency_ms: Math.round((cfg.block_size / cfg.sample_rate) * 3000 * 10) / 10,
      };
      return { ...mockStatus };
    }
    case 'stop_engine':
      await delay(80);
      mockStatus = { ...mockStatus, running: false };
      return null;
    case 'get_telemetry':
      return mockTelemetry();
    case 'list_plugins':
      return mockPlugins;
    case 'scan_plugins': {
      if (!mockScan) {
        mockScan = { done: 0, total: 24, current: 'Сканирование папок VST3…' };
        const iv = setInterval(() => {
          if (!mockScan) return clearInterval(iv);
          mockScan.done += 3;
          mockScan.current = mockScan.done > 12 ? 'Сканирование папок VST2…' : 'Сканирование папок VST3…';
          if (mockScan.done >= mockScan.total) {
            mockScan = null;
            clearInterval(iv);
          }
        }, 180);
      }
      await delay(1600);
      return mockPlugins;
    }
    case 'add_plugin': {
      const p = mockPlugins.find((x) => x.id === args.id);
      if (!p) throw new Error(`Unknown plugin id: ${args.id}`);
      const i = Math.min(args.index as number, mockChain.length);
      mockChain.splice(i, 0, slot(i, p));
      reindex();
      return null;
    }
    case 'remove_plugin':
      mockChain.splice(args.index as number, 1);
      reindex();
      return null;
    case 'move_plugin': {
      const [s] = mockChain.splice(args.from as number, 1);
      mockChain.splice(args.to as number, 0, s);
      reindex();
      return null;
    }
    case 'set_bypass':
      mockChain[args.index as number].bypass = args.bypass as boolean;
      return null;
    case 'set_gains':
      mockInputDb = args.inputDb as number;
      mockMasterDb = args.masterDb as number;
      return null;
    case 'set_monitor':
      mockMonitorOn = args.enabled as boolean;
      return null;
    case 'get_autostart':
      return mockAutostart;
    case 'set_autostart':
      mockAutostart = args.enabled as boolean;
      return mockAutostart;
    case 'open_editor':
      mockChain[args.index as number].editor_open = true;
      mockNotices.push({ kind: 'info', message: 'Окно редактора открыто (демо)' });
      return null;
    case 'close_editor':
      mockChain[args.index as number].editor_open = false;
      return null;
    case 'save_preset':
      mockNotices.push({ kind: 'info', message: `Пресет сохранён (демо): ${args.path}` });
      return null;
    case 'load_preset':
      await delay(100);
      return { loaded: mockChain.length, missing: [] } satisfies LoadReport;
    case 'pick_save_path':
      return 'C:\\Users\\andre\\Documents\\micrack-preset.json';
    case 'pick_open_path':
      return 'C:\\Users\\andre\\Documents\\micrack-preset.json';
    case 'list_scan_dirs':
      return [...mockScanDirs];
    case 'get_config':
      return { input_gain_db: mockInputDb, master_gain_db: mockMasterDb, sample_rate: mockStatus.sample_rate, block_size: mockStatus.block_size, monitor_on: mockMonitorOn };
    case 'add_scan_dir': {
      const p = args.path as string;
      if (!mockScanDirs.includes(p)) mockScanDirs.push(p);
      return [...mockScanDirs];
    }
    case 'remove_scan_dir':
      mockScanDirs = mockScanDirs.filter((d) => d !== args.path);
      return [...mockScanDirs];
    default:
      throw new Error(`Mock: unknown command ${cmd}`);
  }
}
