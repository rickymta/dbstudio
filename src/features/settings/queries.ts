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
