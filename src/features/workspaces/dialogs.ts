import { create } from 'zustand';

/** Which workspace dialog is open (one at a time), rendered by `WorkspaceDialogsHost`. */
export type WorkspaceDialog =
  | { type: 'new' }
  | { type: 'add'; workspaceId: string; folder: string }
  | {
      type: 'name';
      title: string;
      label: string;
      initial: string;
      submitLabel: string;
      onSubmit: (name: string) => void;
    };

interface DialogsState {
  dialog: WorkspaceDialog | null;
  open: (dialog: WorkspaceDialog) => void;
  close: () => void;
}

export const useWorkspaceDialogs = create<DialogsState>((set) => ({
  dialog: null,
  open: (dialog) => set({ dialog }),
  close: () => set({ dialog: null }),
}));

/**
 * Open a dialog. Deferred one tick: called from a dropdown item, the menu first hands the focus
 * back to its trigger as it closes, which would otherwise pull it out of the dialog.
 */
export const openWorkspaceDialog = (dialog: WorkspaceDialog) => {
  setTimeout(() => useWorkspaceDialogs.getState().open(dialog), 0);
};
