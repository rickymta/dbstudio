import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useCallback } from 'react';
import { ipc } from '@/lib/ipc';
import { DEFAULT_SETTINGS, type AppSettings } from '@/lib/types';

/** Query key of the application settings. */
export const settingsQueryKey = ['settings'] as const;

/** Current settings; the defaults while they are loading or if loading failed. */
export function useAppSettings(): AppSettings {
  const { data } = useQuery({
    queryKey: settingsQueryKey,
    queryFn: ipc.settingsGet,
    staleTime: Infinity,
  });
  return data ?? DEFAULT_SETTINGS;
}

/** Persist settings through the backend and update the cache with what it returns. */
export function useSaveSettings() {
  const qc = useQueryClient();
  return useCallback(
    async (settings: AppSettings) => {
      const saved = await ipc.settingsSet(settings);
      qc.setQueryData(settingsQueryKey, saved);
      return saved;
    },
    [qc],
  );
}

export const ROWS_RANGE = { min: 1_000, max: 5_000_000 } as const;
export const CELLS_RANGE = { min: 100_000, max: 200_000_000 } as const;

/** Parse a whole number inside `[min, max]`; digits may be grouped with spaces, commas or dots. */
export function parseBounded(text: string, range: { min: number; max: number }): number | null {
  const cleaned = text.trim().replace(/[\s,.]/g, '');
  if (!/^\d+$/.test(cleaned)) return null;
  const n = Number(cleaned);
  return n >= range.min && n <= range.max ? n : null;
}

/**
 * Whole-number settings of the dialog and the ranges the backend accepts
 * (`crates/dbstudio-store/src/settings.rs`). `zeroIsOff` also accepts 0 below `min`.
 */
export const NUMBER_FIELDS = {
  editorMaxRows: { min: 100, max: 1_000_000, zeroIsOff: false },
  /** Stored as `queryTimeoutMs`; 0 = no timeout. */
  queryTimeoutSecs: { min: 1, max: 86_400, zeroIsOff: true },
  historyMaxPerConnection: { min: 100, max: 100_000, zeroIsOff: false },
  keepaliveSecs: { min: 30, max: 3_600, zeroIsOff: true },
  planHistoryMaxPerConnection: { min: 10, max: 5_000, zeroIsOff: false },
} as const;

export type NumberFieldKey = keyof typeof NUMBER_FIELDS;

export function parseNumberField(key: NumberFieldKey, text: string): number | null {
  const field = NUMBER_FIELDS[key];
  if (field.zeroIsOff && /^0+$/.test(text.trim())) return 0;
  return parseBounded(text, field);
}
