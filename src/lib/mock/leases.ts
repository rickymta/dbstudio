// Mock leases and script helpers. A lease is an in-memory state object; statements are split,
// classified and "executed" against fake data so the editor can be exercised without a backend:
// multiple result sets, affected rows, server messages, errors with a position, cancellable
// sleeps, manual transactions, truncated results with `fetch_more`, read-only connections and
// destructive-statement confirmation on production connections.

import type {
  ColumnMeta,
  DestructiveReason,
  ExecuteRequest,
  ExecuteResult,
  LeaseCloseReport,
  LeaseContext,
  LeaseInfo,
  LeaseOptions,
  LeaseReopened,
  LeaseState,
  ObjectPath,
  ParamRef,
  QueryEvent,
  ScriptAnalysis,
  ScriptDialect,
  ScriptGenerated,
  ScriptProblem,
  ScriptTemplate,
  StatementClass,
  StatementInfo,
  TxOptions,
  Uuid,
  Value,
  Vocabulary,
} from '../types';
import { findMockProfile } from './connections';
import { mockRecordHistory, mockFinishHistory } from './history';
import { mockEmit } from './events';
import { mockSessionConnectionId, mockSessionDialect } from './sessions';
import { clone, ipcError, newId, sleep } from './util';

interface Pending {
  queryId: Uuid;
  total: number;
  offset: number;
  row: (i: number) => Value[];
}

interface MockLease {
  sessionId: Uuid;
  state: LeaseState;
  /** Set by `lease_cancel` for the running query. */
  cancelled: Set<Uuid>;
  pending: Pending | null;
}

const leases = new Map<Uuid, MockLease>();
const tokens = new Map<string, { confirmText: string; text: string }>();

function find(leaseId: Uuid): MockLease {
  const lease = leases.get(leaseId);
  if (!lease) throw ipcError('notFound', `lease ${leaseId}`);
  return lease;
}

// -- script analysis ---------------------------------------------------------------------------

interface RawStatement {
  start: number;
  end: number;
  text: string;
}

/** Split on `;` outside strings, quoted identifiers and comments. Offsets are scalar indexes. */
function splitStatements(chars: string[]): {
  statements: RawStatement[];
  problems: ScriptProblem[];
} {
  const statements: RawStatement[] = [];
  const problems: ScriptProblem[] = [];
  const n = chars.length;
  let start = 0;
  let i = 0;
  const push = (end: number) => {
    let s = start;
    let e = end;
    while (s < e && /\s/.test(chars[s]!)) s++;
    while (e > s && /\s/.test(chars[e - 1]!)) e--;
    if (e > s) statements.push({ start: s, end: e, text: chars.slice(s, e).join('') });
  };
  while (i < n) {
    const c = chars[i]!;
    if (c === '-' && chars[i + 1] === '-') {
      while (i < n && chars[i] !== '\n') i++;
    } else if (c === '/' && chars[i + 1] === '*') {
      const from = i;
      i += 2;
      while (i < n && !(chars[i] === '*' && chars[i + 1] === '/')) i++;
      if (i >= n)
        problems.push({ range: { start: from, end: n }, message: 'unterminated comment' });
      i += 2;
    } else if (c === "'" || c === '"' || c === '`') {
      const from = i;
      i++;
      let closed = false;
      while (i < n) {
        if (chars[i] === c) {
          if (chars[i + 1] === c) {
            i += 2;
            continue;
          }
          closed = true;
          i++;
          break;
        }
        i++;
      }
      if (!closed) {
        problems.push({
          range: { start: from, end: n },
          message: c === "'" ? 'unterminated string' : 'unterminated quoted identifier',
        });
      }
    } else if (c === ';') {
      push(i);
      i++;
      start = i;
    } else {
      i++;
    }
  }
  push(n);
  return { statements, problems };
}

/** Parameter references (`:name`, `$1`, `?`, `@p`) outside strings and comments. */
function findParams(chars: string[]): ParamRef[] {
  const byName = new Map<string, ParamRef>();
  let questions = 0;
  const add = (name: string, style: ParamRef['style'], start: number, end: number) => {
    const ref = byName.get(name) ?? { name, style, ranges: [] };
    ref.ranges.push({ start, end });
    byName.set(name, ref);
  };
  const n = chars.length;
  let i = 0;
  const ident = /[A-Za-z0-9_]/;
  while (i < n) {
    const c = chars[i]!;
    if (c === '-' && chars[i + 1] === '-') {
      while (i < n && chars[i] !== '\n') i++;
    } else if (c === '/' && chars[i + 1] === '*') {
      i += 2;
      while (i < n && !(chars[i] === '*' && chars[i + 1] === '/')) i++;
      i += 2;
    } else if (c === "'" || c === '"' || c === '`') {
      i++;
      while (i < n && chars[i] !== c) i++;
      i++;
    } else if (
      c === ':' &&
      chars[i + 1] !== ':' &&
      chars[i - 1] !== ':' &&
      /[A-Za-z_]/.test(chars[i + 1] ?? '')
    ) {
      let j = i + 1;
      while (j < n && ident.test(chars[j]!)) j++;
      add(chars.slice(i, j).join(''), 'named', i, j);
      i = j;
    } else if (c === '$' && /\d/.test(chars[i + 1] ?? '')) {
      let j = i + 1;
      while (j < n && /\d/.test(chars[j]!)) j++;
      add(chars.slice(i, j).join(''), 'dollar', i, j);
      i = j;
    } else if (c === '?') {
      questions++;
      add(`?${questions}`, 'question', i, i + 1);
      i++;
    } else if (
      c === '@' &&
      chars[i + 1] !== '@' &&
      chars[i - 1] !== '@' &&
      /[A-Za-z_]/.test(chars[i + 1] ?? '')
    ) {
      let j = i + 1;
      while (j < n && ident.test(chars[j]!)) j++;
      add(chars.slice(i, j).join(''), 'at', i, j);
      i = j;
    } else {
      i++;
    }
  }
  return [...byName.values()];
}

const firstWord = (text: string): string =>
  /^[\s(]*([A-Za-z_]+)/.exec(text)?.[1]?.toUpperCase() ?? '';
const hasWhere = (text: string): boolean => /\bWHERE\b/i.test(text);

function classify(
  dialect: ScriptDialect,
  text: string,
): { class: StatementClass; verb: string; destructive: DestructiveReason | null } {
  const word = firstWord(text);
  if (dialect.name === 'redisCli') {
    const cmd = text.trim().split(/\s+/)[0]?.toUpperCase() ?? '';
    if (cmd === 'FLUSHDB') return { class: { kind: 'admin' }, verb: cmd, destructive: 'flushDb' };
    if (cmd === 'FLUSHALL') return { class: { kind: 'admin' }, verb: cmd, destructive: 'flushAll' };
    const read = [
      'GET',
      'MGET',
      'KEYS',
      'SCAN',
      'TTL',
      'TYPE',
      'EXISTS',
      'PING',
      'HGETALL',
      'LRANGE',
    ];
    return { class: { kind: read.includes(cmd) ? 'read' : 'write' }, verb: cmd, destructive: null };
  }
  if (dialect.name === 'mongoShell') {
    const read = /\.(find|findOne|aggregate|countDocuments|distinct)\s*\(/.test(text);
    const drop = /\.drop(Database|Collection)?\s*\(/.test(text);
    return {
      class: { kind: drop ? 'ddl' : read ? 'read' : 'write' },
      verb: text.trim().split('(')[0] ?? '',
      destructive: drop ? 'dropCollection' : null,
    };
  }
  switch (word) {
    case 'SELECT':
    case 'SHOW':
    case 'WITH':
    case 'VALUES':
    case 'TABLE':
    case 'DESCRIBE':
      return { class: { kind: 'read' }, verb: word, destructive: null };
    case 'EXPLAIN':
      return {
        class: { kind: 'explain', analyze: /\bANALYZE\b/i.test(text), writes: false },
        verb: word,
        destructive: null,
      };
    case 'INSERT':
    case 'MERGE':
    case 'REPLACE':
      return { class: { kind: 'write' }, verb: word, destructive: null };
    case 'UPDATE':
      return {
        class: { kind: 'write' },
        verb: word,
        destructive: hasWhere(text) ? null : 'updateWithoutWhere',
      };
    case 'DELETE':
      return {
        class: { kind: 'write' },
        verb: word,
        destructive: hasWhere(text) ? null : 'deleteWithoutWhere',
      };
    case 'CREATE':
    case 'ALTER':
      return { class: { kind: 'ddl' }, verb: word, destructive: null };
    case 'DROP':
      return { class: { kind: 'ddl' }, verb: word, destructive: 'drop' };
    case 'TRUNCATE':
      return { class: { kind: 'ddl' }, verb: word, destructive: 'truncate' };
    case 'BEGIN':
    case 'START':
      return { class: { kind: 'transaction', op: 'begin' }, verb: word, destructive: null };
    case 'COMMIT':
    case 'END':
      return { class: { kind: 'transaction', op: 'commit' }, verb: word, destructive: null };
    case 'ROLLBACK':
      return { class: { kind: 'transaction', op: 'rollback' }, verb: word, destructive: null };
    case 'SET':
    case 'USE':
      return { class: { kind: 'session' }, verb: word, destructive: null };
    case 'CALL':
    case 'EXEC':
    case 'EXECUTE':
    case 'DO':
    case 'PRINT':
      return { class: { kind: 'procedural' }, verb: word, destructive: null };
    case 'GRANT':
    case 'REVOKE':
      return { class: { kind: 'admin' }, verb: word, destructive: null };
    default:
      return { class: { kind: 'unknown' }, verb: word, destructive: null };
  }
}

function analyze(dialect: ScriptDialect, text: string): ScriptAnalysis {
  const chars = Array.from(text);
  const { statements, problems } = splitStatements(chars);
  const infos: StatementInfo[] = statements.map((s, index) => {
    const c = classify(dialect, s.text);
    return {
      index,
      range: { start: s.start, end: s.end },
      class: problems.length > 0 ? { kind: 'unknown' } : c.class,
      verb: c.verb,
      destructive: c.destructive,
      implicitCommit: c.class.kind === 'ddl' && dialect.name === 'mySql',
      targets: [],
    };
  });
  return {
    dialect,
    statements: infos,
    params: dialect.name === 'redisCli' ? [] : findParams(chars),
    problems,
  };
}

// -- execution ---------------------------------------------------------------------------------

const USERS_COLUMNS: ColumnMeta[] = [
  { name: 'id', typeName: 'int4', valueKind: 'int', nullable: false },
  { name: 'name', typeName: 'text', valueKind: 'text', nullable: false },
  { name: 'email', typeName: 'text', valueKind: 'text', nullable: true },
];
const KNOWN_TABLES = new Set(['users', 'orders', 'products', 'customers']);

function userRow(i: number): Value[] {
  return [
    { t: 'int', v: i + 1 },
    { t: 'text', v: `User ${i + 1}` },
    i % 7 === 3 ? { t: 'null' } : { t: 'text', v: `user${i + 1}@example.com` },
  ];
}

interface Plan {
  columns: ColumnMeta[];
  total: number;
  row: (i: number) => Value[];
}

/** Shape of the result set of a read statement, or an error with a scalar position. */
function planRead(text: string, base: number, dialect: ScriptDialect): Plan {
  if (dialect.name === 'redisCli') {
    return {
      columns: [{ name: 'result', typeName: 'string', valueKind: 'text' }],
      total: 1,
      row: () => [{ t: 'text', v: 'OK' }],
    };
  }
  if (dialect.name === 'mongoShell') {
    return {
      columns: [{ name: 'document', typeName: 'document', valueKind: 'document' }],
      total: 3,
      row: (i) => [
        {
          t: 'document',
          v: [
            ['_id', { t: 'int', v: i + 1 }],
            ['name', { t: 'text', v: `doc ${i + 1}` }],
          ],
        },
      ],
    };
  }
  const bad = /\bnope\b/i.exec(text);
  if (bad) {
    throw ipcError('query', 'column "nope" does not exist', {
      position: base + Array.from(text.slice(0, bad.index)).length,
    });
  }
  const series = /generate_series\(\s*(\d+)\s*,\s*(\d+)\s*\)/i.exec(text);
  if (series) {
    const from = Number(series[1]);
    const to = Number(series[2]);
    return {
      columns: [{ name: 'n', typeName: 'int4', valueKind: 'int' }],
      total: Math.max(0, to - from + 1),
      row: (i) => [{ t: 'int', v: from + i }],
    };
  }
  const table = /\bFROM\s+(?:[\w"]+\.)?"?(\w+)"?/i.exec(text);
  if (table) {
    const name = table[1]!;
    if (!KNOWN_TABLES.has(name.toLowerCase())) {
      throw ipcError('query', `relation "${name}" does not exist`, {
        position:
          base + Array.from(text.slice(0, table.index + table[0].length - name.length)).length,
      });
    }
    return { columns: USERS_COLUMNS, total: 2500, row: userRow };
  }
  return {
    columns: [{ name: '?column?', typeName: 'int4', valueKind: 'int' }],
    total: 1,
    row: () => [{ t: 'int', v: 1 }],
  };
}

function profileOfSession(sessionId: Uuid) {
  const connectionId = mockSessionConnectionId(sessionId);
  return connectionId ? findMockProfile(connectionId) : undefined;
}

const emptyResult = (
  statements: number,
  state: LeaseState | null,
  historyId: number | null,
): ExecuteResult => ({
  summary: { statements, elapsedMs: 0 },
  state: state ? clone(state) : null,
  historyId,
});

async function waitCancellable(ms: number, lease: MockLease | null, queryId: Uuid): Promise<void> {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (lease?.cancelled.has(queryId)) {
      lease.cancelled.delete(queryId);
      throw ipcError('cancelled', 'canceling statement due to user request');
    }
    await new Promise((r) => setTimeout(r, 10));
  }
}

/** Guard checks of `src-tauri/src/guard.rs`: read-only, then production confirmation. */
function guard(
  request: ExecuteRequest,
  analysis: ScriptAnalysis,
  profile: ReturnType<typeof profileOfSession>,
  confirmText: string,
): void {
  if (profile?.readOnly) {
    const bad = analysis.statements.find((s) => {
      const k = s.class.kind;
      if (k === 'explain') return (s.class as { writes: boolean }).writes;
      return !['read', 'transaction', 'session', 'empty'].includes(k);
    });
    if (bad) {
      throw ipcError('readOnly', 'the connection is read-only', { position: bad.range.start });
    }
  }
  const destructive = analysis.statements.filter((s) => s.destructive);
  if (profile?.environment === 'production' && destructive.length > 0) {
    const grant = request.confirm;
    const entry = grant ? tokens.get(grant.token) : undefined;
    if (grant && entry && entry.text === request.text && grant.typed === entry.confirmText) {
      tokens.delete(grant.token); // single use
      return;
    }
    if (grant) tokens.delete(grant.token);
    const token = newId().replaceAll('-', '');
    tokens.set(token, { confirmText, text: request.text });
    throw ipcError('confirmationRequired', 'confirmation required for destructive statements', {
      confirmation: {
        token,
        confirmText,
        expiresInMs: 60_000,
        statements: destructive.map((s) => ({
          index: s.index,
          range: s.range,
          text: Array.from(request.text).slice(s.range.start, s.range.end).join('').slice(0, 2000),
          reason: s.destructive!,
        })),
      },
    });
  }
}

async function run(
  sessionId: Uuid,
  lease: MockLease | null,
  request: ExecuteRequest,
  onEvent: (e: QueryEvent) => void,
): Promise<ExecuteResult> {
  const dialect = mockSessionDialect(sessionId);
  const profile = profileOfSession(sessionId);
  const state: LeaseState = lease?.state ?? {
    autoCommit: true,
    transaction: 'idle',
    running: null,
    pendingResult: null,
    database: null,
    schema: null,
    broken: false,
  };
  if (lease) {
    if (state.broken) throw ipcError('connection', 'the connection was lost');
    if (state.running) throw ipcError('busy', 'a statement is already running');
  }
  const analysis = analyze(dialect, request.text);

  // Parameters must all have a value.
  const given = new Set((request.params ?? []).map((p) => p.name));
  const missing = analysis.params.find((p) => !given.has(p.name));
  if (missing) throw ipcError('query', `missing value for parameter ${missing.name}`);

  const confirmText =
    state.database ?? profile?.database ?? (dialect.name === 'redisCli' ? 'db0' : 'app');
  guard(request, analysis, profile, confirmText);

  // A new execution drops the pending result of the previous one.
  if (lease) lease.pending = null;
  state.pendingResult = null;

  const history =
    request.recordHistory === false
      ? null
      : mockRecordHistory({
          connectionId: profile?.id ?? '',
          database: state.database,
          text: request.text,
          kind: analysis.statements.length > 1 ? 'script' : 'query',
        });
  const maxRows = request.maxRows ?? 1000;
  const queryId = request.queryId;
  const started = Date.now();
  state.running = queryId;
  let ran = 0;
  let rowsTotal = 0;
  try {
    for (const [pos, s] of analysis.statements.entries()) {
      const isLast = pos === analysis.statements.length - 1;
      const text = Array.from(request.text).slice(s.range.start, s.range.end).join('');
      onEvent({ type: 'statementStart', start: { index: s.index, range: s.range } });
      ran++;
      await waitCancellable(2, lease, queryId);
      const t0 = Date.now();
      const kind = s.class.kind;
      const sleepMatch = /(?:pg_sleep|sleep)\(\s*(\d+(?:\.\d+)?)\s*\)/i.exec(text);
      if (sleepMatch) await waitCancellable(Number(sleepMatch[1]) * 1000, lease, queryId);

      // Transaction bookkeeping: manual mode opens a transaction before the first statement.
      const opensTx =
        kind === 'read' || kind === 'write' || kind === 'ddl' || kind === 'procedural';
      if (!state.autoCommit && state.transaction === 'idle' && opensTx)
        state.transaction = 'active';

      try {
        if (kind === 'transaction' && s.class.kind === 'transaction') {
          state.transaction = s.class.op === 'begin' ? 'active' : 'idle';
          onEvent({
            type: 'statementEnd',
            end: { outcome: { kind: 'done' }, elapsedMs: Date.now() - t0 },
          });
        } else if (kind === 'read' || kind === 'explain') {
          const plan = planRead(text, s.range.start, dialect);
          onEvent({ type: 'columns', columns: plan.columns });
          const limit = Math.min(plan.total, maxRows);
          for (let from = 0; from < limit; from += 500) {
            const to = Math.min(limit, from + 500);
            onEvent({
              type: 'rows',
              rows: Array.from({ length: to - from }, (_, i) => plan.row(from + i)),
            });
          }
          const truncated = plan.total > limit;
          rowsTotal += limit;
          if (truncated && isLast && lease) {
            lease.pending = { queryId, total: plan.total, offset: limit, row: plan.row };
            state.pendingResult = queryId;
          }
          onEvent({
            type: 'statementEnd',
            end: {
              outcome: { kind: 'rows', rowCount: limit, truncated },
              elapsedMs: Date.now() - t0,
            },
          });
        } else if (
          kind === 'write' &&
          dialect.name !== 'redisCli' &&
          dialect.name !== 'mongoShell'
        ) {
          const rows = s.destructive ? 42 : s.verb === 'INSERT' ? 1 : 3;
          rowsTotal += rows;
          onEvent({
            type: 'statementEnd',
            end: { outcome: { kind: 'affected', rows }, elapsedMs: Date.now() - t0 },
          });
        } else {
          const notice = /(?:RAISE\s+NOTICE|PRINT)\s+'([^']*)'/i.exec(text);
          if (notice) {
            onEvent({
              type: 'message',
              message: dialect.name === 'msSql' ? notice[1]! : `NOTICE:  ${notice[1]}`,
            });
          }
          if (s.implicitCommit) state.transaction = 'idle';
          onEvent({
            type: 'statementEnd',
            end: { outcome: { kind: 'done' }, elapsedMs: Date.now() - t0 },
          });
        }
      } catch (e) {
        // PostgreSQL aborts the open transaction on the first error.
        if (dialect.name === 'postgres' && state.transaction === 'active')
          state.transaction = 'failed';
        throw e;
      }
    }
    if (history) mockFinishHistory(history, 'ok', Date.now() - started, ran, rowsTotal, null);
    return emptyResult(ran, lease ? state : null, history);
  } catch (e) {
    const err = e as { kind?: string; message?: string };
    if (history) {
      mockFinishHistory(
        history,
        err.kind === 'cancelled' ? 'cancelled' : 'error',
        Date.now() - started,
        ran,
        rowsTotal,
        err.message ?? null,
      );
    }
    throw e;
  } finally {
    state.running = null;
  }
}

function pushLeaseState(leaseId: Uuid, lease: MockLease): void {
  mockEmit('lease://state', { leaseId, state: clone(lease.state) });
}

export const leasesMock = {
  async scriptAnalyze(sessionId: Uuid, text: string): Promise<ScriptAnalysis> {
    return analyze(mockSessionDialect(sessionId), text);
  },
  async scriptVocabulary(sessionId: Uuid): Promise<Vocabulary> {
    const dialect = mockSessionDialect(sessionId);
    if (dialect.name === 'redisCli') {
      return {
        keywords: [],
        functions: [],
        commands: [
          { name: 'GET', syntax: 'GET key', summary: 'Get the value of a key', flags: [] },
          {
            name: 'SET',
            syntax: 'SET key value [EX seconds]',
            summary: 'Set a key',
            flags: ['EX', 'NX'],
          },
          { name: 'DEL', syntax: 'DEL key [key ...]', summary: 'Delete keys', flags: [] },
          {
            name: 'FLUSHDB',
            syntax: 'FLUSHDB [ASYNC]',
            summary: 'Remove all keys',
            flags: ['ASYNC'],
          },
        ],
      };
    }
    return {
      keywords: [
        'SELECT',
        'FROM',
        'WHERE',
        'GROUP BY',
        'ORDER BY',
        'HAVING',
        'LIMIT',
        'JOIN',
        'LEFT JOIN',
        'INNER JOIN',
        'ON',
        'INSERT INTO',
        'VALUES',
        'UPDATE',
        'SET',
        'DELETE FROM',
        'CREATE TABLE',
        'DROP TABLE',
        'AS',
        'AND',
        'OR',
        'NOT',
        'NULL',
        'DISTINCT',
        'BEGIN',
        'COMMIT',
        'ROLLBACK',
      ],
      functions: ['count', 'sum', 'avg', 'min', 'max', 'coalesce', 'now', 'lower', 'upper'],
      commands: [],
    };
  },
  async queryExecute(
    sessionId: Uuid,
    request: ExecuteRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> {
    return run(sessionId, null, request, onEvent);
  },
  async leaseOpen(sessionId: Uuid, options: LeaseOptions): Promise<LeaseInfo> {
    mockSessionDialect(sessionId);
    await sleep(5, 20);
    const leaseId = newId();
    const state: LeaseState = {
      autoCommit: options.autoCommit ?? true,
      transaction: 'idle',
      running: null,
      pendingResult: null,
      database: options.database ?? null,
      schema: options.schema ?? null,
      broken: false,
    };
    leases.set(leaseId, { sessionId, state, cancelled: new Set(), pending: null });
    return { leaseId, sessionId, state: clone(state) };
  },
  /** The mock connection never breaks; reopening clears `broken` and drops an open transaction. */
  async leaseReopen(leaseId: Uuid): Promise<LeaseReopened> {
    const lease = find(leaseId);
    await sleep(5, 20);
    const transactionLost =
      lease.state.transaction === 'active' || lease.state.transaction === 'failed';
    lease.state = { ...lease.state, broken: false, transaction: 'idle', running: null };
    lease.pending = null;
    pushLeaseState(leaseId, lease);
    return {
      lease: { leaseId, sessionId: lease.sessionId, state: clone(lease.state) },
      transactionLost,
    };
  },
  /** A sample template; the real backend generates it from the table's columns. */
  async scriptGenerate(
    sessionId: Uuid,
    path: ObjectPath,
    template: ScriptTemplate,
  ): Promise<ScriptGenerated> {
    const dialect = mockSessionDialect(sessionId);
    await sleep(5, 20);
    const table = path.segments.at(-1)?.name ?? 'table';
    const quote = dialect.name === 'mySql' ? '`' : '"';
    const name = `${quote}${table}${quote}`;
    const named = dialect.name !== 'msSql';
    const text = {
      select:
        dialect.name === 'msSql'
          ? `SELECT TOP 100 * FROM ${name}`
          : `SELECT * FROM ${name} LIMIT 100`,
      insert: `INSERT INTO ${name} (id) VALUES (${named ? ':id' : '@id'})`,
      update: `UPDATE ${name} SET id = ${named ? ':id' : '@id'} WHERE id = ${named ? ':id' : '@id'}`,
      delete: `DELETE FROM ${name} WHERE id = ${named ? ':id' : '@id'}`,
    }[template];
    return { text, dialect };
  },
  async leaseClose(leaseId: Uuid): Promise<LeaseCloseReport> {
    const lease = leases.get(leaseId);
    leases.delete(leaseId);
    const open = lease?.state.transaction === 'active' || lease?.state.transaction === 'failed';
    return { rolledBack: open, forced: false };
  },
  async leaseExecute(
    leaseId: Uuid,
    request: ExecuteRequest,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> {
    const lease = find(leaseId);
    const result = await run(lease.sessionId, lease, request, onEvent).catch((e: unknown) => {
      pushLeaseState(leaseId, lease);
      throw e;
    });
    return result;
  },
  async leaseFetchMore(
    leaseId: Uuid,
    queryId: Uuid,
    maxRows: number,
    batchSize: number | null,
    onEvent: (e: QueryEvent) => void,
  ): Promise<ExecuteResult> {
    void batchSize;
    const lease = find(leaseId);
    const pending = lease.pending;
    if (!pending || pending.queryId !== queryId) throw ipcError('notFound', `result ${queryId}`);
    const limit = Math.min(pending.total, pending.offset + maxRows);
    onEvent({
      type: 'rows',
      rows: Array.from({ length: limit - pending.offset }, (_, i) =>
        pending.row(pending.offset + i),
      ),
    });
    const truncated = limit < pending.total;
    const count = limit - pending.offset;
    pending.offset = limit;
    if (!truncated) {
      lease.pending = null;
      lease.state.pendingResult = null;
    }
    onEvent({
      type: 'statementEnd',
      end: { outcome: { kind: 'rows', rowCount: count, truncated }, elapsedMs: 1 },
    });
    return emptyResult(1, lease.state, null);
  },
  async leaseDiscardResult(leaseId: Uuid): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.pending = null;
    lease.state.pendingResult = null;
    return clone(lease.state);
  },
  async leaseCancel(leaseId: Uuid, queryId: Uuid): Promise<void> {
    const lease = find(leaseId);
    // Late cancels never hit the next statement: only the running one is interrupted.
    if (lease.state.running === queryId) lease.cancelled.add(queryId);
  },
  async leaseBegin(leaseId: Uuid, options: TxOptions): Promise<LeaseState> {
    void options;
    const lease = find(leaseId);
    lease.pending = null;
    lease.state.pendingResult = null;
    lease.state.transaction = 'active';
    return clone(lease.state);
  },
  async leaseCommit(leaseId: Uuid): Promise<LeaseState> {
    const lease = find(leaseId);
    if (lease.state.transaction === 'failed') {
      // PostgreSQL turns COMMIT of an aborted transaction into ROLLBACK.
      lease.state.transaction = 'idle';
      return clone(lease.state);
    }
    lease.state.transaction = 'idle';
    return clone(lease.state);
  },
  async leaseRollback(leaseId: Uuid): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.state.transaction = 'idle';
    return clone(lease.state);
  },
  async leaseSetAutoCommit(leaseId: Uuid, autoCommit: boolean): Promise<LeaseState> {
    const lease = find(leaseId);
    if (
      autoCommit &&
      (lease.state.transaction === 'active' || lease.state.transaction === 'failed')
    ) {
      throw ipcError('busy', 'a transaction is open');
    }
    lease.state.autoCommit = autoCommit;
    return clone(lease.state);
  },
  async leaseSetContext(leaseId: Uuid, context: LeaseContext): Promise<LeaseState> {
    const lease = find(leaseId);
    lease.state.database = context.database ?? lease.state.database;
    lease.state.schema = context.schema ?? lease.state.schema;
    return clone(lease.state);
  },
  async leaseState(leaseId: Uuid): Promise<LeaseState> {
    return clone(find(leaseId).state);
  },
};

/** Dev/test helper: the connection of a lease is lost (emits `lease://state`). */
/** Session behind a mock lease, or `undefined` when the lease does not exist. */
export function mockLeaseSessionId(leaseId: Uuid): Uuid | undefined {
  return leases.get(leaseId)?.sessionId;
}

export function mockBreakLease(leaseId: Uuid): void {
  const lease = leases.get(leaseId);
  if (!lease) return;
  lease.state.broken = true;
  lease.state.running = null;
  pushLeaseState(leaseId, lease);
}

export function resetMockLeases(): void {
  leases.clear();
  tokens.clear();
}
