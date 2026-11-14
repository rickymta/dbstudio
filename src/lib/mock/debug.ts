// SQL debugger mock (browser mode). A full simulation of the engine described in
// docs/design/sql-debugger.md section 2: the script is split with the lease mock's analyzer,
// a state machine drives start / step / resume / pause / stop / restart / run-to-cursor / jump-to,
// breakpoints (condition, hit count, log message, mute), a fake PL/pgSQL routine pair
// (`f_outer` calls `f_inner`) for `postgres` kinds with stack frames, variables and deposit,
// read-only watches / evaluate, console output, the read-only guard, production confirmation
// tokens (single use), an owned transaction with end actions and a simulated paused TTL.
//
// Commands are applied synchronously: the `running` state is emitted as an event first, then
// output events, and the settled state is returned (the real backend returns `running`
// immediately and the settled state arrives by event). `seq` is shared by both event streams.

import { mockEmit } from './events';
import { findMockProfile } from './connections';
import { leasesMock, mockLeaseSessionId } from './leases';
import { mockSessionConnectionId, mockSessionKind } from './sessions';
import { clone, ipcError, newId } from './util';
import type {
  Breakpoint,
  BreakpointStatus,
  ColumnMeta,
  ConfirmationGrant,
  ConnectionProfile,
  DebugAction,
  DebugEndAction,
  DebugEndReport,
  DebugError,
  DebugFrame,
  DebugOptions,
  DebugOutput,
  DebugPosition,
  DebugStartRequest,
  DebugState,
  DebugSupport,
  DebugVariable,
  EvalResult,
  EvalValue,
  InlineValue,
  ParamInput,
  PauseReason,
  QueryEvent,
  ResultSet,
  RoutineBreakpoint,
  RoutineRef,
  RoutineSource,
  SourceRef,
  StatementInfo,
  TerminateReason,
  TextRange,
  Uuid,
  Value,
  WatchResult,
  WatchSpec,
} from '../types';
import { DEFAULT_DEBUG_OPTIONS } from '../types';

// -- fake routines -----------------------------------------------------------------------------

interface RoutineDef {
  name: string;
  signature: string;
  params: string[];
  locals: string[];
  /** Executable lines (1-based) of the fake source. */
  exec: number[];
  source: string;
}

const ROUTINES: RoutineDef[] = [
  {
    name: 'f_outer',
    signature: 'public.f_outer(integer)',
    params: ['p_n'],
    locals: ['v_total'],
    exec: [3, 4, 5],
    source: [
      'DECLARE v_total integer;',
      'BEGIN',
      '  v_total := p_n * 2;',
      '  v_total := v_total + f_inner(v_total);',
      '  RETURN v_total;',
      'END',
    ].join('\n'),
  },
  {
    name: 'f_inner',
    signature: 'public.f_inner(integer)',
    params: ['p_x'],
    locals: ['v_r'],
    exec: [3, 4],
    source: ['DECLARE v_r integer;', 'BEGIN', '  v_r := p_x + 1;', '  RETURN v_r;', 'END'].join(
      '\n',
    ),
  },
];

function routineBySignature(signature: string): RoutineDef | undefined {
  return ROUTINES.find((r) => r.signature === signature);
}

function routineRef(def: RoutineDef): RoutineRef {
  return { signature: def.signature };
}

/** Deterministic stand-in for sha256 of the routine source (64 hex chars). */
function fakeHash(text: string): string {
  let h = 5381;
  for (const c of text) h = (h * 33 + c.codePointAt(0)!) >>> 0;
  return h.toString(16).padStart(8, '0').repeat(8);
}

// -- internal session model --------------------------------------------------------------------

interface Step {
  index: number;
  range: TextRange;
  text: string;
  lineStart: number;
  lineEnd: number;
  info: StatementInfo;
}

interface RoutineFrame {
  def: RoutineDef;
  line: number;
  vars: Record<string, number>;
  /** `v_total` at the time `f_inner` was called (the assignment completes on return). */
  base?: number;
}

interface PendingToken {
  debugId: Uuid;
  text: string;
  confirmText: string;
  expiresAt: number;
}

interface Sess {
  state: DebugState;
  sessionId: Uuid;
  profile: ConnectionProfile | undefined;
  routineOk: boolean;
  options: DebugOptions;
  text: string;
  params: Map<string, ParamInput>;
  steps: Step[];
  cursor: number;
  /** The current script step is executing inside routine frames. */
  inflight: boolean;
  /** The step at `cursor` failed (breakOnError pause); the next command skips it. */
  failed: boolean;
  stack: RoutineFrame[];
  scriptBps: Breakpoint[];
  routineBps: Map<string, { bps: Breakpoint[]; hash: string | null }>;
  hits: Map<string, number>;
  watchSpecs: WatchSpec[];
  tempTables: Set<string>;
  inline: InlineValue[];
  changed: Set<string>;
  stepsRun: number;
  txOpenedAt: number | null;
  last: { kind: 'rows' | 'affected' | 'done'; count: number; elapsedMs: number } | null;
  warnTimer: ReturnType<typeof setTimeout> | null;
  ttlTimer: ReturnType<typeof setTimeout> | null;
  report: DebugEndReport | null;
  /** Grant supplied with the command being applied. */
  grant: ConfirmationGrant | null;
  enteredRoutine: boolean;
}

const sessions = new Map<Uuid, Sess>();
const tokens = new Map<string, PendingToken>();
let routineBreakpoints = new Map<Uuid, RoutineBreakpoint[]>();
let timings = { pausedTtlSecs: 900, txWarnSecs: 60 };

/** Test hook: shorten the simulated paused TTL / transaction warning. */
export function setMockDebugTimings(next: Partial<typeof timings>): void {
  timings = { ...timings, ...next };
}

export function resetMockDebug(): void {
  for (const s of sessions.values()) clearTimers(s);
  sessions.clear();
  tokens.clear();
  routineBreakpoints = new Map();
  timings = { pausedTtlSecs: 900, txWarnSecs: 60 };
}

function find(debugId: Uuid): Sess {
  const s = sessions.get(debugId);
  if (!s) throw ipcError('notFound', `debug session ${debugId}`);
  return s;
}

function isLive(s: Sess): boolean {
  return s.state.status.kind !== 'terminated';
}

function isPaused(s: Sess): boolean {
  return s.state.status.kind === 'paused';
}

// -- events ------------------------------------------------------------------------------------

function emitOutput(s: Sess, output: DebugOutput): void {
  s.state.seq += 1;
  mockEmit('debug://output', { debugId: s.state.debugId, seq: s.state.seq, output });
}

function message(s: Sess, text: string, source: 'info' | 'log' | 'notice' = 'info'): void {
  emitOutput(s, { kind: 'message', text, source });
}

function debugError(kind: string, text: string): DebugError {
  return { kind, message: text };
}

function publish(s: Sess): DebugState {
  refresh(s);
  s.state.seq += 1;
  mockEmit('debug://state', clone(s.state));
  return clone(s.state);
}

// -- text helpers (offsets are Unicode scalar indexes) -----------------------------------------

function lineOfOffset(chars: string[], offset: number): number {
  let line = 1;
  for (let i = 0; i < offset && i < chars.length; i++) if (chars[i] === '\n') line++;
  return line;
}

/** First offset of the statement that is neither whitespace nor part of a leading comment. */
function firstCodeOffset(chars: string[], range: TextRange): number {
  let i = range.start;
  while (i < range.end) {
    if (/\s/.test(chars[i]!)) i++;
    else if (chars[i] === '-' && chars[i + 1] === '-') {
      while (i < range.end && chars[i] !== '\n') i++;
    } else if (chars[i] === '/' && chars[i + 1] === '*') {
      i += 2;
      while (i < range.end && !(chars[i] === '*' && chars[i + 1] === '/')) i++;
      i += 2;
    } else break;
  }
  return Math.min(i, Math.max(range.start, range.end - 1));
}

function buildSteps(analysis: StatementInfo[], text: string, range?: TextRange | null): Step[] {
  const chars = Array.from(text);
  const steps: Step[] = [];
  for (const info of analysis) {
    if (range && (info.range.end <= range.start || info.range.start >= range.end)) continue;
    steps.push({
      index: steps.length,
      range: info.range,
      text: chars.slice(info.range.start, info.range.end).join(''),
      lineStart: lineOfOffset(chars, firstCodeOffset(chars, info.range)),
      lineEnd: lineOfOffset(chars, Math.max(info.range.start, info.range.end - 1)),
      info,
    });
  }
  return steps;
}

/** Step that owns `line`; blank/comment lines snap to the next statement (like JetBrains). */
function snapStep(steps: Step[], line: number): Step | undefined {
  return (
    steps.find((st) => line >= st.lineStart && line <= st.lineEnd) ??
    steps.find((st) => st.lineStart > line)
  );
}

function snapRoutineLine(def: RoutineDef, line: number): number | undefined {
  return def.exec.find((l) => l >= line);
}

// -- expression evaluator (read-only frame) ----------------------------------------------------

type Scalar = number | string | boolean | null;

const TOKEN =
  /\s*(?:(\d+(?:\.\d+)?)|'((?:[^']|'')*)'|([A-Za-z_][\w.]*)|(<>|!=|<=|>=|[-+*/()=<>,]))/y;

function tokenize(src: string): { kind: 'num' | 'str' | 'id' | 'op'; v: string }[] {
  const out: { kind: 'num' | 'str' | 'id' | 'op'; v: string }[] = [];
  TOKEN.lastIndex = 0;
  let pos = 0;
  while (pos < src.length) {
    if (/^\s*$/.test(src.slice(pos))) break;
    TOKEN.lastIndex = pos;
    const m = TOKEN.exec(src);
    if (!m) throw new Error(`syntax error at or near "${src.slice(pos).trim().slice(0, 12)}"`);
    pos = TOKEN.lastIndex;
    if (m[1] !== undefined) out.push({ kind: 'num', v: m[1] });
    else if (m[2] !== undefined) out.push({ kind: 'str', v: m[2].replaceAll("''", "'") });
    else if (m[3] !== undefined) out.push({ kind: 'id', v: m[3] });
    else out.push({ kind: 'op', v: m[4]! });
  }
  return out;
}

function evalScalar(src: string, lookup: (name: string) => Scalar | undefined): Scalar {
  const toks = tokenize(src);
  let i = 0;
  const peek = () => toks[i];
  const isOp = (v: string) => peek()?.kind === 'op' && peek()!.v === v;
  const isKw = (v: string) => peek()?.kind === 'id' && peek()!.v.toLowerCase() === v;
  const num = (x: Scalar): number => {
    if (typeof x === 'number') return x;
    if (typeof x === 'string' && x.trim() !== '' && !Number.isNaN(Number(x))) return Number(x);
    throw new Error('operator does not exist for these operand types');
  };
  const truthy = (x: Scalar): boolean => x === true || (typeof x === 'number' && x !== 0);

  const parseOr = (): Scalar => {
    let l = parseAnd();
    while (isKw('or')) {
      i++;
      const r = parseAnd();
      l = truthy(l) || truthy(r);
    }
    return l;
  };
  const parseAnd = (): Scalar => {
    let l = parseNot();
    while (isKw('and')) {
      i++;
      const r = parseNot();
      l = truthy(l) && truthy(r);
    }
    return l;
  };
  const parseNot = (): Scalar => {
    if (isKw('not')) {
      i++;
      return !truthy(parseNot());
    }
    return parseCmp();
  };
  const parseCmp = (): Scalar => {
    const l = parseAdd();
    const t = peek();
    if (t?.kind === 'op' && ['=', '<>', '!=', '<', '>', '<=', '>='].includes(t.v)) {
      i++;
      const r = parseAdd();
      const a = typeof l === 'string' || typeof r === 'string' ? String(l) : num(l);
      const b = typeof l === 'string' || typeof r === 'string' ? String(r) : num(r);
      switch (t.v) {
        case '=':
          return a === b;
        case '<>':
        case '!=':
          return a !== b;
        case '<':
          return a < b;
        case '>':
          return a > b;
        case '<=':
          return a <= b;
        default:
          return a >= b;
      }
    }
    return l;
  };
  const parseAdd = (): Scalar => {
    let l = parseMul();
    while (isOp('+') || isOp('-')) {
      const op = toks[i++]!.v;
      const r = parseMul();
      l = op === '+' ? num(l) + num(r) : num(l) - num(r);
    }
    return l;
  };
  const parseMul = (): Scalar => {
    let l = parseUnary();
    while (isOp('*') || isOp('/')) {
      const op = toks[i++]!.v;
      const r = num(parseUnary());
      if (op === '/' && r === 0) throw new Error('division by zero');
      l = op === '*' ? num(l) * r : Math.trunc(num(l) / r);
    }
    return l;
  };
  const parseUnary = (): Scalar => {
    if (isOp('-')) {
      i++;
      return -num(parseUnary());
    }
    return parsePrimary();
  };
  const parsePrimary = (): Scalar => {
    const t = toks[i++];
    if (!t) throw new Error('syntax error at end of input');
    if (t.kind === 'num') return Number(t.v);
    if (t.kind === 'str') return t.v;
    if (t.kind === 'op' && t.v === '(') {
      const v = parseOr();
      if (!isOp(')')) throw new Error('syntax error: missing )');
      i++;
      return v;
    }
    if (t.kind === 'id') {
      const lower = t.v.toLowerCase();
      if (lower === 'true') return true;
      if (lower === 'false') return false;
      if (lower === 'null') return null;
      if (isOp('(')) {
        i++;
        const arg = isOp(')') ? null : parseOr();
        if (!isOp(')')) throw new Error('syntax error: missing )');
        i++;
        if (lower === 'abs') return Math.abs(num(arg));
        if (lower === 'upper') return String(arg).toUpperCase();
        if (lower === 'lower') return String(arg).toLowerCase();
        if (lower === 'length') return String(arg).length;
        throw new Error(`function ${lower}() does not exist`);
      }
      const v = lookup(t.v);
      if (v === undefined) throw new Error(`column "${t.v}" does not exist`);
      return v;
    }
    throw new Error(`syntax error at or near "${t.v}"`);
  };

  const result = parseOr();
  if (i < toks.length) throw new Error(`syntax error at or near "${toks[i]!.v}"`);
  return result;
}

function scalarToValue(x: Scalar): { value: Value; typeName: string } {
  if (x === null) return { value: { t: 'null' }, typeName: 'unknown' };
  if (typeof x === 'boolean') return { value: { t: 'bool', v: x }, typeName: 'bool' };
  if (typeof x === 'number') {
    return Number.isInteger(x)
      ? { value: { t: 'int', v: x }, typeName: 'int4' }
      : { value: { t: 'float', v: x }, typeName: 'float8' };
  }
  return { value: { t: 'text', v: x }, typeName: 'text' };
}

function paramScalar(p: ParamInput): Scalar {
  switch (p.kind) {
    case 'null':
      return null;
    case 'bool':
      return p.v;
    case 'number':
      return Number(p.v);
    default:
      return p.v;
  }
}

const USERS_COLUMNS: ColumnMeta[] = [
  { name: 'id', typeName: 'int4', valueKind: 'int', nullable: false },
  { name: 'name', typeName: 'text', valueKind: 'text', nullable: false },
];
const KNOWN_TABLES = new Set(['users', 'orders', 'products', 'customers']);

function userRows(n: number): Value[][] {
  return Array.from({ length: n }, (_, i) => [
    { t: 'int', v: i + 1 } as Value,
    { t: 'text', v: `User ${i + 1}` } as Value,
  ]);
}

const WRITE_WORD =
  /^\s*(insert|update|delete|drop|create|alter|truncate|grant|revoke|merge|call|do|copy|set)\b/i;

function frameInfo(s: Sess, frame: number): { routine: RoutineFrame | null } {
  const idx = s.stack.length - 1 - frame;
  return { routine: idx >= 0 && idx < s.stack.length ? s.stack[idx]! : null };
}

function lookupIn(s: Sess, routine: RoutineFrame | null): (name: string) => Scalar | undefined {
  return (name) => {
    const key = name.toLowerCase();
    if (routine) {
      const v = routine.vars[key];
      if (v !== undefined) return v;
    }
    const p = s.params.get(name.replace(/^[:@$]/, ''));
    return p ? paramScalar(p) : undefined;
  };
}

/** Evaluate in the read-only frame; never throws, errors become `EvalValue.error`. */
function evaluate(s: Sess, expr: string, frame: number): EvalResult {
  const { routine } = frameInfo(s, frame);
  const bare = /^[A-Za-z_]\w*$/.test(expr.trim());
  const context: EvalResult['context'] = routine ? (bare ? 'routineVariable' : 'proxy') : 'lease';
  const result = (value: EvalValue): EvalResult => ({ value, context, elapsedMs: 0 });
  const err = (kind: string, text: string) =>
    result({ kind: 'error', error: debugError(kind, text) });

  let text = expr.trim().replace(/;+\s*$/, '');
  if (text === '') return err('invalid', 'empty expression');
  if (text.includes(';')) return err('readOnly', 'only a single read-only statement is allowed');
  if (WRITE_WORD.test(text)) return err('readOnly', 'evaluation is read-only');
  if (s.state.transaction.status === 'failed') {
    return result({ kind: 'skipped', reason: 'transaction aborted - only ROLLBACK is possible' });
  }
  text = text.replace(/^select\s+/i, '');
  const from = /\bfrom\s+(?:[\w"]+\.)?"?(\w+)"?/i.exec(text);
  if (from) {
    const table = from[1]!.toLowerCase();
    if (!KNOWN_TABLES.has(table) && !s.tempTables.has(table)) {
      return err('query', `relation "${from[1]}" does not exist`);
    }
    const set: ResultSet = { columns: USERS_COLUMNS, rows: userRows(3), truncated: false };
    return result({ kind: 'rows', result: set });
  }
  try {
    const { value, typeName } = scalarToValue(evalScalar(text, lookupIn(s, routine)));
    return result({ kind: 'scalar', value, typeName });
  } catch (e) {
    return err('query', e instanceof Error ? e.message : String(e));
  }
}

function evalTruthy(s: Sess, expr: string, frame: number): boolean | string {
  const r = evaluate(s, expr, frame);
  if (r.value.kind === 'error') return r.value.error.message;
  if (r.value.kind === 'skipped') return r.value.reason;
  if (r.value.kind === 'rows') return r.value.result.rows.length > 0;
  const v = r.value.value;
  if (v.t === 'bool') return v.v;
  if (v.t === 'int') return Number(v.v) !== 0;
  return v.t !== 'null';
}

function valueText(v: Value): string {
  switch (v.t) {
    case 'null':
      return 'NULL';
    case 'text':
      return `'${v.v}'`;
    case 'int':
    case 'uInt':
    case 'float':
    case 'bool':
    case 'decimal':
      return String(v.v);
    default:
      return v.t;
  }
}

function evalDisplay(r: EvalResult): string {
  switch (r.value.kind) {
    case 'scalar':
      return valueText(r.value.value);
    case 'rows':
      return `${r.value.result.rows.length} rows`;
    case 'error':
      return r.value.error.message;
    default:
      return r.value.reason;
  }
}

// -- breakpoints -------------------------------------------------------------------------------

function scriptBpStatuses(s: Sess): BreakpointStatus[] {
  return s.scriptBps.map((b) => {
    const step = snapStep(s.steps, b.line);
    return step
      ? { id: b.id, verified: true, line: Math.max(b.line, step.lineStart) }
      : { id: b.id, verified: false, line: b.line, message: 'no statement at this line' };
  });
}

function routineBpStatuses(s: Sess): BreakpointStatus[] {
  const out: BreakpointStatus[] = [];
  for (const [signature, entry] of s.routineBps) {
    const def = routineBySignature(signature);
    for (const b of entry.bps) {
      if (!def) {
        out.push({ id: b.id, verified: false, line: b.line, message: 'routine not found' });
      } else if (entry.hash && entry.hash !== fakeHash(def.source)) {
        out.push({ id: b.id, verified: false, line: b.line, message: 'source has changed' });
      } else {
        const line = snapRoutineLine(def, b.line);
        out.push(
          line
            ? { id: b.id, verified: true, line }
            : { id: b.id, verified: false, line: b.line, message: 'no executable line' },
        );
      }
    }
  }
  return out;
}

function hasEnabledBreakpoint(s: Sess): boolean {
  if (s.scriptBps.some((b) => b.enabled)) return true;
  for (const e of s.routineBps.values()) if (e.bps.some((b) => b.enabled)) return true;
  return false;
}

/** Breakpoints at the current location; logs are written, returns the ids that stop. */
function checkBreakpoints(s: Sess): string[] {
  if (s.state.muted) return [];
  let candidates: Breakpoint[] = [];
  let line = 0;
  const top = s.stack[s.stack.length - 1];
  if (top) {
    line = top.line;
    const entry = s.routineBps.get(top.def.signature);
    if (entry && (!entry.hash || entry.hash === fakeHash(top.def.source))) {
      candidates = entry.bps.filter((b) => snapRoutineLine(top.def, b.line) === line);
    }
  } else {
    const step = s.steps[s.cursor];
    if (!step) return [];
    line = step.lineStart;
    candidates = s.scriptBps.filter((b) => snapStep(s.steps, b.line)?.index === step.index);
  }
  const stops: string[] = [];
  for (const b of candidates) {
    if (!b.enabled) continue;
    let conditionError: string | null = null;
    if (b.condition?.trim()) {
      const r = evalTruthy(s, b.condition, 0);
      if (typeof r === 'string') conditionError = r;
      else if (!r) continue;
    }
    const hits = (s.hits.get(b.id) ?? 0) + 1;
    s.hits.set(b.id, hits);
    if (conditionError) {
      // A failing condition stops, like JetBrains.
      message(s, `breakpoint condition failed (line ${line}): ${conditionError}`);
      stops.push(b.id);
      continue;
    }
    // hitCount N: stop from the N-th hit on (1 = every hit).
    if (b.hitCount && b.hitCount > 1 && hits < b.hitCount) continue;
    if (b.logMessage) {
      const text = b.logMessage.replace(/\{([^}]*)\}/g, (_m, expr: string) =>
        evalDisplay(evaluate(s, expr, 0)),
      );
      message(s, text, 'log');
    }
    if (b.suspend) stops.push(b.id);
  }
  return stops;
}

// -- guard and execution -----------------------------------------------------------------------

function confirmTextOf(s: Sess, leaseDatabase: string | null): string {
  return leaseDatabase ?? s.profile?.database ?? 'app';
}

type Gate = { kind: 'allow' } | { kind: 'pause'; reason: PauseReason; rejected?: boolean };

async function leaseDatabase(leaseId: Uuid): Promise<string | null> {
  try {
    return (await leasesMock.leaseState(leaseId)).database;
  } catch {
    return null;
  }
}

function gate(s: Sess, step: Step, database: string | null): Gate {
  const cls = step.info.class.kind;
  if (s.profile?.readOnly) {
    const bad =
      cls === 'explain'
        ? (step.info.class as { writes: boolean }).writes
        : !['read', 'transaction', 'session', 'empty'].includes(cls);
    if (bad) {
      return {
        kind: 'pause',
        reason: { kind: 'blocked', error: debugError('readOnly', 'the connection is read-only') },
      };
    }
  }
  if (s.profile?.environment === 'production' && step.info.destructive) {
    const grant = s.grant;
    s.grant = null;
    const entry = grant ? tokens.get(grant.token) : undefined;
    if (grant && entry) tokens.delete(grant.token); // single use, whatever the outcome
    if (
      grant &&
      entry &&
      entry.debugId === s.state.debugId &&
      entry.text === step.text &&
      entry.expiresAt > Date.now() &&
      grant.typed === entry.confirmText
    ) {
      return { kind: 'allow' };
    }
    const confirmText = confirmTextOf(s, database);
    // Only the latest token of a session is valid.
    for (const [k, v] of tokens) if (v.debugId === s.state.debugId) tokens.delete(k);
    const token = newId().replaceAll('-', '');
    tokens.set(token, {
      debugId: s.state.debugId,
      text: step.text,
      confirmText,
      expiresAt: Date.now() + 60_000,
    });
    return {
      kind: 'pause',
      rejected: grant !== null,
      reason: {
        kind: 'confirmation',
        request: {
          token,
          confirmText,
          expiresInMs: 60_000,
          statements: [
            {
              index: step.index,
              range: step.range,
              text: step.text.slice(0, 2000),
              reason: step.info.destructive!,
            },
          ],
        },
      },
    };
  }
  return { kind: 'allow' };
}

function openTransaction(s: Sess): void {
  if (s.options.txMode !== 'wrap' || s.state.transaction.owned) return;
  s.state.transaction = { status: 'active', owned: true, warn: false };
  s.txOpenedAt = Date.now();
}

/** Fake result of a statement, or an error like `planRead` of the lease mock. */
function simulate(
  s: Sess,
  text: string,
  cls: string,
): { events: QueryEvent[]; end: 'rows' | 'affected' | 'done'; count: number } {
  const bad = /\bnope\b/i.exec(text);
  if (bad) throw debugError('query', 'column "nope" does not exist');
  const mk =
    /^\s*create\s+(?:global\s+|local\s+)?temp(?:orary)?\s+table\s+(?:if\s+not\s+exists\s+)?(\w+)/i.exec(
      text,
    );
  if (mk) s.tempTables.add(mk[1]!.toLowerCase());
  const ref = /\b(?:from|join|into|update)\s+(?:[\w"]+\.)?"?(\w+)"?/i.exec(text);
  if (
    ref &&
    cls !== 'ddl' &&
    !/^generate_series$/i.test(ref[1]!) &&
    !KNOWN_TABLES.has(ref[1]!.toLowerCase()) &&
    !s.tempTables.has(ref[1]!.toLowerCase())
  ) {
    throw debugError('query', `relation "${ref[1]}" does not exist`);
  }
  if (cls === 'read' || cls === 'explain') {
    const max = s.options.maxRowsPerStep ?? 200;
    const total = /\bfrom\b/i.test(text) ? 3 : 1;
    const rows = total === 3 ? userRows(3).slice(0, max) : [[{ t: 'int', v: 1 } as Value]];
    const columns: ColumnMeta[] =
      total === 3 ? USERS_COLUMNS : [{ name: '?column?', typeName: 'int4', valueKind: 'int' }];
    return {
      events: [
        { type: 'columns', columns },
        { type: 'rows', rows },
      ],
      end: 'rows',
      count: rows.length,
    };
  }
  if (cls === 'write') return { events: [], end: 'affected', count: 1 };
  return { events: [], end: 'done', count: 0 };
}

function inlineFor(s: Sess, source: SourceRef, line: number, text: string): void {
  s.inline = s.inline.filter(
    (i) => !(i.line === line && JSON.stringify(i.source) === JSON.stringify(source)),
  );
  s.inline.push({ source, line, text, kind: 'result' });
  if (s.inline.length > 200) s.inline.shift();
}

function completeStep(
  s: Sess,
  step: Step,
  events: QueryEvent[],
  end: 'rows' | 'affected' | 'done',
  count: number,
): void {
  for (const event of events) emitOutput(s, { kind: 'query', step: step.index, event });
  const outcome =
    end === 'rows'
      ? ({ kind: 'rows', rowCount: count, truncated: false } as const)
      : end === 'affected'
        ? ({ kind: 'affected', rows: count } as const)
        : ({ kind: 'done' } as const);
  emitOutput(s, {
    kind: 'stepEnd',
    step: step.index,
    end: { outcome, elapsedMs: 0 },
    truncatedAt: null,
  });
  s.last = { kind: end, count, elapsedMs: 0 };
  inlineFor(
    s,
    { kind: 'script' },
    step.lineEnd,
    end === 'rows'
      ? `${count} row${count === 1 ? '' : 's'}`
      : end === 'affected'
        ? `${count} row${count === 1 ? '' : 's'} affected`
        : 'done',
  );
  s.stepsRun += 1;
  s.cursor += 1;
  s.inflight = false;
}

const CALL = /\b(f_outer|f_inner)\s*\(\s*(-?\d+)?/i;

function callTargets(s: Sess, step: Step): RoutineDef[] {
  if (!s.routineOk || step.info.class.kind !== 'read') return [];
  const m = CALL.exec(step.text);
  const def = m ? ROUTINES.find((r) => r.name === m[1]!.toLowerCase()) : undefined;
  return def ? [def] : [];
}

function pushFrame(s: Sess, def: RoutineDef, arg: number): void {
  const vars: Record<string, number> = {};
  vars[def.params[0]!] = arg;
  s.stack.push({ def, line: def.exec[0]!, vars });
  s.changed.add(`${s.stack.length - 1}:${def.params[0]}`);
}

type Unit = 'ok' | 'paused' | 'terminated';

function fail(s: Sess, step: Step, error: DebugError): Unit {
  emitOutput(s, { kind: 'error', step: step.index, error });
  inlineFor(s, { kind: 'script' }, step.lineEnd, error.message);
  s.inline[s.inline.length - 1]!.kind = 'error';
  if (s.state.transaction.owned) s.state.transaction.status = 'failed';
  s.failed = true;
  s.stepsRun += 1;
  if (s.options.breakOnError) {
    setPaused(s, { kind: 'error', error });
    return 'paused';
  }
  terminate(s, 'error', 'rollback');
  return 'terminated';
}

/** Execute one unit: a script step, or one routine line. */
function unit(s: Sess, intoTarget: RoutineDef | null, database: string | null): Unit {
  const top = s.stack[s.stack.length - 1];
  if (top) return routineLine(s, top);
  const step = s.steps[s.cursor]!;
  const g = gate(s, step, database);
  if (g.kind === 'pause') {
    setPaused(s, g.reason);
    // A wrong / expired / reused grant is refused; the new token is in the paused state.
    if (g.rejected) throw ipcError('invalid', 'invalid or expired confirmation');
    return 'paused';
  }
  emitOutput(s, { kind: 'stepStart', step: step.index, line: step.lineStart, text: step.text });
  openTransaction(s);
  if (s.state.transaction.status === 'failed' && step.info.class.kind !== 'transaction') {
    return fail(s, step, debugError('query', 'current transaction is aborted, commands ignored'));
  }
  const targets = callTargets(s, step);
  if (targets.length > 0) {
    const def = intoTarget ?? targets[0]!;
    const m = CALL.exec(step.text);
    pushFrame(s, def, m?.[2] !== undefined ? Number(m[2]) : 1);
    s.inflight = true;
    s.enteredRoutine = true;
    return 'ok';
  }
  try {
    const r = simulate(s, step.text, step.info.class.kind);
    completeStep(s, step, r.events, r.end, r.count);
    return 'ok';
  } catch (e) {
    return fail(s, step, e as DebugError);
  }
}

function routineLine(s: Sess, fr: RoutineFrame): Unit {
  const depthIdx = s.stack.length - 1;
  const src: SourceRef = { kind: 'routine', routine: routineRef(fr.def) };
  const set = (name: string, v: number, line: number) => {
    fr.vars[name] = v;
    s.changed.add(`${depthIdx}:${name}`);
    inlineFor(s, src, line, `${name} = ${v}`);
  };
  const line = fr.line;
  if (fr.def.name === 'f_outer') {
    if (line === 3) {
      set('v_total', fr.vars.p_n! * 2, 3);
      fr.line = 4;
    } else if (line === 4) {
      fr.base = fr.vars.v_total!;
      pushFrame(s, ROUTINES[1]!, fr.base);
      s.enteredRoutine = true;
    } else {
      popFrame(s, fr.vars.v_total!);
    }
  } else if (line === 3) {
    set('v_r', fr.vars.p_x! + 1, 3);
    fr.line = 4;
  } else {
    popFrame(s, fr.vars.v_r!);
  }
  return 'ok';
}

function popFrame(s: Sess, result: number): void {
  const fr = s.stack.pop()!;
  const caller = s.stack[s.stack.length - 1];
  if (caller) {
    const v = (caller.base ?? 0) + result;
    caller.vars.v_total = v;
    s.changed.add(`${s.stack.length - 1}:v_total`);
    inlineFor(
      s,
      { kind: 'routine', routine: routineRef(caller.def) },
      caller.line,
      `v_total = ${v}`,
    );
    caller.line = 5;
    return;
  }
  const step = s.steps[s.cursor]!;
  const column: ColumnMeta = { name: fr.def.name, typeName: 'int4', valueKind: 'int' };
  completeStep(
    s,
    step,
    [
      { type: 'columns', columns: [column] },
      { type: 'rows', rows: [[{ t: 'int', v: result }]] },
    ],
    'rows',
    1,
  );
}

// -- run loop ----------------------------------------------------------------------------------

type Mode =
  | { kind: 'resume' }
  | { kind: 'over' }
  | { kind: 'into'; target: RoutineDef | null }
  | { kind: 'out' }
  | { kind: 'toCursor'; source: SourceRef; line: number; force: boolean };

function atTarget(s: Sess, mode: Extract<Mode, { kind: 'toCursor' }>): boolean {
  const top = s.stack[s.stack.length - 1];
  if (mode.source.kind === 'routine') {
    if (!top || top.def.signature !== mode.source.routine.signature) return false;
    return top.line === snapRoutineLine(top.def, mode.line);
  }
  if (top || s.inflight) return false;
  return snapStep(s.steps, mode.line)?.index === s.cursor;
}

function finishScript(s: Sess): void {
  if (s.state.transaction.owned && s.options.endAction === 'ask') {
    setPaused(s, { kind: 'awaitingEnd' });
    return;
  }
  const action = s.options.endAction === 'ask' ? 'rollback' : s.options.endAction;
  terminate(s, 'finished', action);
}

async function execute(s: Sess, mode: Mode, firstChecksBreakpoint = false): Promise<void> {
  clearTimers(s);
  s.changed = new Set();
  s.enteredRoutine = false;
  if (s.failed && s.stack.length === 0) {
    s.cursor += 1;
    s.failed = false;
  }
  const startDepth = s.stack.length + 1;
  const database = await leaseDatabase(s.state.leaseId);
  s.state.status = { kind: 'running', step: s.cursor };
  publish(s);
  let first = !firstChecksBreakpoint;
  for (let guardCount = 0; guardCount < 100_000; guardCount++) {
    if (s.stack.length === 0 && !s.inflight && s.cursor >= s.steps.length) {
      finishScript(s);
      return;
    }
    if (!first) {
      const force = mode.kind === 'toCursor' && mode.force;
      if (!force) {
        const ids = checkBreakpoints(s);
        if (ids.length > 0) {
          setPaused(s, { kind: 'breakpoint', ids });
          return;
        }
      }
      if (mode.kind === 'toCursor' && atTarget(s, mode)) {
        setPaused(s, { kind: 'runToCursor' });
        return;
      }
    }
    const result = unit(s, mode.kind === 'into' ? mode.target : null, database);
    if (result !== 'ok') return;
    first = false;
    if (s.stack.length === 0 && !s.inflight && s.cursor >= s.steps.length) {
      finishScript(s);
      return;
    }
    const depth = s.stack.length + 1;
    if (mode.kind === 'into') {
      setPaused(s, s.enteredRoutine ? { kind: 'routineEntry' } : { kind: 'step' });
      return;
    }
    if (mode.kind === 'over' && depth <= startDepth) {
      setPaused(s, { kind: 'step' });
      return;
    }
    if (mode.kind === 'out' && depth < startDepth) {
      setPaused(s, { kind: 'step' });
      return;
    }
  }
  setPaused(s, { kind: 'pause' });
}

// -- state refresh, timers, termination --------------------------------------------------------

function clearTimers(s: Sess): void {
  if (s.warnTimer) clearTimeout(s.warnTimer);
  if (s.ttlTimer) clearTimeout(s.ttlTimer);
  s.warnTimer = null;
  s.ttlTimer = null;
}

function armTimers(s: Sess): void {
  clearTimers(s);
  s.state.transaction.ttlLeftSecs = timings.pausedTtlSecs;
  s.state.transaction.warn = false;
  s.warnTimer = setTimeout(() => {
    if (!isPaused(s)) return;
    if (s.state.transaction.status === 'active' || s.state.transaction.status === 'failed') {
      s.state.transaction.warn = true;
    }
    s.state.transaction.ttlLeftSecs = Math.max(0, timings.pausedTtlSecs - timings.txWarnSecs);
    publish(s);
  }, timings.txWarnSecs * 1000);
  s.ttlTimer = setTimeout(() => {
    if (!isPaused(s)) return;
    message(s, 'debug session ended: paused for too long (TTL)');
    terminate(s, 'ttl', 'rollback');
  }, timings.pausedTtlSecs * 1000);
}

function setPaused(s: Sess, reason: PauseReason): void {
  s.state.status = { kind: 'paused', reason };
  s.state.selectedFrame = 0;
  armTimers(s);
  s.state.watches = evalWatches(s);
  publish(s);
}

function terminate(s: Sess, reason: TerminateReason, action: DebugEndAction): DebugEndReport {
  clearTimers(s);
  const owned = s.state.transaction.owned;
  const report: DebugEndReport = { stepsRun: s.stepsRun, committed: false, rolledBack: false };
  if (owned && action === 'commit') {
    report.committed = true;
    message(s, 'debug transaction committed');
  } else if (owned && action === 'rollback') {
    report.rolledBack = true;
    message(s, 'debug transaction rolled back');
  }
  if (owned && action !== 'keep') {
    s.state.transaction = { status: 'idle', owned: false, warn: false };
    s.txOpenedAt = null;
  } else if (owned) {
    s.state.transaction = { ...s.state.transaction, owned: false, warn: false };
  }
  s.stack = [];
  s.inflight = false;
  s.report = report;
  s.state.status = { kind: 'terminated', reason, report };
  publish(s);
  return report;
}

function refresh(s: Sess): void {
  const st = s.state;
  const live = isLive(s);
  const top = s.stack[s.stack.length - 1];
  const step = s.steps[Math.min(s.cursor, Math.max(0, s.steps.length - 1))];
  if (!live) {
    st.position = null;
    st.frames = [];
    st.variables = [];
    st.stepIntoTargets = [];
    st.inlineValues = clone(s.inline);
    st.breakpoints = [...scriptBpStatuses(s), ...routineBpStatuses(s)];
    return;
  }
  const scriptLine = step ? step.lineStart : 1;
  let position: DebugPosition;
  if (top) {
    position = {
      source: { kind: 'routine', routine: routineRef(top.def) },
      line: top.line,
      range: null,
      step: s.cursor,
      stepCount: s.steps.length,
    };
  } else {
    position = {
      source: { kind: 'script' },
      line: scriptLine,
      range: step?.range ?? null,
      step: s.cursor,
      stepCount: s.steps.length,
    };
  }
  st.position = position;
  const frames: DebugFrame[] = [];
  for (let i = s.stack.length - 1; i >= 0; i--) {
    const fr = s.stack[i]!;
    frames.push({
      id: s.stack.length - 1 - i,
      kind: 'routine',
      name: fr.def.name,
      source: { kind: 'routine', routine: routineRef(fr.def) },
      line: fr.line,
      language: 'plpgsql',
      args: fr.def.params.map((p) => `${p}=${fr.vars[p]}`).join(', '),
    });
  }
  frames.push({
    id: s.stack.length,
    kind: 'script',
    name: `script.sql:${scriptLine}`,
    source: { kind: 'script' },
    line: scriptLine,
  });
  st.frames = frames;
  st.variables = topVariables(s);
  st.inlineValues = clone(s.inline);
  st.breakpoints = [...scriptBpStatuses(s), ...routineBpStatuses(s)];
  st.caps = {
    stepInto: s.routineOk,
    stepOut: s.routineOk,
    pauseRunning: true,
    setVariable: true,
    routine: s.routineOk,
    jumpTo: true,
  };
  if (st.transaction.owned && s.txOpenedAt !== null) {
    st.transaction.openForMs = Date.now() - s.txOpenedAt;
  }
  const next = !top && step && !s.inflight ? callTargets(s, step) : [];
  st.stepIntoTargets = next.map(routineRef);
}

// -- variables ---------------------------------------------------------------------------------

function group(reference: string, name: string, count: number): DebugVariable {
  return {
    reference,
    name,
    kind: 'group',
    display: `${count} item${count === 1 ? '' : 's'}`,
    editable: false,
    hasChildren: count > 0,
    changed: false,
  };
}

function topVariables(s: Sess): DebugVariable[] {
  const out: DebugVariable[] = [];
  if (s.params.size > 0) out.push(group('g:params', 'Parameters', s.params.size));
  const { routine } = frameInfo(s, s.state.selectedFrame);
  if (routine) {
    out.push(
      group(
        `g:routine:${s.stack.indexOf(routine)}`,
        `Routine: ${routine.def.name}`,
        routine.def.params.length + routine.def.locals.length,
      ),
    );
  }
  out.push(group('g:session', 'Session', 4));
  out.push(group('g:tx', 'Transaction', 3));
  out.push(group('g:last', 'Last result', s.last ? 3 : 0));
  return out;
}

function leaf(
  reference: string,
  name: string,
  kind: DebugVariable['kind'],
  value: Value,
  editable: boolean,
  changed = false,
): DebugVariable {
  return {
    reference,
    name,
    kind,
    value,
    display: valueText(value),
    typeName: value.t === 'int' ? 'int4' : value.t === 'bool' ? 'bool' : value.t,
    editable,
    hasChildren: false,
    changed,
  };
}

function children(s: Sess, reference: string): DebugVariable[] {
  const text = (v: string): Value => ({ t: 'text', v });
  if (reference === 'g:params') {
    return [...s.params].map(([name, p]) => {
      const sc = paramScalar(p);
      return leaf(`p:${name}`, name, 'parameter', scalarToValue(sc).value, true);
    });
  }
  if (reference === 'g:session') {
    return [
      leaf('s:db', 'database', 'session', text(s.profile?.database ?? 'app'), false),
      leaf('s:user', 'user', 'session', text(s.profile?.username ?? 'postgres'), false),
      leaf('s:pid', 'pid', 'session', { t: 'int', v: 4242 }, false),
      leaf('s:iso', 'isolation', 'session', text('read committed'), false),
    ];
  }
  if (reference === 'g:tx') {
    const tx = s.state.transaction;
    return [
      leaf('t:status', 'status', 'transaction', text(tx.status), false),
      leaf('t:owned', 'owned by debugger', 'transaction', { t: 'bool', v: tx.owned }, false),
      leaf('t:open', 'open (ms)', 'transaction', { t: 'int', v: tx.openForMs ?? 0 }, false),
    ];
  }
  if (reference === 'g:last') {
    if (!s.last) return [];
    return [
      leaf('l:kind', 'kind', 'result', text(s.last.kind), false),
      leaf('l:count', 'rows', 'result', { t: 'int', v: s.last.count }, false),
      leaf('l:ms', 'elapsed (ms)', 'result', { t: 'int', v: s.last.elapsedMs }, false),
    ];
  }
  const m = /^g:routine:(\d+)$/.exec(reference);
  if (m) {
    const idx = Number(m[1]);
    const fr = s.stack[idx];
    if (!fr) return [];
    const mk = (name: string, kind: 'routineArgument' | 'routineLocal') =>
      leaf(
        `v:${idx}:${name}`,
        name,
        kind,
        { t: 'int', v: fr.vars[name] ?? 0 },
        true,
        s.changed.has(`${idx}:${name}`),
      );
    return [
      ...fr.def.params.map((p) => mk(p, 'routineArgument')),
      ...fr.def.locals.filter((l) => fr.vars[l] !== undefined).map((l) => mk(l, 'routineLocal')),
    ];
  }
  return [];
}

// -- watches -----------------------------------------------------------------------------------

function evalWatches(s: Sess): WatchResult[] {
  return s.watchSpecs.map((w) => ({
    id: w.id,
    expr: w.expr,
    result: evaluate(s, w.expr, s.state.selectedFrame),
  }));
}

// -- session construction ----------------------------------------------------------------------

function buildSession(
  request: DebugStartRequest,
  sessionId: Uuid,
  steps: Step[],
  text: string,
): Sess {
  const options = { ...DEFAULT_DEBUG_OPTIONS, ...request.options };
  const kind = mockSessionKind(sessionId);
  const connectionId = mockSessionConnectionId(sessionId);
  const routineOk = (kind === 'postgres' || kind === 'yugabytedb') && options.routine;
  const state: DebugState = {
    debugId: newId(),
    leaseId: request.leaseId,
    seq: 0,
    status: { kind: 'starting' },
    position: null,
    frames: [],
    selectedFrame: 0,
    variables: [],
    watches: [],
    inlineValues: [],
    breakpoints: [],
    transaction: { status: 'idle', owned: false, warn: false },
    caps: {
      stepInto: routineOk,
      stepOut: routineOk,
      pauseRunning: true,
      setVariable: true,
      routine: routineOk,
      jumpTo: true,
    },
    stepIntoTargets: [],
    muted: request.muted ?? false,
  };
  const s: Sess = {
    state,
    sessionId,
    profile: connectionId ? findMockProfile(connectionId) : undefined,
    routineOk,
    options,
    text,
    params: new Map((request.params ?? []).map((p) => [p.name.replace(/^[:@$]/, ''), p.value])),
    steps,
    cursor: 0,
    inflight: false,
    failed: false,
    stack: [],
    scriptBps: clone(request.breakpoints ?? []),
    routineBps: new Map(),
    hits: new Map(),
    watchSpecs: clone(request.watches ?? []),
    tempTables: new Set(),
    inline: [],
    changed: new Set(),
    stepsRun: 0,
    txOpenedAt: null,
    last: null,
    warnTimer: null,
    ttlTimer: null,
    report: null,
    grant: null,
    enteredRoutine: false,
  };
  for (const rb of request.routineBreakpoints ?? []) {
    const entry = s.routineBps.get(rb.routine.signature) ?? {
      bps: [],
      hash: rb.sourceHash ?? null,
    };
    entry.bps.push(clone(rb.breakpoint));
    s.routineBps.set(rb.routine.signature, entry);
  }
  return s;
}

async function startSession(request: DebugStartRequest, replaced?: Uuid): Promise<DebugState> {
  const sessionId = mockLeaseSessionId(request.leaseId);
  if (!sessionId) throw ipcError('notFound', `lease ${request.leaseId}`);
  const live = [...sessions.values()].filter(isLive);
  if (live.some((x) => x.state.leaseId === request.leaseId && x.state.debugId !== replaced)) {
    throw ipcError('busy', 'lease is already being debugged');
  }
  if (live.filter((x) => x.state.debugId !== replaced).length >= 8) {
    throw ipcError('busy', 'too many debug sessions');
  }
  const analysis = await leasesMock.scriptAnalyze(sessionId, request.text);
  const steps = buildSteps(analysis.statements, request.text, request.range);
  const s = buildSession(request, sessionId, steps, request.text);
  sessions.set(s.state.debugId, s);
  if (steps.length === 0) {
    terminate(s, 'finished', 'rollback');
    return clone(s.state);
  }
  const stopOnEntry = s.options.stopOnEntry ?? !hasEnabledBreakpoint(s);
  if (stopOnEntry) {
    s.state.status = { kind: 'paused', reason: { kind: 'entry' } };
    setPaused(s, { kind: 'entry' });
  } else {
    await execute(s, { kind: 'resume' }, true);
  }
  return clone(s.state);
}

// -- commands ----------------------------------------------------------------------------------

async function checkLease(s: Sess): Promise<void> {
  try {
    const lease = await leasesMock.leaseState(s.state.leaseId);
    if (lease.broken && isLive(s)) {
      message(s, 'lease lost');
      terminate(s, 'leaseLost', 'rollback');
    }
  } catch {
    if (isLive(s)) terminate(s, 'leaseLost', 'rollback');
  }
}

function resolveAction(s: Sess, end: DebugEndAction | null | undefined): DebugEndAction {
  return end ?? s.options.endAction;
}

export const debugMock = {
  async debugSupport(leaseId: Uuid): Promise<DebugSupport> {
    const sessionId = mockLeaseSessionId(leaseId);
    if (!sessionId) throw ipcError('notFound', `lease ${leaseId}`);
    const kind = mockSessionKind(sessionId);
    const routine: DebugSupport['routine'] =
      kind === 'postgres' || kind === 'yugabytedb'
        ? { kind: 'available' }
        : { kind: 'unsupported', reason: 'no debugger API' };
    return { script: true, routine, transactions: true, scriptVariables: false };
  },
  async debugStart(request: DebugStartRequest): Promise<DebugState> {
    return startSession(request);
  },
  /** `confirm` is the answer to a `confirmation` pause (one-time token). */
  async debugStep(
    debugId: Uuid,
    action: DebugAction,
    confirm?: ConfirmationGrant | null,
  ): Promise<DebugState> {
    const s = find(debugId);
    if (!isLive(s)) throw ipcError('invalid', 'the debug session has ended');
    await checkLease(s);
    if (!isLive(s)) return clone(s.state);
    if (!isPaused(s)) throw ipcError('busy', 'the debug session is running');
    const reason = s.state.status.kind === 'paused' ? s.state.status.reason : null;
    if (reason?.kind === 'awaitingEnd') {
      throw ipcError('invalid', 'the script has ended: choose commit or rollback (stop)');
    }
    s.grant = confirm ?? null;
    const top = s.stack.length > 0;
    switch (action.kind) {
      case 'pause':
        setPaused(s, { kind: 'pause' });
        break;
      case 'resume':
        await execute(s, { kind: 'resume' });
        break;
      case 'stepOver':
        await execute(s, { kind: 'over' });
        break;
      case 'stepInto':
      case 'forceStepInto': {
        const step = s.steps[s.cursor];
        const canEnter = top || (step && !s.inflight && callTargets(s, step).length > 0);
        if (!canEnter) {
          message(s, 'Step Into is not available here: stepping over the statement');
          await execute(s, { kind: 'over' });
        } else {
          await execute(s, { kind: 'into', target: null });
        }
        break;
      }
      case 'smartStepInto': {
        const step = s.steps[s.cursor];
        const def = routineBySignature(action.signature);
        const ok = !top && step && def && callTargets(s, step).some((d) => d === def);
        if (!ok) throw ipcError('invalid', `no such step-into target: ${action.signature}`);
        await execute(s, { kind: 'into', target: def });
        break;
      }
      case 'stepOut':
        if (!top) throw ipcError('invalid', 'Step Out is not available in the script frame');
        await execute(s, { kind: 'out' });
        break;
      case 'runToCursor':
        await execute(s, {
          kind: 'toCursor',
          source: action.source,
          line: action.line,
          force: action.force,
        });
        break;
      case 'jumpTo': {
        if (top) throw ipcError('invalid', 'Jump To is only available in the script frame');
        const target = snapStep(s.steps, action.line);
        if (!target) throw ipcError('invalid', `no statement at line ${action.line}`);
        s.cursor = target.index;
        s.failed = false;
        s.inflight = false;
        setPaused(s, { kind: 'step' });
        break;
      }
    }
    return clone(s.state);
  },
  async debugStop(debugId: Uuid, end?: DebugEndAction | null): Promise<DebugEndReport> {
    const s = sessions.get(debugId);
    if (!s) return { stepsRun: 0, committed: false, rolledBack: false };
    if (!isLive(s)) return clone(s.report ?? { stepsRun: 0, committed: false, rolledBack: false });
    const action = resolveAction(s, end);
    const awaiting =
      s.state.status.kind === 'paused' && s.state.status.reason.kind === 'awaitingEnd';
    if (action === 'ask') {
      if (awaiting) throw ipcError('invalid', 'choose commit, rollback or keep');
      if (s.state.transaction.owned) {
        // The UI shows the commit / rollback dialog and calls stop again with the choice.
        setPaused(s, { kind: 'awaitingEnd' });
        return { stepsRun: s.stepsRun, committed: false, rolledBack: false };
      }
      return terminate(s, 'stopped', 'rollback');
    }
    return terminate(s, 'stopped', action);
  },
  async debugRestart(
    debugId: Uuid,
    text: string,
    end?: DebugEndAction | null,
  ): Promise<DebugState> {
    const old = find(debugId);
    if (isLive(old)) {
      const action = resolveAction(old, end);
      terminate(old, 'stopped', action === 'ask' ? 'rollback' : action);
    }
    const state = await startSession(
      {
        leaseId: old.state.leaseId,
        text,
        params: [...old.params].map(([name, value]) => ({ name, value })),
        breakpoints: old.scriptBps,
        routineBreakpoints: [...old.routineBps].flatMap(([signature, e]) =>
          e.bps.map((breakpoint) => ({
            routine: { signature },
            sourceHash: e.hash,
            breakpoint,
          })),
        ),
        watches: old.watchSpecs,
        options: old.options,
        muted: old.state.muted,
      },
      debugId,
    );
    return state;
  },
  async debugEvaluate(debugId: Uuid, expr: string, frame?: number | null): Promise<EvalResult> {
    const s = find(debugId);
    if (!isPaused(s)) throw ipcError('invalid', 'the debug session is not paused');
    clearTimers(s);
    const result = evaluate(s, expr, frame ?? s.state.selectedFrame);
    armTimers(s);
    return result;
  },
  async debugSetBreakpoints(
    debugId: Uuid,
    source: SourceRef,
    breakpoints: Breakpoint[],
  ): Promise<BreakpointStatus[]> {
    const s = find(debugId);
    if (!isLive(s)) throw ipcError('invalid', 'the debug session has ended');
    if (source.kind === 'script') {
      s.scriptBps = clone(breakpoints);
    } else {
      const signature = source.routine.signature;
      const prev = s.routineBps.get(signature);
      s.routineBps.set(signature, { bps: clone(breakpoints), hash: prev?.hash ?? null });
    }
    const statuses =
      source.kind === 'script'
        ? scriptBpStatuses(s)
        : routineBpStatuses(s).filter((st) => breakpoints.some((b) => b.id === st.id));
    publish(s);
    return clone(statuses);
  },
  async debugSetMuted(debugId: Uuid, muted: boolean): Promise<DebugState> {
    const s = find(debugId);
    s.state.muted = muted;
    return publish(s);
  },
  async debugSetVariable(
    debugId: Uuid,
    reference: string,
    valueText: string,
  ): Promise<DebugVariable> {
    const s = find(debugId);
    if (!isPaused(s)) throw ipcError('invalid', 'the debug session is not paused');
    const n = Number(valueText.trim());
    const m = /^v:(\d+):(\w+)$/.exec(reference);
    if (m) {
      const fr = s.stack[Number(m[1])];
      if (!fr || fr.vars[m[2]!] === undefined) throw ipcError('notFound', reference);
      if (valueText.trim() === '' || !Number.isInteger(n)) {
        throw ipcError('invalid', `invalid input syntax for integer: "${valueText}"`);
      }
      fr.vars[m[2]!] = n;
      s.changed.add(`${m[1]}:${m[2]}`);
      publish(s);
      return children(s, `g:routine:${m[1]}`).find((v) => v.reference === reference)!;
    }
    const p = /^p:(.+)$/.exec(reference);
    if (p && s.params.has(p[1]!)) {
      s.params.set(
        p[1]!,
        valueText.trim() !== '' && !Number.isNaN(n)
          ? { kind: 'number', v: String(n) }
          : { kind: 'text', v: valueText },
      );
      publish(s);
      return children(s, 'g:params').find((v) => v.reference === reference)!;
    }
    throw ipcError('invalid', 'variable is not editable');
  },
  async debugVariableChildren(debugId: Uuid, reference: string): Promise<DebugVariable[]> {
    return clone(children(find(debugId), reference));
  },
  async debugWatchesSet(debugId: Uuid, watches: WatchSpec[]): Promise<WatchResult[]> {
    const s = find(debugId);
    s.watchSpecs = clone(watches);
    s.state.watches = isPaused(s)
      ? evalWatches(s)
      : watches.map((w) => ({
          id: w.id,
          expr: w.expr,
          result: {
            value: { kind: 'skipped', reason: 'session is not paused' },
            context: 'lease',
            elapsedMs: 0,
          },
        }));
    publish(s);
    return clone(s.state.watches);
  },
  async debugSelectFrame(debugId: Uuid, frame: number): Promise<DebugState> {
    const s = find(debugId);
    if (frame < 0 || frame > s.stack.length) throw ipcError('invalid', `no frame ${frame}`);
    s.state.selectedFrame = frame;
    s.state.watches = isPaused(s) ? evalWatches(s) : s.state.watches;
    return publish(s);
  },
  async debugRoutineSource(
    debugId: Uuid | null,
    sessionId: Uuid,
    routine: RoutineRef,
  ): Promise<RoutineSource> {
    void [debugId, sessionId];
    const def = routineBySignature(routine.signature);
    if (!def) {
      return {
        text: `-- ${routine.signature}\nBEGIN\n  RETURN;\nEND`,
        sourceHash: '0'.repeat(64),
        language: 'plpgsql',
        firstLine: 1,
      };
    }
    return {
      text: def.source,
      sourceHash: fakeHash(def.source),
      language: 'plpgsql',
      firstLine: 1,
    };
  },
  async debugSnapshot(debugId: Uuid): Promise<DebugState> {
    const s = find(debugId);
    refresh(s);
    return clone(s.state);
  },
  async routineBreakpointsGet(connectionId: Uuid): Promise<RoutineBreakpoint[]> {
    return clone(routineBreakpoints.get(connectionId) ?? []);
  },
  async routineBreakpointsSet(connectionId: Uuid, list: RoutineBreakpoint[]): Promise<void> {
    routineBreakpoints.set(connectionId, clone(list));
  },
};
