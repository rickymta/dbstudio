import type { ComponentType } from 'react';
import { JobsIndicator } from '@/features/jobs/JobsIndicator';

/** Named places in the status bar that features fill. */
export type StatusBarSlotName = 'jobs';

/** Renderer of each slot; features replace their own entry. */
export const statusBarSlots: Record<StatusBarSlotName, ComponentType | undefined> = {
  jobs: JobsIndicator,
};
