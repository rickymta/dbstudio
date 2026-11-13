// Contract test between the Rust types and these TypeScript mirrors.
//
// The Rust side (`crates/dbstudio-core/tests/contract.rs`, `UPDATE_FIXTURES=1`) writes
// `contract/fixtures/<group>.json`: an object mapping a type name to a sample JSON value.
// Groups: value, error, script, lease, edit, keyvalue, plan, model, job, ssh, capabilities,
// driver, query, export, backup, history. Type names are the Rust type names (`LeaseState`,
// `ChangeSet`, `JobEvent`, ...); in the `value` group the keys are free (every sample must be a
// `Value`), in the `error` group every sample must be a serialized `Error`. Names without a
// guard below are only checked to be loadable JSON, so new samples never break this test.
// Without the fixtures directory the fixture tests are skipped.

import { describe, expect, it } from 'vitest';
import { NO_CAPABILITIES, type IpcErrorKind } from './core';

type Json = unknown;
type Obj = Record<string, unknown>;
/** Returns `null` when `v` has the right shape, else a description of the problem. */
type Guard = (v: Json) => string | null;

const isObj = (v: Json): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
const str: Guard = (v) => (typeof v === 'string' ? null : `expected string, got ${typeof v}`);
const num: Guard = (v) => (typeof v === 'number' ? null : `expected number, got ${typeof v}`);
const bool: Guard = (v) => (typeof v === 'boolean' ? null : `expected boolean, got ${typeof v}`);
const arrayOf =
  (g: Guard): Guard =>
  (v) => {
    if (!Array.isArray(v)) return 'expected array';
    for (const [i, x] of v.entries()) {
      const e = g(x);
      if (e) return `[${i}]: ${e}`;
    }
    return null;
  };
const oneOf =
  (...values: string[]): Guard =>
  (v) =>
    typeof v === 'string' && values.includes(v) ? null : `expected one of ${values.join('|')}`;

/** Object whose listed fields satisfy their guards; optional ones may be absent or null. */
const shape =
  (required: Record<string, Guard>, optional: Record<string, Guard> = {}): Guard =>
  (v) => {
    if (!isObj(v)) return 'expected object';
    for (const [k, g] of Object.entries(required)) {
      if (!(k in v)) return `missing field ${k}`;
      const e = g(v[k]);
      if (e) return `${k}: ${e}`;
    }
    for (const [k, g] of Object.entries(optional)) {
      if (v[k] === undefined || v[k] === null) continue;
      const e = g(v[k]);
      if (e) return `${k}: ${e}`;
    }
    return null;
  };

/** Tagged object: `tag` field selects one of the variants. */
const tagged =
  (tag: string, variants: Record<string, Guard>): Guard =>
  (v) => {
    if (!isObj(v)) return 'expected object';
    const t = v[tag];
    if (typeof t !== 'string') return `missing tag ${tag}`;
    const g = variants[t];
    if (!g) return `unknown ${tag} "${t}"`;
    const e = g(v);
    return e ? `${t}: ${e}` : null;
  };

// ---------------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------------

const intLike: Guard = (v) =>
  typeof v === 'number' || typeof v === 'string' ? null : 'expected number or decimal string';

export const isValue: Guard = (v) => {
  if (!isObj(v) || typeof v.t !== 'string') return 'expected {t, v}';
  switch (v.t) {
    case 'null':
      return null;
    case 'bool':
      return bool(v.v);
    case 'int':
    case 'uInt':
    case 'float':
      return intLike(v.v);
    case 'decimal':
    case 'text':
    case 'bytes':
    case 'date':
    case 'time':
    case 'dateTime':
    case 'timestamp':
    case 'uuid':
    case 'json':
      return str(v.v);
    case 'array':
      return arrayOf(isValue)(v.v);
    case 'document':
      // ordered [name, value] pairs, never an object
      return arrayOf((p) => {
        if (!Array.isArray(p) || p.length !== 2) return 'expected [name, value] pair';
        return str(p[0]) ?? isValue(p[1]);
      })(v.v);
    case 'ext':
      return shape({ type: str, text: str })(v.v);
    default:
      return `unknown value tag ${v.t}`;
  }
};

const ERROR_EXTRAS: Partial<Record<IpcErrorKind, Guard>> = {
  confirmationRequired: shape({
    confirmation: shape({
      token: str,
      confirmText: str,
      statements: arrayOf(
        shape({ index: num, range: shape({ start: num, end: num }), text: str, reason: str }),
      ),
      expiresInMs: num,
    }),
  }),
  sshHostKey: shape({ host: str, port: num, keyType: str, fingerprint: str, changed: bool }),
  changeFailed: shape({ index: num, applied: num }),
};
const ERROR_KINDS_ALL: IpcErrorKind[] = [
  'connection',
  'authentication',
  'query',
  'unsupported',
  'cancelled',
  'timeout',
  'tls',
  'notFound',
  'storage',
  'io',
  'other',
  'readOnly',
  'confirmationRequired',
  'busy',
  'permission',
  'driverUnavailable',
  'sshHostKey',
  'integrity',
  'changeFailed',
];

export const isIpcError: Guard = (v) => {
  const base = shape({ kind: oneOf(...ERROR_KINDS_ALL), message: str })(v);
  if (base) return base;
  const o = v as Obj;
  return ERROR_EXTRAS[o.kind as IpcErrorKind]?.(v) ?? null;
};

const textRange = shape({ start: num, end: num });
const scriptDialect: Guard = tagged('name', {
  postgres: () => null,
  mySql: shape({ mariadb: bool, backslashEscapes: bool }),
  msSql: () => null,
  oracle: () => null,
  mongoShell: () => null,
  redisCli: () => null,
  opaque: () => null,
});
const statementClass: Guard = tagged('kind', {
  read: () => null,
  write: () => null,
  ddl: () => null,
  transaction: shape({ op: str }),
  session: () => null,
  procedural: () => null,
  explain: shape({ analyze: bool, writes: bool }),
  admin: () => null,
  empty: () => null,
  unknown: () => null,
});
export const isScriptAnalysis: Guard = shape({
  dialect: scriptDialect,
  statements: arrayOf(
    shape(
      {
        index: num,
        range: textRange,
        class: statementClass,
        verb: str,
        implicitCommit: bool,
        targets: arrayOf(str),
      },
      { destructive: str },
    ),
  ),
  params: arrayOf(
    shape({
      name: str,
      style: oneOf('named', 'dollar', 'question', 'at'),
      ranges: arrayOf(textRange),
    }),
  ),
  problems: arrayOf(shape({ range: textRange, message: str })),
});

export const isLeaseState: Guard = shape(
  {
    autoCommit: bool,
    transaction: oneOf('idle', 'active', 'failed', 'unknown'),
    broken: bool,
  },
  { running: str, pendingResult: str, database: str, schema: str },
);

const columnMeta = shape({ name: str, typeName: str, valueKind: str }, { nullable: bool });
export const isQueryEvent: Guard = tagged('type', {
  columns: shape({ columns: arrayOf(columnMeta) }),
  rows: shape({ rows: arrayOf(arrayOf(isValue)) }),
  message: shape({ message: str }),
  statementStart: shape({ start: shape({ index: num, range: textRange }) }),
  statementEnd: shape({
    end: shape({
      outcome: tagged('kind', {
        rows: shape({ rowCount: num, truncated: bool }),
        affected: shape({ rows: num }),
        done: () => null,
      }),
      elapsedMs: num,
    }),
  }),
});

const objectPath = shape({ segments: arrayOf(shape({ kind: str, name: str })) });
const docInput: Guard = tagged('kind', {
  value: shape({ value: isValue }),
  ejson: shape({ text: str }),
});
const editValue: Guard = tagged('kind', {
  value: shape({ v: isValue }),
  text: shape({ v: str }),
  default: () => null,
});
const cellEdit = shape({ column: str, value: editValue });
export const isChangeSet: Guard = tagged('model', {
  rows: shape({
    table: objectPath,
    keyColumns: arrayOf(str),
    changes: arrayOf(
      tagged('op', {
        update: shape({ key: arrayOf(isValue), set: arrayOf(cellEdit) }),
        insert: shape({ values: arrayOf(cellEdit) }),
        delete: shape({ key: arrayOf(isValue) }),
      }),
    ),
  }),
  documents: shape({
    collection: objectPath,
    changes: arrayOf(
      tagged('op', {
        insert: shape({ document: docInput }),
        replace: shape({ id: isValue, document: docInput }),
        update: shape({ id: isValue, set: arrayOf(shape({ path: arrayOf(str), value: isValue })) }),
        delete: shape({ id: isValue }),
      }),
    ),
  }),
  keys: shape({
    database: objectPath,
    changes: arrayOf(
      tagged('op', {
        setString: shape({ key: str, value: isValue }),
        hashSet: shape({ key: str }),
        hashDelete: shape({ key: str }),
        listSet: shape({ key: str, index: num, value: isValue }),
        listPush: shape({ key: str, values: arrayOf(isValue), head: bool }),
        listRemoveAt: shape({ key: str, index: num }),
        setAdd: shape({ key: str }),
        setRemove: shape({ key: str }),
        zAdd: shape({ key: str }),
        zRemove: shape({ key: str }),
        streamAdd: shape({ key: str }),
        streamDelete: shape({ key: str, ids: arrayOf(str) }),
        expire: shape({ key: str }),
        rename: shape({ key: str, newKey: str }),
        delete: shape({ keys: arrayOf(str) }),
      }),
    ),
  }),
});

const jobProgress = shape(
  {
    phase: str,
    objectsDone: num,
    rowsDone: num,
    bytesDone: num,
  },
  { object: str, objectsTotal: num, rowsTotal: num, bytesTotal: num, message: str },
);
const jobInfo = shape(
  {
    id: str,
    kind: oneOf('export', 'import', 'sqlScript', 'backup', 'restore', 'verify', 'keyDelete'),
    title: str,
    status: oneOf('queued', 'running', 'succeeded', 'failed', 'cancelled'),
    progress: jobProgress,
    createdAt: str,
    cancellable: bool,
  },
  { connectionId: str, startedAt: str, finishedAt: str },
);
export const isJobEvent: Guard = tagged('type', {
  started: shape({ job: jobInfo }),
  progress: shape({ jobId: str, progress: jobProgress }),
  finished: shape({ job: jobInfo }),
});

export const isCapabilities: Guard = (v) => {
  if (!isObj(v)) return 'expected object';
  for (const key of Object.keys(NO_CAPABILITIES)) {
    if (typeof v[key] !== 'boolean') return `capability ${key} missing or not boolean`;
  }
  return null;
};

export const isDriverStatus: Guard = tagged('state', {
  available: () => null,
  planned: shape({ phase: num }),
  unavailable: shape({ reason: oneOf('nativeClientMissing', 'nativeClientTooOld') }),
});

const sshConfig = shape(
  {
    host: str,
    port: num,
    username: str,
    auth: tagged('method', {
      password: () => null,
      privateKey: shape({ keyPath: str, hasPassphrase: bool }),
      agent: () => null,
    }),
  },
  { keepaliveSecs: num, connectTimeoutMs: num },
);

/** Guards by group and Rust type name. */
const GUARDS: Record<string, Record<string, Guard>> = {
  script: { ScriptAnalysis: isScriptAnalysis, ScriptDialect: scriptDialect },
  lease: { LeaseState: isLeaseState },
  edit: { ChangeSet: isChangeSet },
  job: { JobEvent: isJobEvent, JobInfo: jobInfo },
  ssh: { SshConfig: sshConfig },
  capabilities: { Capabilities: isCapabilities },
  driver: { DriverStatus: isDriverStatus },
  query: { QueryEvent: isQueryEvent },
};

/** Groups whose every sample must satisfy one guard regardless of its name. */
const WHOLE_GROUP: Record<string, Guard> = { value: isValue, error: isIpcError };

export function checkFixtureGroup(group: string, samples: Obj): string[] {
  const problems: string[] = [];
  for (const [name, sample] of Object.entries(samples)) {
    const guard = WHOLE_GROUP[group] ?? GUARDS[group]?.[name];
    const problem = guard?.(sample);
    if (problem) problems.push(`${group}.${name}: ${problem}`);
  }
  return problems;
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

describe('contract guards (hand-written samples)', () => {
  it('accepts every Value variant including document pairs and ext', () => {
    const samples: Json[] = [
      { t: 'null' },
      { t: 'int', v: '9007199254740993' },
      { t: 'float', v: 'NaN' },
      { t: 'array', v: [{ t: 'bool', v: true }] },
      { t: 'document', v: [['a', { t: 'int', v: 1 }]] },
      { t: 'ext', v: { type: 'objectId', text: '65f0a1b2c3d4e5f6a7b8c9d0' } },
    ];
    for (const s of samples) expect(isValue(s)).toBeNull();
  });

  it('rejects the old object form of documents and malformed ext', () => {
    expect(isValue({ t: 'document', v: { a: { t: 'null' } } })).not.toBeNull();
    expect(isValue({ t: 'document', v: [['a']] })).not.toBeNull();
    expect(isValue({ t: 'ext', v: 'x' })).not.toBeNull();
    expect(isValue({ t: 'nope' })).not.toBeNull();
  });

  it('checks error kinds and their extra fields', () => {
    expect(isIpcError({ kind: 'readOnly', message: 'm', position: 3 })).toBeNull();
    expect(isIpcError({ kind: 'busy', message: 'm' })).toBeNull();
    expect(isIpcError({ kind: 'weird', message: 'm' })).not.toBeNull();
    expect(
      isIpcError({
        kind: 'confirmationRequired',
        message: 'm',
        confirmation: {
          token: 't',
          confirmText: 'db',
          statements: [
            { index: 0, range: { start: 0, end: 5 }, text: 'DROP TABLE x', reason: 'drop' },
          ],
          expiresInMs: 120000,
        },
      }),
    ).toBeNull();
    expect(isIpcError({ kind: 'confirmationRequired', message: 'm' })).not.toBeNull();
    expect(
      isIpcError({
        kind: 'sshHostKey',
        message: 'm',
        host: 'h',
        port: 22,
        keyType: 'ssh-ed25519',
        fingerprint: 'SHA256:x',
        changed: false,
      }),
    ).toBeNull();
    expect(isIpcError({ kind: 'changeFailed', message: 'm', index: 1, applied: 0 })).toBeNull();
    expect(isIpcError({ kind: 'changeFailed', message: 'm' })).not.toBeNull();
  });

  it('checks query events, script analysis, lease state, change sets and job events', () => {
    expect(
      isQueryEvent({
        type: 'statementStart',
        start: { index: 0, range: { start: 0, end: 17 } },
      }),
    ).toBeNull();
    expect(isQueryEvent({ type: 'statementStart', start: { index: 0 } })).not.toBeNull();
    expect(
      isScriptAnalysis({
        dialect: { name: 'mySql', mariadb: false, backslashEscapes: true },
        statements: [
          {
            index: 0,
            range: { start: 0, end: 8 },
            class: { kind: 'explain', analyze: true, writes: false },
            verb: 'EXPLAIN',
            destructive: null,
            implicitCommit: false,
            targets: [],
          },
        ],
        params: [{ name: ':id', style: 'named', ranges: [{ start: 1, end: 4 }] }],
        problems: [],
      }),
    ).toBeNull();
    expect(
      isLeaseState({
        autoCommit: true,
        transaction: 'idle',
        running: null,
        pendingResult: null,
        database: 'app',
        schema: null,
        broken: false,
      }),
    ).toBeNull();
    expect(
      isChangeSet({
        model: 'rows',
        table: { segments: [{ kind: 'table', name: 't' }] },
        keyColumns: ['id'],
        changes: [
          {
            op: 'update',
            key: [{ t: 'int', v: 1 }],
            set: [{ column: 'n', value: { kind: 'text', v: 'x' } }],
          },
          { op: 'delete', key: [{ t: 'int', v: 2 }] },
        ],
      }),
    ).toBeNull();
    expect(isChangeSet({ model: 'rows', table: {}, keyColumns: [], changes: [] })).not.toBeNull();
    const progress = {
      phase: 'data',
      object: 'public.orders',
      objectsDone: 3,
      objectsTotal: 12,
      rowsDone: 10,
      rowsTotal: null,
      bytesDone: 0,
      bytesTotal: null,
      message: null,
    };
    expect(isJobEvent({ type: 'progress', jobId: 'j', progress })).toBeNull();
    expect(isJobEvent({ type: 'progress', jobId: 'j' })).not.toBeNull();
  });

  it('knows every capability flag', () => {
    expect(isCapabilities(NO_CAPABILITIES)).toBeNull();
    expect(isCapabilities({ ...NO_CAPABILITIES, leases: undefined })).not.toBeNull();
    expect(isDriverStatus({ state: 'unavailable', reason: 'nativeClientMissing' })).toBeNull();
    expect(isDriverStatus({ state: 'unavailable', reason: 'x' })).not.toBeNull();
  });
});

// Vite resolves the glob at transform time; the folder may not exist yet (then it is empty).
const fixtureFiles = import.meta.glob<Obj>('/contract/fixtures/*.json', {
  eager: true,
  import: 'default',
});
const groups = Object.entries(fixtureFiles).map(
  ([file, samples]) => [/\/([^/]+)\.json$/.exec(file)![1]!, samples] as const,
);

describe.skipIf(groups.length === 0)('contract fixtures from Rust', () => {
  it.each(groups)('%s fixtures match the TypeScript shapes', (group, samples) => {
    expect(isObj(samples)).toBe(true);
    expect(checkFixtureGroup(group, samples)).toEqual([]);
  });
});
