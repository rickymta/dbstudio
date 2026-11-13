import type { IpcError } from '@/lib/types';

/** An SSH host key that must be trusted before the connection can go on. */
export interface HostKeyPrompt {
  host: string;
  port: number;
  keyType: string;
  fingerprint: string;
  /** A different key is already trusted for this host. */
  changed: boolean;
}

/** The prompt carried by an `sshHostKey` error, or `null` for any other error. */
export function hostKeyPromptFromError(error: IpcError): HostKeyPrompt | null {
  if (error.kind !== 'sshHostKey') return null;
  if (!error.host || !error.keyType || !error.fingerprint) return null;
  return {
    host: error.host,
    port: error.port ?? 22,
    keyType: error.keyType,
    fingerprint: error.fingerprint,
    changed: error.changed === true,
  };
}
