// Launchpad: one page across connections (unfinished work, connection health, pinned and recent
// objects, jobs). It covers the tab content while open (it is not a tab); picking any tab, or
// opening something from it, closes it — the GitStudio Launchpad works the same way.

import { create } from 'zustand';
import { useTabsStore } from '@/stores/tabs';

export type LaunchpadSection = 'work' | 'connections' | 'objects' | 'jobs';
export type LaunchpadScope = 'workspace' | 'all';

const SCOPE_KEY = 'dbstudio.launchpad.scope';

function readScope(): LaunchpadScope {
  try {
    return localStorage.getItem(SCOPE_KEY) === 'all' ? 'all' : 'workspace';
  } catch {
    return 'workspace';
  }
}

interface LaunchpadState {
  open: boolean;
  section: LaunchpadSection;
  /** Active workspace only, or every connection. Remembered. */
  scope: LaunchpadScope;
  setOpen: (open: boolean) => void;
  toggle: () => void;
  setSection: (section: LaunchpadSection) => void;
  setScope: (scope: LaunchpadScope) => void;
}

export const useLaunchpad = create<LaunchpadState>((set) => ({
  open: false,
  section: 'work',
  scope: readScope(),
  setOpen: (open) => set({ open }),
  toggle: () => set((s) => ({ open: !s.open })),
  setSection: (section) => set({ section }),
  setScope: (scope) => {
    try {
      localStorage.setItem(SCOPE_KEY, scope);
    } catch {
      // storage unavailable
    }
    set({ scope });
  },
}));

// Activating or opening a tab (or switching workspace) shows that tab: the Launchpad steps aside.
useTabsStore.subscribe((s, old) => {
  if (s.activeId !== old.activeId || s.tabs.length > old.tabs.length || s.scope !== old.scope) {
    if (useLaunchpad.getState().open) useLaunchpad.getState().setOpen(false);
  }
});
