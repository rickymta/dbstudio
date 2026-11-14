// Pure helpers over a `PlanTree`: traversal, warnings, hot path, numbers, raw-text locating.

import type { TFunction } from 'i18next';
import { ipc } from '@/lib/ipc';
import type {
  IpcError,
  PlanFormat,
  PlanNode,
  PlanTree,
  PlanWarning,
  RawPlan,
  Severity,
} from '@/lib/types';

/** Pre-order list of every node (the order of `id`). */
export function flatten(root: PlanNode): PlanNode[] {
  const out: PlanNode[] = [];
  const walk = (n: PlanNode) => {
    out.push(n);
    n.children.forEach(walk);
  };
  walk(root);
  return out;
}

export function nodeById(tree: PlanTree, id: number): PlanNode | undefined {
  return flatten(tree.root).find((n) => n.id === id);
}

/** Parent id of every node (`null` for the root). */
export function parentMap(root: PlanNode): Map<number, number | null> {
  const map = new Map<number, number | null>([[root.id, null]]);
  const walk = (n: PlanNode) =>
    n.children.forEach((c) => {
      map.set(c.id, n.id);
      walk(c);
    });
  walk(root);
  return map;
}

/** Ids from the root down to `id`. */
export function pathTo(root: PlanNode, id: number): number[] {
  const parents = parentMap(root);
  const out: number[] = [];
  for (let cur: number | null | undefined = id; cur != null; cur = parents.get(cur)) {
    out.unshift(cur);
  }
  return out;
}

export function hotSet(tree: PlanTree): Set<number> {
  return new Set(tree.hotPath);
}

export function warningsByNode(tree: PlanTree): Map<number, PlanWarning[]> {
  const map = new Map<number, PlanWarning[]>();
  for (const w of tree.warnings) {
    const list = map.get(w.nodeId) ?? [];
    list.push(w);
    map.set(w.nodeId, list);
  }
  return map;
}

const SEVERITY_RANK: Record<Severity, number> = { info: 0, warning: 1, critical: 2 };

export function maxSeverity(warnings: readonly PlanWarning[] | undefined): Severity | null {
  let best: Severity | null = null;
  for (const w of warnings ?? []) {
    if (best === null || SEVERITY_RANK[w.severity] > SEVERITY_RANK[best]) best = w.severity;
  }
  return best;
}

/** What the plan can be measured by: actual time when present, else estimated cost. */
export function metricOf(tree: PlanTree): 'time' | 'cost' | null {
  const nodes = flatten(tree.root);
  if (nodes.some((n) => n.selfTimeMs != null)) return 'time';
  if (nodes.some((n) => n.selfCost != null)) return 'cost';
  return null;
}

/** Share of the whole plan spent in the node alone, 0..1 (`null` when unknown). */
export function nodeShare(tree: PlanTree, node: PlanNode): number | null {
  const metric = metricOf(tree);
  if (metric === 'time') {
    const total = tree.root.actualTimeMs ?? tree.totals.executionTimeMs;
    return total && node.selfTimeMs != null ? Math.min(1, node.selfTimeMs / total) : null;
  }
  if (metric === 'cost') {
    const total = tree.root.estCost ?? tree.totals.totalCost;
    return total && node.selfCost != null ? Math.min(1, node.selfCost / total) : null;
  }
  return null;
}

// ---------------------------------------------------------------------------------------------
// Number formatting
// ---------------------------------------------------------------------------------------------

const DASH = '-';

export function formatCount(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return DASH;
  if (Math.abs(n) >= 1e7) return `${(n / 1e6).toFixed(1)}M`;
  return new Intl.NumberFormat('en-US', { maximumFractionDigits: n < 10 && n % 1 ? 1 : 0 }).format(
    n,
  );
}

export function formatCost(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return DASH;
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  return new Intl.NumberFormat('en-US', { maximumFractionDigits: n < 100 ? 2 : 0 }).format(n);
}

export function formatDuration(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return DASH;
  if (ms < 0.001) return '0 ms';
  if (ms < 1) return `${ms.toFixed(3)} ms`;
  if (ms < 1000) return `${ms < 10 ? ms.toFixed(2) : ms.toFixed(1)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`;
  return `${Math.floor(ms / 60_000)} min ${Math.round((ms % 60_000) / 1000)} s`;
}

export function formatPercent(share: number | null): string {
  return share == null ? DASH : `${(share * 100).toFixed(share < 0.1 ? 1 : 0)}%`;
}

// ---------------------------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------------------------

/** `schema.name (alias) via index` */
export function objectLabel(node: PlanNode): string {
  const o = node.object;
  if (!o) return '';
  const base = o.schema ? `${o.schema}.${o.name}` : o.name;
  return o.alias && o.alias !== o.name ? `${base} (${o.alias})` : base;
}

export function warningKey(w: PlanWarning): string {
  if (w.rule === 'misestimate') {
    return w.params.direction === 'over' ? 'warning.misestimateOver' : 'warning.misestimateUnder';
  }
  return `warning.${w.rule}`;
}

export function warningMessage(t: TFunction<'plan'>, w: PlanWarning): string {
  return t(warningKey(w), { ...w.params, defaultValue: w.rule });
}

/** Text of an error of `plan_explain`, using the capability to explain what is unsupported. */
export function explainErrorMessage(t: TFunction<'plan'>, e: IpcError): string {
  if (e.kind === 'unsupported') {
    switch (e.capability) {
      case 'explain_analyze_writes':
        return t('plan:errors.analyzeWrites');
      case 'explain_analyze_ddl':
        return t('plan:errors.analyzeDdl');
      case 'explain_analyze':
        return t('plan:errors.analyzeVersion');
      default:
        return t('plan:errors.unsupported', { what: e.capability ?? e.message });
    }
  }
  return e.message;
}

export const FORMAT_LANGUAGE: Record<PlanFormat, 'json' | 'xml' | 'text'> = {
  postgresJson: 'json',
  mySqlJson: 'json',
  mySqlJsonV2: 'json',
  mySqlTree: 'text',
  mariaDbJson: 'json',
  msSqlShowplanXml: 'xml',
  oraclePlanRows: 'json',
  mongoExplain: 'json',
  clickHouseJson: 'json',
  cockroachText: 'text',
  tiDbRows: 'json',
  cqlTrace: 'json',
};

// ---------------------------------------------------------------------------------------------
// Statement classification (decides whether ANALYZE needs the "this changes data" warning)
// ---------------------------------------------------------------------------------------------

/** Words of a SQL text outside comments, strings and quoted identifiers, with their paren depth. */
function sqlWords(text: string): { word: string; depth: number; call: boolean }[] {
  const out: { word: string; depth: number; call: boolean }[] = [];
  let depth = 0;
  let i = 0;
  while (i < text.length) {
    const c = text[i]!;
    if (c === '(') {
      depth++;
      i++;
    } else if (c === ')') {
      depth = Math.max(0, depth - 1);
      i++;
    } else if (c === '-' && text[i + 1] === '-') {
      while (i < text.length && text[i] !== '\n') i++;
    } else if (c === '#') {
      while (i < text.length && text[i] !== '\n') i++;
    } else if (c === '/' && text[i + 1] === '*') {
      const end = text.indexOf('*/', i + 2);
      i = end < 0 ? text.length : end + 2;
    } else if (c === "'" || c === '"' || c === '`') {
      i++;
      while (i < text.length) {
        if (text[i] === '\\' && c !== '`') i += 2;
        else if (text[i] === c) {
          if (text[i + 1] === c) i += 2;
          else break;
        } else i++;
      }
      i++;
    } else if (/[A-Za-z_]/.test(c)) {
      const start = i;
      while (i < text.length && /[A-Za-z0-9_]/.test(text[i]!)) i++;
      out.push({
        word: text.slice(start, i).toUpperCase(),
        depth,
        call: /^\s*\(/.test(text.slice(i, i + 16)),
      });
    } else i++;
  }
  return out;
}

const WRITE_WORDS = new Set(['INSERT', 'UPDATE', 'DELETE', 'MERGE', 'REPLACE', 'TRUNCATE']);

/** Functions acting outside the transaction of an ANALYZE (locks, sequences, other servers). */
function isImpureCall(word: string): boolean {
  const w = word.toLowerCase();
  return (
    [
      'nextval',
      'setval',
      'lo_import',
      'lo_export',
      'pg_terminate_backend',
      'pg_cancel_backend',
      'get_lock',
      'release_lock',
      'release_all_locks',
      'load_file',
    ].includes(w) ||
    w.startsWith('pg_advisory') ||
    w.startsWith('pg_try_advisory') ||
    w.startsWith('dblink')
  );
}

/**
 * Local guess whether executing `sql` can change data or act outside its transaction (ANALYZE
 * then deserves a warning). Used when the backend analysis is not available; it errs on the
 * safe side: a write keyword anywhere (also inside a CTE), `INTO`, lock/sequence functions.
 */
export function isWriteStatement(sql: string): boolean {
  const words = sqlWords(sql);
  const first = words[0]?.word;
  if (!first) return false;
  if (WRITE_WORDS.has(first) || first === 'CREATE' || first === 'EXECUTE') return true;
  if (first !== 'WITH' && first !== 'SELECT') return false;
  return words.some(
    (w) =>
      // `REPLACE(...)` and `INSERT(...)` are string functions.
      (WRITE_WORDS.has(w.word) && !w.call) ||
      (w.word === 'INTO' && w.depth === 0) ||
      (w.call && isImpureCall(w.word)),
  );
}

/**
 * Whether the statement writes, from the backend's script analysis (the parser of the
 * session's dialect) combined with the local guess; falls back to the guess alone when the
 * analysis cannot be fetched.
 */
export async function statementWrites(sessionId: string, sql: string): Promise<boolean> {
  const local = isWriteStatement(sql);
  try {
    const analysis = await ipc.scriptAnalyze(sessionId, sql);
    const backend = analysis.statements.some(
      (s) => s.class.kind !== 'read' && s.class.kind !== 'empty',
    );
    return backend || local;
  } catch {
    return local;
  }
}

// ---------------------------------------------------------------------------------------------
// Raw text locating
// ---------------------------------------------------------------------------------------------

/**
 * Inclusive 0-based line range of `node` in `raw.text`, or `null` when it cannot be found.
 * `lines` refs are used directly; XML nodes are found by `NodeId`; Oracle rows by their `ID`.
 */
export function locateRaw(raw: RawPlan, node: PlanNode): [number, number] | null {
  const lines = raw.text.split('\n');
  const ref = node.raw;
  switch (ref.kind) {
    case 'lines':
      return ref.start < lines.length ? [ref.start, Math.min(ref.end, lines.length - 1)] : null;
    case 'xmlNode': {
      const open = new RegExp(`<RelOp\\b[^>]*\\bNodeId="${escapeRegExp(ref.nodeId)}"`);
      const start = lines.findIndex((l) => open.test(l));
      if (start < 0) return null;
      let depth = 0;
      for (let i = start; i < lines.length; i++) {
        const line = lines[i]!;
        depth += (line.match(/<RelOp\b/g) ?? []).length;
        const selfClosed = /<RelOp\b[^>]*\/>/.test(line) ? 1 : 0;
        depth -= (line.match(/<\/RelOp>/g) ?? []).length + selfClosed;
        if (depth <= 0) return [start, i];
      }
      return [start, start];
    }
    case 'row': {
      const idRe = new RegExp(`"ID"\\s*:\\s*${ref.id}\\b`, 'i');
      const hit = lines.findIndex((l) => idRe.test(l));
      if (hit < 0) return null;
      let start = hit;
      while (start > 0 && !lines[start]!.includes('{')) start--;
      let end = hit;
      while (end < lines.length - 1 && !lines[end]!.includes('}')) end++;
      return [start, end];
    }
    default:
      return null;
  }
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
