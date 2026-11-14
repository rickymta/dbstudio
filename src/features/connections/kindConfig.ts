import type { DbKind, Environment, TlsMode } from '@/lib/types';

/** i18n key suffix (`connections:field.<key>`) of the "database" field, per database kind. */
export const DATABASE_FIELD: Record<DbKind, 'database' | 'serviceName' | 'dbIndex' | 'keyspace'> = {
  postgres: 'database',
  mysql: 'database',
  sqlserver: 'database',
  oracle: 'serviceName',
  mongodb: 'database',
  redis: 'dbIndex',
  cassandra: 'keyspace',
  scylladb: 'keyspace',
  clickhouse: 'database',
  cockroachdb: 'database',
  yugabytedb: 'database',
  tidb: 'database',
};

/** Short badge shown in the sidebar for each kind. */
export const KIND_BADGE: Record<DbKind, string> = {
  postgres: 'PG',
  mysql: 'MY',
  sqlserver: 'MS',
  oracle: 'OR',
  mongodb: 'MG',
  redis: 'RD',
  cassandra: 'CA',
  scylladb: 'SC',
  clickhouse: 'CH',
  cockroachdb: 'CR',
  yugabytedb: 'YB',
  tidb: 'TI',
};

export const TLS_MODES: TlsMode[] = ['disable', 'prefer', 'require', 'verifyCa', 'verifyFull'];
/** TLS modes that validate the server certificate and so take certificate paths. */
export const TLS_VERIFYING: ReadonlySet<TlsMode> = new Set(['verifyCa', 'verifyFull']);

export const ENVIRONMENTS: Environment[] = ['development', 'staging', 'production'];

/** Profile colors are user data (CSS hex), so the palette lives here rather than in the theme. */
export const COLOR_SWATCHES = [
  { hex: '#30a46c', key: 'green' },
  { hex: '#3e63dd', key: 'blue' },
  { hex: '#8e4ec6', key: 'purple' },
  { hex: '#d6409f', key: 'pink' },
  { hex: '#e5484d', key: 'red' },
  { hex: '#f76b15', key: 'orange' },
  { hex: '#f5a524', key: 'yellow' },
  { hex: '#12a594', key: 'teal' },
] as const;

/** Color implied by an environment when the profile has no color of its own. */
export const ENVIRONMENT_COLOR: Record<Environment, string> = {
  development: 'var(--success)',
  staging: 'var(--warning)',
  production: 'var(--danger)',
};

/** Fallbacks keep the UI working when the backend reports a kind this build does not know. */
export const databaseFieldFor = (kind: DbKind) => DATABASE_FIELD[kind] ?? 'database';
export const kindBadgeFor = (kind: DbKind) =>
  KIND_BADGE[kind] ?? String(kind).slice(0, 2).toUpperCase();
