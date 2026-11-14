import { useCallback, useState, type ReactNode } from 'react';
import { ipc } from '@/lib/ipc';
import type { IpcError, SessionInfo, Uuid } from '@/lib/types';
import { isPasswordRequired, reconnectSession } from '@/stores/sessionRecovery';
import { PasswordDialog } from './PasswordDialog';
import { useHostKeyPrompt } from './useHostKeyPrompt';

interface Asking {
  name: string;
  resolve: (password: string) => void;
  reject: () => void;
}

/**
 * [`reconnectSession`] with the prompts of the connections list: the SSH host key
 * (`useHostKeyPrompt`) and the password of a profile that stores none. Render `dialog` next to
 * the component that uses `reconnect`. Declining a prompt rejects with `cancelled`.
 */
export function useSessionReconnect(): {
  reconnect: (sessionId: Uuid, connectionId: Uuid) => Promise<SessionInfo>;
  dialog: ReactNode;
} {
  const { run: runWithHostKey, dialog: hostKeyDialog } = useHostKeyPrompt();
  const [asking, setAsking] = useState<Asking | null>(null);

  const reconnect = useCallback(
    async (sessionId: Uuid, connectionId: Uuid): Promise<SessionInfo> => {
      let password: string | undefined;
      for (;;) {
        try {
          return await runWithHostKey(() =>
            reconnectSession(sessionId, connectionId, { password }),
          );
        } catch (e) {
          if (!isPasswordRequired(e)) throw e;
          const snapshot = await ipc.connectionsList().catch(() => null);
          const name = snapshot?.profiles.find((p) => p.id === connectionId)?.name ?? '';
          password = await new Promise<string>((resolve, reject) =>
            setAsking({
              name,
              resolve,
              reject: () =>
                reject({ kind: 'cancelled', message: 'connection cancelled' } satisfies IpcError),
            }),
          );
        }
      }
    },
    [runWithHostKey],
  );

  const dialog = (
    <>
      {hostKeyDialog}
      <PasswordDialog
        open={asking !== null}
        onOpenChange={(open) => {
          if (!open) {
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
  return { reconnect, dialog };
}
