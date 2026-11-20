import { create } from 'zustand';

/** Whether the command palette (APP-08) is shown. */
export const useCommandPalette = create<{ open: boolean; setOpen: (open: boolean) => void }>(
  (set) => ({
    open: false,
    setOpen: (open) => set({ open }),
  }),
);

/** Whether the settings dialog is shown (opened from the status bar, Ctrl+, or the palette). */
export const useSettingsDialog = create<{ open: boolean; setOpen: (open: boolean) => void }>(
  (set) => ({
    open: false,
    setOpen: (open) => set({ open }),
  }),
);

/** Whether the Activity Logs window is shown. */
export const useActivityWindow = create<{ open: boolean; setOpen: (open: boolean) => void }>(
  (set) => ({
    open: false,
    setOpen: (open) => set({ open }),
  }),
);

/** Whether the Help & Support dialog is shown. */
export const useHelpDialog = create<{ open: boolean; setOpen: (open: boolean) => void }>((set) => ({
  open: false,
  setOpen: (open) => set({ open }),
}));

export type CommandGroup = 'action' | 'tab' | 'connection' | 'object';

export interface PaletteCommand {
  id: string;
  group: CommandGroup;
  /** Translated text the user reads and searches. */
  label: string;
  /** Secondary text (connection name, shortcut context); also searched. */
  detail?: string;
  /** Formatted keyboard shortcut, shown at the right. */
  keys?: string;
  run: () => void;
}

const fold = (text: string) =>
  text.normalize('NFD').replace(/\p{M}/gu, '').replace(/đ/gi, 'd').toLowerCase();

/**
 * Commands matching every word of `query` (accents and case ignored), best first: a label that
 * starts with the query, then a word that starts with it, then any other match. Order within a
 * rank is the given order.
 */
export function filterCommands(commands: PaletteCommand[], query: string): PaletteCommand[] {
  const words = fold(query).split(/\s+/).filter(Boolean);
  if (words.length === 0) return commands;
  const whole = words.join(' ');
  const ranked: { command: PaletteCommand; rank: number; index: number }[] = [];
  commands.forEach((command, index) => {
    const label = fold(command.label);
    const haystack = `${label} ${fold(command.detail ?? '')}`;
    if (!words.every((w) => haystack.includes(w))) return;
    const rank = label.startsWith(whole)
      ? 0
      : words.every((w) => label.split(/[\s:›./_-]+/).some((part) => part.startsWith(w)))
        ? 1
        : 2;
    ranked.push({ command, rank, index });
  });
  return ranked.sort((a, b) => a.rank - b.rank || a.index - b.index).map((r) => r.command);
}
