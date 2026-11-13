import { create } from 'zustand';

export type ThemeMode = 'system' | 'light' | 'dark';
export const THEME_MODES: readonly ThemeMode[] = ['system', 'light', 'dark'];

const STORAGE_KEY = 'dbstudio.theme';

function readStored(): ThemeMode {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    // storage unavailable
  }
  return 'system';
}

function systemPrefersDark(): boolean {
  return typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)').matches
    : false;
}

/** Apply the mode to <html>: toggles the `dark` class and the `data-theme` attribute. */
export function applyTheme(mode: ThemeMode): void {
  const dark = mode === 'dark' || (mode === 'system' && systemPrefersDark());
  const root = document.documentElement;
  root.classList.toggle('dark', dark);
  root.dataset.theme = mode;
  root.style.colorScheme = dark ? 'dark' : 'light';
}

interface ThemeState {
  mode: ThemeMode;
  setMode: (mode: ThemeMode) => void;
}

export const useThemeStore = create<ThemeState>((set) => ({
  mode: readStored(),
  setMode: (mode) => {
    try {
      localStorage.setItem(STORAGE_KEY, mode);
    } catch {
      // storage unavailable
    }
    applyTheme(mode);
    set({ mode });
  },
}));

/** Apply the stored theme and follow OS changes while in `system` mode. Returns a cleanup. */
export function initTheme(): () => void {
  applyTheme(useThemeStore.getState().mode);
  if (typeof window.matchMedia !== 'function') return () => {};
  const media = window.matchMedia('(prefers-color-scheme: dark)');
  const onChange = () => {
    if (useThemeStore.getState().mode === 'system') applyTheme('system');
  };
  media.addEventListener('change', onChange);
  return () => media.removeEventListener('change', onChange);
}
