import { getCurrentWebview } from '@tauri-apps/api/webview';
import { create } from 'zustand';
import { ipcTransport } from '@/lib/ipc';

const ZOOM_KEY = 'dbstudio.zoom';
export const ZOOM_MIN = 0.6;
export const ZOOM_MAX = 2;
export const ZOOM_STEP = 0.1;

const clamp = (z: number) => Math.round(Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, z)) * 10) / 10;

function initialZoom(): number {
  try {
    const v = Number(localStorage.getItem(ZOOM_KEY));
    return v >= ZOOM_MIN && v <= ZOOM_MAX ? clamp(v) : 1;
  } catch {
    return 1;
  }
}

/** Zoom of the whole interface, shared by the status bar and the keyboard shortcuts. */
export const useZoom = create<{ zoom: number; setZoom: (z: number) => void }>((set) => ({
  zoom: initialZoom(),
  setZoom: (z) => {
    const zoom = clamp(z);
    set({ zoom });
    try {
      localStorage.setItem(ZOOM_KEY, String(zoom));
    } catch {
      // storage unavailable
    }
  },
}));

/**
 * Apply the zoom: the webview's own zoom in the desktop app (the layout, the virtualized grids
 * and the editor all see consistent sizes), CSS `zoom` in a plain browser.
 */
export function applyZoom(zoom: number): void {
  if (ipcTransport() === 'tauri') {
    void getCurrentWebview()
      .setZoom(zoom)
      .catch(() => applyCssZoom(zoom));
  } else {
    applyCssZoom(zoom);
  }
}

function applyCssZoom(zoom: number): void {
  (document.documentElement.style as CSSStyleDeclaration & { zoom: string }).zoom =
    zoom === 1 ? '' : String(zoom);
}
