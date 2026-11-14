import { initialForm, type ConnectionForm } from '@/features/connections/connectionForm';
import type { ParsedConnectionUri } from '@/lib/types';

/** Form fields a parsed URI fills in (identity such as group, color and name is kept). */
export function uriFormPatch(parsed: ParsedConnectionUri): Partial<ConnectionForm> {
  const f = initialForm(parsed.profile);
  return {
    kind: f.kind,
    host: f.host,
    port: f.port,
    portTouched: true,
    database: f.database,
    username: f.username,
    tlsMode: f.tlsMode,
    caCertPath: f.caCertPath,
    clientCertPath: f.clientCertPath,
    clientKeyPath: f.clientKeyPath,
    timeoutSec: f.timeoutSec,
    // Every parsed option (srv, hosts, authSource, replicaSet ...) replaces the previous ones.
    options: f.options,
  };
}

/** Default connection name after "Fill from URI": `<kind> · <db>` or `<kind> <host>:<port>`. */
export function defaultUriName(
  f: Pick<ConnectionForm, 'kind' | 'host' | 'port' | 'database'>,
): string {
  const db = f.database.trim();
  return db ? `${f.kind} · ${db}` : `${f.kind} ${f.host.trim()}:${f.port.trim()}`;
}
