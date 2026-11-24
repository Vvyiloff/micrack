import * as ipc from './ipc';
import { ui } from './uiActivity.svelte';
import type {
  DeviceInfo,
  EngineConfig,
  EngineStatus,
  HostInfo,
  Notice,
  PluginEntry,
  SlotState,
  Telemetry,
} from './types';

export const fmtDb = (peak: number) =>
  peak <= 0.0001 ? '-∞' : `${(20 * Math.log10(peak)).toFixed(1)} dB`;

class AppStore {
  status = $state<EngineStatus | null>(null);
  telemetry = $state<Telemetry | null>(null);
  hosts = $state<HostInfo[]>([]);
  inputDevices = $state<DeviceInfo[]>([]);
  outputDevices = $state<DeviceInfo[]>([]);
  plugins = $state<PluginEntry[]>([]);
  scanDirs = $state<string[]>([]);
  cfg = $state<EngineConfig>({
    input: { host: 'wasapi', device: '' },
    output: { host: 'wasapi', device: '' },
    sample_rate: 48000,
    block_size: 128,
  });
  inputDb = $state(0);
  masterDb = $state(0);
  monitorOn = $state(false);
  processingOff = $state(false);
  autostart = $state(false);
  scanning = $state(false);
  cable = $state<ipc.VirtualCableStatus | null>(null);
  notices = $state<(Notice & { id: number; ts: number })[]>([]);

  chain: SlotState[] = $derived(this.telemetry?.chain ?? []);

  #started = false;
  #seenNoticeCount = 0;
  #toastId = 0;
  #hadCable: boolean | null = null;
  #bypassSnapshot: boolean[] = [];

  async init() {
    if (this.#started) return;
    this.#started = true;
    ui.start();
    this.checkCable();
    try {
      const [status, hosts, plugins, cfgSnap] = await Promise.all([
        ipc.getStatus(),
        ipc.listHosts(),
        ipc.listPlugins(),
        ipc.getConfig().catch(() => null),
      ]);
      this.status = status;
      this.hosts = hosts;
      this.plugins = plugins;
      this.scanDirs = await ipc.listScanDirs().catch(() => []);
      this.autostart = await ipc.getAutostart().catch(() => false);
      // Hydrate devices + gains from the saved/running config (M2): avoids the GUI
      // defaulting to Windows devices / zero gains and mis-routing on manual Start.
      if (cfgSnap) {
        if (cfgSnap.input?.device) this.cfg.input = { host: cfgSnap.input.host, device: cfgSnap.input.device };
        if (cfgSnap.output?.device) this.cfg.output = { host: cfgSnap.output.host, device: cfgSnap.output.device };
        if (cfgSnap.sample_rate) this.cfg.sample_rate = cfgSnap.sample_rate;
        if (cfgSnap.block_size) this.cfg.block_size = cfgSnap.block_size;
        this.inputDb = cfgSnap.input_gain_db ?? 0;
        this.masterDb = cfgSnap.master_gain_db ?? 0;
        this.monitorOn = cfgSnap.monitor_on ?? false;
      } else {
        if (status.sample_rate > 0) this.cfg.sample_rate = status.sample_rate;
        if (status.block_size > 0) this.cfg.block_size = status.block_size;
      }
      if (hosts.length && !hosts.some((h) => h.id === this.cfg.input.host)) {
        this.cfg.input.host = this.cfg.output.host = hosts[0].id;
      }
      await this.refreshDevices();
    } catch (e) {
      this.notify('error', String(e));
    }
    this.#poll();
  }

  async refreshDevices() {
    // N6: enumerate each side under its OWN host. Mixed-host configs (e.g. WASAPI
    // input + ASIO output) must not list output devices under the input host.
    [this.inputDevices, this.outputDevices] = await Promise.all([
      ipc.listDevices(this.cfg.input.host, 'input'),
      ipc.listDevices(this.cfg.output.host, 'output'),
    ]);
    const pick = (cur: string, list: DeviceInfo[]) =>
      list.some((d) => d.id === cur) ? cur : (list.find((d) => d.is_default) ?? list[0])?.id ?? '';
    this.cfg.input.device = pick(this.cfg.input.device, this.inputDevices);
    this.cfg.output.device = pick(this.cfg.output.device, this.outputDevices);
  }

  // Semi-auto VB-CABLE: detect, and on a missing→present transition while the
  // app runs, switch WASAPI output to the cable. Never overrides a saved choice
  // on the first check (#hadCable === null).
  async checkCable() {
    try {
      const c = await ipc.virtualCableStatus();
      const appeared = this.#hadCable === false && !!c.cable_input;
      this.#hadCable = !!c.cable_input;
      this.cable = c;
      if (appeared && this.cfg.output.host === 'wasapi' && !this.cfg.output.device.includes('CABLE Input')) {
        this.cfg.output = { host: 'wasapi', device: c.cable_input! };
        await this.refreshDevices();
        await this.applyConfig();
        this.notify('info', 'VB-CABLE найден: вывод переключён на CABLE Input. В Discord/OBS выберите микрофон «CABLE Output».');
      }
    } catch {
      /* backend without the command — ignore */
    }
  }

  async setHost(host: string) {
    this.cfg.input.host = this.cfg.output.host = host;
    await this.refreshDevices();
    await this.applyConfig();
  }

  async applyConfig() {
    if (!this.status?.running) return;
    try {
      this.status = await ipc.startEngine(this.cfg);
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async toggleEngine() {
    try {
      if (this.status?.running) {
        await ipc.stopEngine();
        this.status = await ipc.getStatus();
      } else {
        this.status = await ipc.startEngine(this.cfg);
      }
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  // Discord/OBS listen to Voicemeeter B1, which is fed ONLY by micrack's output.
  // Stopping the engine therefore kills the mic for them. "Processing off" must
  // instead bypass every plugin and keep streaming the dry mic.
  async toggleProcessing() {
    if (!this.status?.running) {
      await this.toggleEngine();
      return;
    }
    try {
      if (!this.processingOff) {
        this.#bypassSnapshot = this.chain.map((s) => s.bypass);
        for (let i = 0; i < this.chain.length; i++) await ipc.setBypass(i, true);
        this.processingOff = true;
      } else {
        for (let i = 0; i < this.chain.length; i++) {
          await ipc.setBypass(i, this.#bypassSnapshot[i] ?? false);
        }
        this.processingOff = false;
      }
      this.telemetry = await ipc.getTelemetry();
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async setInputGain(db: number) {
    this.inputDb = db;
    await this.#gains();
  }

  async setMasterGain(db: number) {
    this.masterDb = db;
    await this.#gains();
  }

  async toggleAutostart() {
    try {
      this.autostart = await ipc.setAutostart(!this.autostart);
      this.notify('info', this.autostart ? 'Автозапуск включён' : 'Автозапуск выключен');
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async toggleMonitor() {
    const next = !this.monitorOn;
    try {
      await ipc.setMonitor(next);
      this.monitorOn = next;
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async #gains() {
    try {
      await ipc.setGains(this.inputDb, this.masterDb);
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async add(id: string) {
    await this.#chainOp(() => ipc.addPlugin(id, this.chain.length));
  }
  async remove(index: number) {
    await this.#chainOp(() => ipc.removePlugin(index));
  }
  async move(from: number, to: number) {
    if (to < 0 || to >= this.chain.length) return;
    await this.#chainOp(() => ipc.movePlugin(from, to));
  }
  async bypass(index: number, value: boolean) {
    await this.#chainOp(() => ipc.setBypass(index, value));
  }
  async editor(index: number, open: boolean) {
    await this.#chainOp(() => (open ? ipc.openEditor(index) : ipc.closeEditor(index)));
  }

  async #chainOp(op: () => Promise<void>) {
    try {
      await op();
      this.telemetry = await ipc.getTelemetry();
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async scan() {
    if (this.scanning) return;
    this.scanning = true;
    try {
      this.plugins = await ipc.scanPlugins();
      this.notify('info', `Сканирование завершено: ${this.plugins.length} плагинов`);
    } catch (e) {
      this.notify('error', String(e));
    } finally {
      this.scanning = false;
    }
  }

  async addScanDir() {
    try {
      const path = await ipc.pickFolder();
      if (!path) return;
      this.scanDirs = await ipc.addScanDir(path);
      this.plugins = await ipc.listPlugins();
      this.notify('info', `Папка добавлена: ${path}`);
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async removeScanDir(path: string) {
    try {
      this.scanDirs = await ipc.removeScanDir(path);
      this.plugins = await ipc.listPlugins();
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async savePreset() {
    try {
      const path = await ipc.pickSavePath();
      if (!path) return;
      await ipc.savePreset(path);
      this.notify('info', `Пресет сохранён: ${path}`);
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  async loadPreset() {
    try {
      const path = await ipc.pickOpenPath();
      if (!path) return;
      const report = await ipc.loadPreset(path);
      this.telemetry = await ipc.getTelemetry();
      if (report.missing.length) {
        this.notify('warn', `Загружено: ${report.loaded}, отсутствуют: ${report.missing.join(', ')}`);
      } else {
        this.notify('info', `Пресет загружен (${report.loaded} плагинов)`);
      }
    } catch (e) {
      this.notify('error', String(e));
    }
  }

  notify(kind: string, message: string) {
    this.notices = [...this.notices.slice(-4), { kind, message, id: ++this.#toastId, ts: performance.now() }];
  }

  // Time-based telemetry poll (~30/s), independent of display refresh rate.
  // Drops to a slow heartbeat while the window is minimized or hidden.
  #poll() {
    let inflight = false;
    let lastStatus = 0;
    let lastCable = 0;
    const tick = () => {
      const now = performance.now();
      if (this.notices.length && this.notices.some((n) => now - n.ts > 5000)) {
        this.notices = this.notices.filter((n) => now - n.ts <= 5000);
      }
      if (ui.active) {
        if (!inflight) {
          inflight = true;
          ipc
            .getTelemetry()
            .then((t) => {
              this.telemetry = t;
              const unread = Math.min(t.notices.length, Math.max(0, t.notice_count - this.#seenNoticeCount));
              this.#seenNoticeCount = t.notice_count;
              for (const n of t.notices.slice(t.notices.length - unread)) {
                this.notify(n.kind, n.message);
              }
            })
            .catch(() => {})
            .finally(() => (inflight = false));
        }
        if (now - lastStatus > 2000) {
          lastStatus = now;
          ipc.getStatus().then((s) => (this.status = s)).catch(() => {});
        }
        if (now - lastCable > 10000) {
          lastCable = now;
          this.checkCable();
        }
      }
      window.setTimeout(tick, ui.active ? 33 : 500);
    };
    tick();
  }
}

export const store = new AppStore();
