import { create } from 'zustand';
import type { ObjectPath, Uuid } from '@/lib/types';

export interface RevealRequest {
  connectionId: Uuid;
  path: ObjectPath;
  /** Distinguishes repeated requests for the same node. */
  nonce: number;
}

interface RevealState {
  request: RevealRequest | null;
  /** Ask the connection's object tree to expand down to `path` and focus it. */
  reveal: (connectionId: Uuid, path: ObjectPath) => void;
  /** Mark the pending request as taken so a tree mounted later does not replay it. */
  consume: (nonce: number) => void;
}

let nonce = 0;

export const useRevealStore = create<RevealState>((set) => ({
  request: null,
  reveal: (connectionId, path) => set({ request: { connectionId, path, nonce: ++nonce } }),
  consume: (n) => set((s) => (s.request?.nonce === n ? { request: null } : s)),
}));
