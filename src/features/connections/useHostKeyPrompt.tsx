import { useCallback, useState, type ReactNode } from 'react';
import { toIpcError } from '@/lib/ipc';
import { HostKeyDialog } from './HostKeyDialog';
import { hostKeyPromptFromError, type HostKeyPrompt } from './hostKeyPrompt';

interface Pending extends HostKeyPrompt {
  resolve: () => void;
  reject: () => void;
}

/**
 * Runs an operation that may fail with `sshHostKey`. The failure opens [`HostKeyDialog`]; once
 * the user trusts the key the operation runs again, otherwise it fails with the original error.
 * Render `dialog` next to the component that uses `run`.
 */
export function useHostKeyPrompt(): {
  run: <T>(fn: () => Promise<T>) => Promise<T>;
  dialog: ReactNode;
} {
  const [pending, setPending] = useState<Pending | null>(null);

  const run = useCallback(async <T,>(fn: () => Promise<T>): Promise<T> => {
    for (;;) {
      try {
        return await fn();
      } catch (e) {
        const prompt = hostKeyPromptFromError(toIpcError(e));
        if (!prompt) throw e;
        await new Promise<void>((resolve, reject) => {
          setPending({ ...prompt, resolve, reject: () => reject(e) });
        });
      }
    }
  }, []);

  const dialog = (
    <HostKeyDialog
      prompt={pending}
      onCancel={() => {
        pending?.reject();
        setPending(null);
      }}
      onTrusted={() => {
        pending?.resolve();
        setPending(null);
      }}
    />
  );
  return { run, dialog };
}
