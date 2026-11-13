import { create } from 'zustand';

interface HelpState {
  open: boolean;
  setOpen: (open: boolean) => void;
}

/** Whether the keyboard cheat-sheet is shown. */
export const useShortcutsHelp = create<HelpState>((set) => ({
  open: false,
  setOpen: (open) => set({ open }),
}));
