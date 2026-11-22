import { create } from 'zustand';
import type { Uuid } from '@/lib/types';

/**
 * Last failed connect of each connection (message already translated), until it connects. Kept
 * outside the sidebar so the workspace overview and the Launchpad can show it too.
 */
interface ConnectErrorsState {
  byConnection: Record<Uuid, { message: string; at: number }>;
  set: (connectionId: Uuid, message: string) => void;
  clear: (connectionId: Uuid) => void;
}

export const useConnectErrors = create<ConnectErrorsState>((set) => ({
  byConnection: {},
  set: (connectionId, message) =>
    set((s) => ({
      byConnection: { ...s.byConnection, [connectionId]: { message, at: Date.now() } },
    })),
  clear: (connectionId) =>
    set((s) => {
      if (!(connectionId in s.byConnection)) return s;
      const byConnection = { ...s.byConnection };
      delete byConnection[connectionId];
      return { byConnection };
    }),
}));
