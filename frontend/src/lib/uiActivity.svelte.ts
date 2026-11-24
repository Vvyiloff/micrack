import { getCurrentWindow } from '@tauri-apps/api/window';
import { listen } from '@tauri-apps/api/event';

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

// WebView2 keeps requestAnimationFrame running while the window is minimized,
// so animation and polling must be gated on real visibility, not on rAF alone.
class UiActivity {
  active = $state(true);
  #started = false;

  start() {
    if (this.#started || typeof window === 'undefined') return;
    this.#started = true;

    const sync = async () => {
      let minimized = false;
      if (isTauri) {
        try {
          minimized = await getCurrentWindow().isMinimized();
        } catch {
          /* window not ready yet */
        }
      }
      this.active = !document.hidden && !minimized;
    };

    document.addEventListener('visibilitychange', sync);
    window.addEventListener('focus', sync);
    window.addEventListener('blur', sync);
    if (isTauri) listen('tauri://resize', sync).catch(() => {});
    // Minimizing does not reliably emit a DOM event in WebView2.
    window.setInterval(sync, 1000);
    sync();
  }
}

export const ui = new UiActivity();