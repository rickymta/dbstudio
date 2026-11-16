import { useCallback, useState, type ReactNode } from 'react';
import { ipc } from '@/lib/ipc';
import type { IpcError, SessionInfo, Uuid } from '@/lib/types';
import { isPasswordRequired } from '@/stores/sessionRecovery';
import { useSessionsStore } from '@/stores/sessions';
import { needsPassword } from './connectionsTree';
import { PasswordDialog } from './PasswordDialog';
import { useHostKeyPrompt } from './useHostKeyPrompt';

interface Asking {
  name: string;
  resolve: (password: string) => void;
  reject: () => void;
}

const cancelled = (): IpcError => ({ kind: 'cancelled', message: 'connection cancelled' });

/**
 * Open the session of a connection with the prompts of the connections list: the password of a
 * profile that stores none (asked before opening, and again when the server says it is
 * required) and the SSH host key. Render `dialog` next to the component that uses `open`.
 * Declining a prompt rejects with `cancelled`. The password is only passed on, never kept.
 */
export function useSessionOpen(): {
  open: (connectionId: Uuid) => Promise<SessionInfo>;
  dialog: ReactNode;
} {
  const { run: runWithHostKey, dialog: hostKeyDialog } = useHostKeyPrompt();
  const [asking, setAsking] = useState<Asking | null>(null);

  const ask = useCallback(
    (name: string) =>
      new Promise<string>((resolve, reject) =>
        setAsking({ name, resolve, reject: () => reject(cancelled()) }),
      ),
    [],
  );

  const open = useCallback(
    async (connectionId: Uuid): Promise<SessionInfo> => {
      const existing = useSessionsStore.getState().sessionsByConnection[connectionId];
      if (existing) return existing;
      const snapshot = await ipc.connectionsList().catch(() => null);
      const profile = snapshot?.profiles.find((p) => p.id === connectionId);
      let password: string | null = null;
      if (profile && needsPassword(profile)) password = await ask(profile.name);
      for (;;) {
        try {
          return await runWithHostKey(() =>
            useSessionsStore.getState().open(connectionId, password),
          );
        } catch (e) {
          if (!isPasswordRequired(e)) throw e;
          password = await ask(profile?.name ?? '');
        }
      }
    },
    [ask, runWithHostKey],
  );

  const dialog = (
    <>
      {hostKeyDialog}
      <PasswordDialog
        open={asking !== null}
        onOpenChange={(isOpen) => {
          if (!isOpen) {
            // Submitting resolves first (and clears `asking`); closing otherwise declines.
            setAsking((cur) => {
              cur?.reject();
              return null;
            });
          }
        }}
        connectionName={asking?.name ?? ''}
        onSubmit={(password) =>
          setAsking((cur) => {
            cur?.resolve(password);
            return null;
          })
        }
      />
    </>
  );
  return { open, dialog };
}
