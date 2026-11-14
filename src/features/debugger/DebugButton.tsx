import type { FC } from 'react';
import type { EditorMode } from '@/stores/tabs';

export interface DebugButtonProps {
  tabStateId: string;
  sessionId: string | null;
  connectionId: string | null;
  mode: EditorMode;
}

/** The "Debug" split button of the editor toolbar. Contract placeholder: package F fills it. */
export const DebugButton: FC<DebugButtonProps> = () => null;
