import { useSyncExternalStore } from 'react';
import type { GridModel } from './gridModel';

/** Re-render when the grid model changes; returns the model's version. */
export function useGridVersion(model: GridModel): number {
  return useSyncExternalStore(model.subscribe, model.getVersion);
}
