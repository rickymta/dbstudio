import { useState } from 'react';
import { create } from 'zustand';
import type { Layout } from 'react-resizable-panels';

export const useWorkspaceStore = create<{
  sidebarHidden: boolean;
  toggleSidebar: () => void;
  showSidebar: () => void;
}>((set) => ({
  sidebarHidden: false,
  toggleSidebar: () => set((s) => ({ sidebarHidden: !s.sidebarHidden })),
  showSidebar: () => set({ sidebarHidden: false }),
}));

/** Only layout geometry is stored here, never SQL or connection credentials. */
export function useSavedLayout(key: string) {
  const storageKey = `dbstudio.layout.${key}`;
  const [defaultLayout] = useState<Layout | undefined>(() => {
    try {
      const value: unknown = JSON.parse(localStorage.getItem(storageKey) ?? 'null');
      if (value && typeof value === 'object' && !Array.isArray(value)) {
        const entries = Object.entries(value);
        if (
          entries.length > 0 &&
          entries.every(
            ([, n]) => typeof n === 'number' && Number.isFinite(n) && n >= 0 && n <= 100,
          )
        )
          return value as Layout;
      }
    } catch {
      /* storage unavailable */
    }
    return undefined;
  });
  return {
    defaultLayout,
    onLayoutChanged: (layout: Layout) => {
      try {
        localStorage.setItem(storageKey, JSON.stringify(layout));
      } catch {
        /* storage unavailable */
      }
    },
  };
}
