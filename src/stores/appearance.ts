import { create } from 'zustand';

export type Density = 'compact' | 'comfortable';
export type MonoFont = 'system' | 'menlo' | 'consolas';

export interface Appearance {
  density: Density;
  uiFontSize: number;
  gridFontSize: number;
  editorFontSize: number;
  monoFont: MonoFont;
  ligatures: boolean;
}

export const DEFAULT_APPEARANCE: Appearance = {
  density: 'comfortable',
  uiFontSize: 13,
  gridFontSize: 13,
  editorFontSize: 14,
  monoFont: 'system',
  ligatures: false,
};

const STORAGE_KEY = 'dbstudio.appearance';
const FONTS: Record<MonoFont, string> = {
  system: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
  menlo: 'Menlo, ui-monospace, Consolas, monospace',
  consolas: 'Consolas, ui-monospace, Menlo, monospace',
};

function fontSize(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value)
    ? Math.min(24, Math.max(12, Math.round(value)))
    : fallback;
}

export function normalizeAppearance(value: Partial<Appearance>): Appearance {
  return {
    density: value.density === 'compact' ? 'compact' : 'comfortable',
    uiFontSize: fontSize(value.uiFontSize, DEFAULT_APPEARANCE.uiFontSize),
    gridFontSize: fontSize(value.gridFontSize, DEFAULT_APPEARANCE.gridFontSize),
    editorFontSize: fontSize(value.editorFontSize, DEFAULT_APPEARANCE.editorFontSize),
    monoFont:
      value.monoFont === 'menlo' || value.monoFont === 'consolas' ? value.monoFont : 'system',
    ligatures: value.ligatures === true,
  };
}

function readAppearance(): Appearance {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? 'null');
    if (value && typeof value === 'object') return normalizeAppearance(value);
  } catch {
    // A corrupt preference or unavailable storage must not prevent opening the app.
  }
  return { ...DEFAULT_APPEARANCE };
}

export function gridRowHeight(appearance: Appearance): number {
  return Math.max(appearance.density === 'compact' ? 24 : 30, appearance.gridFontSize + 12);
}

export function applyAppearance(value: Appearance): void {
  const root = document.documentElement;
  root.dataset.density = value.density;
  root.style.setProperty('--ui-font-size', `${value.uiFontSize}px`);
  root.style.setProperty('--grid-font-size', `${value.gridFontSize}px`);
  root.style.setProperty('--editor-font-size', `${value.editorFontSize}px`);
  root.style.setProperty('--data-font-family', FONTS[value.monoFont]);
  root.style.setProperty('--code-ligatures', value.ligatures ? 'normal' : 'none');
  root.style.setProperty('--grid-row-height', `${gridRowHeight(value)}px`);
}

interface AppearanceStore {
  appearance: Appearance;
  update: (patch: Partial<Appearance>) => void;
  reset: () => void;
}

export const useAppearanceStore = create<AppearanceStore>((set, get) => ({
  appearance: readAppearance(),
  update: (patch) => {
    const appearance = normalizeAppearance({ ...get().appearance, ...patch });
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(appearance));
    } catch {
      // Keep preferences usable in memory when storage is unavailable.
    }
    applyAppearance(appearance);
    set({ appearance });
  },
  reset: () => get().update(DEFAULT_APPEARANCE),
}));

export function initAppearance(): void {
  applyAppearance(useAppearanceStore.getState().appearance);
}
