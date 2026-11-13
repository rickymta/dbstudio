// The destructive-statement confirmation that is waiting for the user. One at a time: a second
// request queues behind the first. The dialog host (`ConfirmDestructiveHost`) renders the head.

import { create } from 'zustand';
import type { ConfirmationGrant, ConfirmationRequest, IpcError } from '@/lib/types';

interface Entry {
  request: ConfirmationRequest;
  resolve: (grant: ConfirmationGrant) => void;
  reject: (error: IpcError) => void;
}

interface ConfirmState {
  queue: Entry[];
  push: (entry: Entry) => void;
  /** Remove the head entry (answered). */
  shift: () => void;
}

export const useConfirmStore = create<ConfirmState>((set) => ({
  queue: [],
  push: (entry) => set((s) => ({ queue: [...s.queue, entry] })),
  shift: () => set((s) => ({ queue: s.queue.slice(1) })),
}));

/** Ask the user to confirm; rejects with a `cancelled` error when they decline. */
export function askConfirmation(request: ConfirmationRequest): Promise<ConfirmationGrant> {
  return new Promise((resolve, reject) => {
    useConfirmStore.getState().push({ request, resolve, reject });
  });
}

export const CONFIRMATION_DECLINED: IpcError = {
  kind: 'cancelled',
  message: 'confirmation declined',
};
