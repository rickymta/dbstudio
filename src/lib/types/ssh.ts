// SSH tunnel configuration (dbstudio-core `ssh.rs`).

export type SshAuth =
  | { method: 'password' }
  | { method: 'privateKey'; keyPath: string; hasPassphrase: boolean }
  | { method: 'agent' };

export interface SshConfig {
  host: string;
  /** Default 22. */
  port: number;
  username: string;
  auth: SshAuth;
  keepaliveSecs?: number | null;
  connectTimeoutMs?: number | null;
}
