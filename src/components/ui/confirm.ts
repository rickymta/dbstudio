import { create } from 'zustand';

/** A question shown in the app's own confirm dialog (instead of the webview's `window.confirm`). */
export interface ConfirmOptions {
  title: string;
  description?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Style the confirm button as a destructive action. */
  destructive?: boolean;
}

interface ConfirmRequest extends ConfirmOptions {
  id: number;
  resolve: (ok: boolean) => void;
}

interface ConfirmState {
  /** Pending questions; the host shows the first one, the rest wait their turn. */
  queue: ConfirmRequest[];
  /** Answer request `id` (`false` for cancel, Escape or closing the dialog) and drop it. */
  answer: (id: number, ok: boolean) => void;
}

let nextId = 1;

export const useConfirmStore = create<ConfirmState>((set, get) => ({
  queue: [],
  answer: (id, ok) => {
    const request = get().queue.find((r) => r.id === id);
    if (!request) return; // already answered (confirm button, then the dialog closing)
    set((s) => ({ queue: s.queue.filter((r) => r.id !== id) }));
    request.resolve(ok);
  },
}));

/**
 * Implementation behind [`confirmDialog`]. An object so tests can stub it:
 * `vi.spyOn(dialogs, 'confirm').mockResolvedValue(true)`.
 */
export const dialogs = {
  confirm(options: ConfirmOptions): Promise<boolean> {
    return new Promise((resolve) => {
      const id = nextId++;
      // One tick later: asked from a dropdown item, the closing menu would take the focus back
      // from the dialog (its confirm button has the focus).
      setTimeout(() => {
        useConfirmStore.setState((s) => ({ queue: [...s.queue, { ...options, id, resolve }] }));
      }, 0);
    });
  },
};

/**
 * Ask a yes/no question in the app's styled dialog (rendered by `ConfirmHost`). Resolves `true`
 * only when the confirm button is pressed.
 */
export function confirmDialog(options: ConfirmOptions): Promise<boolean> {
  return dialogs.confirm(options);
}
