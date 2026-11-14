import { create } from 'zustand';

export type ToastTone = 'info' | 'success' | 'error';

export interface ToastItem {
  id: number;
  tone: ToastTone;
  message: string;
  /** Optional button next to the message; the toast closes when it is pressed. */
  action?: ToastAction;
}

export interface ToastAction {
  label: string;
  onClick: () => void;
}

interface ToastState {
  toasts: ToastItem[];
  push: (tone: ToastTone, message: string, durationMs?: number, action?: ToastAction) => number;
  dismiss: (id: number) => void;
}

let nextId = 1;

export const useToastStore = create<ToastState>((set, get) => ({
  toasts: [],
  push: (tone, message, durationMs, action) => {
    const id = nextId++;
    set((s) => ({ toasts: [...s.toasts, { id, tone, message, ...(action ? { action } : {}) }] }));
    const ms = durationMs ?? (tone === 'error' ? 8000 : 4000);
    if (ms > 0) setTimeout(() => get().dismiss(id), ms);
    return id;
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));

/** Show a toast from anywhere (no hook needed). */
export const toast = {
  info: (message: string) => useToastStore.getState().push('info', message),
  success: (message: string) => useToastStore.getState().push('success', message),
  error: (message: string) => useToastStore.getState().push('error', message),
  dismiss: (id: number) => useToastStore.getState().dismiss(id),
};

/** Hook form of [`toast`]. */
export function useToast() {
  return toast;
}
