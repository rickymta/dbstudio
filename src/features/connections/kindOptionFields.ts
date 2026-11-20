import type { DbKind } from '@/lib/types';

/**
 * Driver options that have their own field in the connection dialog. `id` is the i18n key
 * (`connections:kindOption.<id>`), `name` the key in `ConnectionProfile.options`. A value equal
 * to the driver default is not stored.
 */
export type KindOption =
  | { id: string; name: string; type: 'text' }
  | { id: string; name: string; type: 'switch'; default: boolean }
  | {
      id: string;
      name: string;
      type: 'select';
      values: readonly string[];
      default: string;
      /** Show the values as they are (protocol names) instead of translating them. */
      rawLabels?: boolean;
    };

// crates/drivers/cassandra/src/conn.rs (`Settings::from_config`, `parse_consistency`).
const CQL: KindOption[] = [
  { id: 'localDc', name: 'cql.localDc', type: 'text' },
  {
    id: 'consistency',
    name: 'cql.consistency',
    type: 'select',
    values: [
      'LOCAL_ONE',
      'ONE',
      'TWO',
      'THREE',
      'LOCAL_QUORUM',
      'QUORUM',
      'EACH_QUORUM',
      'ALL',
      'ANY',
      'LOCAL_SERIAL',
      'SERIAL',
    ],
    default: 'LOCAL_ONE',
    rawLabels: true,
  },
  { id: 'lwtEdits', name: 'cql.lwtEdits', type: 'switch', default: true },
  { id: 'singleNode', name: 'cql.singleNode', type: 'switch', default: false },
  { id: 'showSystem', name: 'cql.showSystem', type: 'switch', default: false },
];

// crates/drivers/clickhouse/src/http.rs (`targets`, `build_client`) and lib.rs (`editing`).
const CLICKHOUSE: KindOption[] = [
  {
    id: 'secure',
    name: 'clickhouse.secure',
    type: 'select',
    values: ['auto', 'true', 'false'],
    default: 'auto',
  },
  {
    id: 'compression',
    name: 'clickhouse.compression',
    type: 'select',
    values: ['lz4', 'none'],
    default: 'lz4',
  },
  {
    id: 'editing',
    name: 'clickhouse.editing',
    type: 'select',
    values: ['full', 'insertOnly'],
    default: 'full',
  },
];

// crates/drivers/postgres/src/conn.rs (CockroachDB Cloud routing id).
const COCKROACH: KindOption[] = [{ id: 'cluster', name: 'cockroach.cluster', type: 'text' }];

const BY_KIND: Partial<Record<DbKind, KindOption[]>> = {
  cassandra: CQL,
  scylladb: CQL,
  clickhouse: CLICKHOUSE,
  cockroachdb: COCKROACH,
};

/** Prefixes of options that only one driver family understands. */
const OWNED_PREFIXES: Record<string, readonly DbKind[]> = {
  'cql.': ['cassandra', 'scylladb'],
  'clickhouse.': ['clickhouse'],
  'cockroach.': ['cockroachdb'],
};

export function kindOptionsFor(kind: DbKind): KindOption[] {
  return BY_KIND[kind] ?? [];
}

/**
 * Options kept when the database kind changes: the ones owned by another driver are dropped (the
 * PostgreSQL driver, for one, would send `cql.localDc` to the server as a startup parameter).
 */
export function optionsForKind(
  options: Record<string, string>,
  kind: DbKind,
): Record<string, string> {
  const kept = Object.entries(options).filter(([key]) => {
    const owner = Object.entries(OWNED_PREFIXES).find(([prefix]) => key.startsWith(prefix));
    return !owner || owner[1].includes(kind);
  });
  return kept.length === Object.keys(options).length ? options : Object.fromEntries(kept);
}

const isTrue = (v: string) => ['true', '1', 'yes', 'on'].includes(v.trim().toLowerCase());

/** Value shown by the field of `option`. */
export function readKindOption(
  options: Record<string, string>,
  option: KindOption,
): string | boolean {
  const stored = options[option.name];
  if (option.type === 'switch') return stored === undefined ? option.default : isTrue(stored);
  if (option.type === 'text') return stored ?? '';
  if (stored === undefined) return option.default;
  const known = option.values.find((v) => v.toLowerCase() === stored.trim().toLowerCase());
  return known ?? stored;
}

/** `options` with the field of `option` set to `value`; the driver default removes the key. */
export function writeKindOption(
  options: Record<string, string>,
  option: KindOption,
  value: string | boolean,
): Record<string, string> {
  const next = { ...options };
  const isDefault = option.type === 'text' ? String(value).trim() === '' : value === option.default;
  if (isDefault) delete next[option.name];
  else next[option.name] = String(value);
  return next;
}
