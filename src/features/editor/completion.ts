// Schema-aware completion for the SQL editor (QUERY-03). Everything here is pure: the CodeMirror
// wiring lives in `completionSource.ts`. Offsets are UTF-16 indexes (CodeMirror positions).

export interface CatalogColumn {
  name: string;
  typeName: string;
}

export interface CatalogTable {
  schema: string | null;
  name: string;
  /** `table`, `view`, `collection`... (only used for display). */
  kind: string;
  columns: CatalogColumn[];
}

export interface Catalog {
  tables: CatalogTable[];
  /** Schema searched first when a table name is not qualified. */
  defaultSchema?: string | null;
}

export type CompletionKind =
  'table' | 'view' | 'column' | 'schema' | 'keyword' | 'function' | 'alias';

export interface CompletionOption {
  label: string;
  type: CompletionKind;
  detail?: string;
  boost?: number;
  /** Text inserted instead of `label` (quoted identifiers, `name()`...). */
  apply?: string;
}

export interface CompletionResult {
  from: number;
  options: CompletionOption[];
}

export interface CompletionVocabulary {
  keywords: readonly string[];
  functions: readonly string[];
}

// -- tokenizer ---------------------------------------------------------------------------------

type TokenKind = 'word' | 'quoted' | 'punct' | 'string' | 'comment';

export interface Token {
  kind: TokenKind;
  from: number;
  to: number;
  /** Identifier text without quotes (words and quoted identifiers), else the raw text. */
  value: string;
  /** The literal or comment ends at the end of the text without being closed. */
  open?: boolean;
}

const WORD_START = /[\p{L}_$#@]/u;
const WORD_PART = /[\p{L}\p{N}_$#@]/u;

/** Split `text` into tokens; whitespace is skipped. Strings and comments become single tokens. */
export function tokenize(text: string, bracketQuotes = false): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  const n = text.length;
  while (i < n) {
    const c = text[i]!;
    if (/\s/.test(c)) {
      i++;
    } else if (c === '-' && text[i + 1] === '-') {
      const nl = text.indexOf('\n', i);
      const to = nl < 0 ? n : nl;
      tokens.push({ kind: 'comment', from: i, to, value: text.slice(i, to), open: nl < 0 });
      i = to;
    } else if (c === '/' && text[i + 1] === '*') {
      const end = text.indexOf('*/', i + 2);
      const to = end < 0 ? n : end + 2;
      tokens.push({ kind: 'comment', from: i, to, value: text.slice(i, to), open: end < 0 });
      i = to;
    } else if (c === "'" || c === '"' || c === '`' || (bracketQuotes && c === '[')) {
      const close = c === '[' ? ']' : c;
      let j = i + 1;
      let closed = false;
      while (j < n) {
        if (text[j] === close) {
          if (text[j + 1] === close && c !== '[') {
            j += 2; // doubled quote escapes itself
            continue;
          }
          closed = true;
          j++;
          break;
        }
        if (c === "'" && text[j] === '\\') j++;
        j++;
      }
      const to = Math.min(j, n);
      const raw = text.slice(i, to);
      const inner = closed ? raw.slice(1, -1) : raw.slice(1);
      tokens.push({
        kind: c === "'" ? 'string' : 'quoted',
        from: i,
        to,
        value: c === "'" ? raw : inner.split(close + close).join(close),
        open: !closed,
      });
      i = to;
    } else if (WORD_START.test(c)) {
      let j = i + 1;
      while (j < n && WORD_PART.test(text[j]!)) j++;
      tokens.push({ kind: 'word', from: i, to: j, value: text.slice(i, j) });
      i = j;
    } else if (/\d/.test(c)) {
      let j = i + 1;
      while (j < n && /[\w.]/.test(text[j]!)) j++;
      tokens.push({ kind: 'punct', from: i, to: j, value: text.slice(i, j) });
      i = j;
    } else {
      tokens.push({ kind: 'punct', from: i, to: i + 1, value: c });
      i++;
    }
  }
  return tokens;
}

const isIdent = (t: Token | undefined): t is Token =>
  t !== undefined && (t.kind === 'word' || t.kind === 'quoted');
const isPunct = (t: Token | undefined, v: string): boolean =>
  t !== undefined && t.kind === 'punct' && t.value === v;
const isWord = (t: Token | undefined, v: string): boolean =>
  t !== undefined && t.kind === 'word' && t.value.toUpperCase() === v;

/** Tokens of the statement (between `;` separators) that contains `pos`. */
export function statementTokens(tokens: Token[], pos: number): Token[] {
  let start = 0;
  let end = tokens.length;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (isPunct(t, ';')) {
      if (t.to <= pos) start = i + 1;
      else {
        end = i;
        break;
      }
    }
  }
  return tokens.slice(start, end);
}

// -- table references --------------------------------------------------------------------------

export interface TableRef {
  schema: string | null;
  name: string;
  alias: string | null;
}

/** Words that can follow a table name without being its alias. */
const NOT_ALIAS = new Set([
  'WHERE',
  'ON',
  'JOIN',
  'INNER',
  'LEFT',
  'RIGHT',
  'FULL',
  'CROSS',
  'NATURAL',
  'OUTER',
  'GROUP',
  'ORDER',
  'HAVING',
  'LIMIT',
  'OFFSET',
  'UNION',
  'INTERSECT',
  'EXCEPT',
  'SET',
  'VALUES',
  'USING',
  'SELECT',
  'RETURNING',
  'FETCH',
  'FOR',
  'WINDOW',
  'LATERAL',
  'AS',
  'FROM',
  'INTO',
  'WITH',
  'STRAIGHT_JOIN',
  'PIVOT',
  'UNPIVOT',
  'TABLESAMPLE',
  'OVERRIDING',
  'DEFAULT',
]);

/** Parse `schema.name` starting at `tokens[i]`; returns the name parts and the next index. */
function readQualified(tokens: Token[], i: number): { parts: string[]; next: number } | null {
  if (!isIdent(tokens[i])) return null;
  const parts = [tokens[i]!.value];
  let j = i + 1;
  while (isPunct(tokens[j], '.') && isIdent(tokens[j + 1])) {
    parts.push(tokens[j + 1]!.value);
    j += 2;
  }
  return { parts, next: j };
}

/** Tables a statement refers to (FROM / JOIN / UPDATE / INTO), with their aliases. */
export function parseTableRefs(tokens: Token[]): TableRef[] {
  const refs: TableRef[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (t.kind !== 'word') continue;
    const kw = t.value.toUpperCase();
    if (kw !== 'FROM' && kw !== 'JOIN' && kw !== 'UPDATE' && kw !== 'INTO') continue;
    let j = i + 1;
    for (;;) {
      if (isWord(tokens[j], 'ONLY') || isWord(tokens[j], 'LATERAL')) j++;
      const q = readQualified(tokens, j);
      if (!q) break; // sub-select, function call or incomplete text
      j = q.next;
      if (isPunct(tokens[j], '(')) break; // function call (`generate_series(...)`) or INSERT column list
      const name = q.parts[q.parts.length - 1]!;
      const schema = q.parts.length > 1 ? q.parts[q.parts.length - 2]! : null;
      let alias: string | null = null;
      if (isWord(tokens[j], 'AS')) j++;
      const a = tokens[j];
      if (isIdent(a) && (a.kind === 'quoted' || !NOT_ALIAS.has(a.value.toUpperCase()))) {
        alias = a.value;
        j++;
      }
      refs.push({ schema, name, alias });
      // `FROM a, b`: the comma list only applies to FROM.
      if (kw === 'FROM' && isPunct(tokens[j], ',')) {
        j++;
        continue;
      }
      break;
    }
  }
  return refs;
}

const sameName = (a: string, b: string): boolean => a.toLowerCase() === b.toLowerCase();

/** The catalog table a reference points to (unqualified names prefer the default schema). */
export function resolveTable(
  catalog: Catalog,
  schema: string | null,
  name: string,
): CatalogTable | null {
  const named = catalog.tables.filter((t) => sameName(t.name, name));
  if (schema !== null)
    return named.find((t) => t.schema !== null && sameName(t.schema, schema)) ?? null;
  const preferred = catalog.defaultSchema
    ? named.find((t) => t.schema !== null && sameName(t.schema, catalog.defaultSchema!))
    : undefined;
  return preferred ?? named[0] ?? null;
}

// -- completion context -----------------------------------------------------------------------

export interface CompletionContext {
  text: string;
  /** Cursor, UTF-16 index. */
  pos: number;
  catalog: Catalog;
  vocabulary?: CompletionVocabulary;
  /** `[name]` quotes identifiers (T-SQL). */
  bracketQuotes?: boolean;
}

function quoteIfNeeded(name: string): string {
  return /^[a-z_][a-z0-9_$]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`;
}

function tableOption(t: CatalogTable): CompletionOption {
  return {
    label: t.name,
    type: t.kind === 'view' || t.kind === 'materializedView' ? 'view' : 'table',
    detail: t.schema ?? undefined,
    apply: quoteIfNeeded(t.name),
    boost: 5,
  };
}

function columnOptions(t: CatalogTable, boost: number): CompletionOption[] {
  return t.columns.map((c) => ({
    label: c.name,
    type: 'column' as const,
    detail: `${t.name} · ${c.typeName}`,
    apply: quoteIfNeeded(c.name),
    boost,
  }));
}

/**
 * Table names the statement at `pos` mentions (aliases resolved), so the caller can load their
 * columns before asking for completions. Names come back as written.
 */
export function referencedTables(text: string, pos: number, bracketQuotes = false): TableRef[] {
  return parseTableRefs(statementTokens(tokenize(text, bracketQuotes), pos));
}

/**
 * Completions at `pos`, or `null` when none apply (inside a string or comment).
 * - `alias.` / `table.` lists the columns of that table (aliases come from the statement);
 * - `schema.` lists the tables of that schema;
 * - after FROM / JOIN / INTO / UPDATE it lists tables;
 * - elsewhere: columns of the statement's tables, then tables, keywords and functions.
 */
export function completionsAt(ctx: CompletionContext): CompletionResult | null {
  const { text, pos, catalog } = ctx;
  const tokens = tokenize(text, ctx.bracketQuotes);

  // Inside a string or comment: no completion. A closed literal ends before `pos`.
  for (const t of tokens) {
    if ((t.kind === 'string' || t.kind === 'comment') && t.from < pos && (pos < t.to || t.open)) {
      return null;
    }
    if (t.from >= pos) break;
  }

  // Word being typed (an unterminated quoted identifier counts, without its quote).
  let from = pos;
  while (from > 0 && WORD_PART.test(text[from - 1]!)) from--;
  const quoted = tokens.find((t) => t.kind === 'quoted' && t.open && t.from < pos);
  if (quoted) from = quoted.from + 1;

  const before = tokens.filter((t) => t.to <= (quoted ? quoted.from : from));
  const stmt = statementTokens(tokens, pos);
  const refs = parseTableRefs(stmt);

  // `qualifier.` / `schema.table.`
  const last = before[before.length - 1];
  if (isPunct(last, '.') && last!.to === (quoted ? quoted.from : from)) {
    const q1 = before[before.length - 2];
    if (!isIdent(q1)) return { from, options: [] };
    const dotBefore = before[before.length - 3];
    const q0 = before[before.length - 4];
    const schemaQualifier = isPunct(dotBefore, '.') && isIdent(q0) ? q0.value : null;
    return { from, options: qualifiedOptions(ctx, refs, schemaQualifier, q1.value) };
  }

  const prev = before[before.length - 1];
  const afterTableKeyword =
    prev !== undefined &&
    prev.kind === 'word' &&
    ['FROM', 'JOIN', 'INTO', 'UPDATE', 'TABLE'].includes(prev.value.toUpperCase());
  const options: CompletionOption[] = [];
  const seen = new Set<string>();
  const push = (o: CompletionOption) => {
    const key = `${o.type}:${o.label.toLowerCase()}`;
    if (!seen.has(key)) {
      seen.add(key);
      options.push(o);
    }
  };

  if (!afterTableKeyword) {
    for (const r of refs) {
      const t = resolveTable(catalog, r.schema, r.name);
      if (t) columnOptions(t, 10).forEach(push);
    }
    for (const r of refs) {
      if (r.alias) push({ label: r.alias, type: 'alias', detail: r.name, boost: 4 });
    }
  }
  for (const t of catalog.tables) push(tableOption(t));
  const schemas = new Set(catalog.tables.map((t) => t.schema).filter((s): s is string => !!s));
  for (const s of schemas) push({ label: s, type: 'schema', boost: afterTableKeyword ? 4 : 1 });
  if (!afterTableKeyword) {
    for (const k of ctx.vocabulary?.keywords ?? []) push({ label: k, type: 'keyword', boost: -2 });
    for (const f of ctx.vocabulary?.functions ?? []) {
      push({ label: f, type: 'function', apply: `${f}()`, boost: -1 });
    }
  }
  return { from, options };
}

function qualifiedOptions(
  ctx: CompletionContext,
  refs: TableRef[],
  schemaQualifier: string | null,
  qualifier: string,
): CompletionOption[] {
  const { catalog } = ctx;
  if (schemaQualifier !== null) {
    const t = resolveTable(catalog, schemaQualifier, qualifier);
    return t ? columnOptions(t, 10) : [];
  }
  // Alias or table name used in the statement.
  const ref =
    refs.find((r) => r.alias !== null && sameName(r.alias, qualifier)) ??
    refs.find((r) => r.alias === null && sameName(r.name, qualifier));
  const refTable = ref ? resolveTable(catalog, ref.schema, ref.name) : null;
  if (refTable) return columnOptions(refTable, 10);
  if (ref?.alias) return []; // an alias of something the catalog does not know
  // A table of the catalog (`users.|` without FROM yet), else a schema: its tables.
  const direct = resolveTable(catalog, null, qualifier);
  if (direct) return columnOptions(direct, 10);
  return catalog.tables
    .filter((t) => t.schema !== null && sameName(t.schema, qualifier))
    .map(tableOption);
}

// -- console modes (Mongo shell / Redis CLI) ---------------------------------------------------

export interface CommandHintLike {
  name: string;
  syntax: string;
  summary: string;
}

/** Operators offered after `$` in a Mongo console (query, update and aggregation stages). */
export const MONGO_OPERATORS: readonly string[] = [
  '$eq',
  '$ne',
  '$gt',
  '$gte',
  '$lt',
  '$lte',
  '$in',
  '$nin',
  '$and',
  '$or',
  '$not',
  '$nor',
  '$exists',
  '$type',
  '$regex',
  '$expr',
  '$elemMatch',
  '$size',
  '$all',
  '$set',
  '$unset',
  '$inc',
  '$push',
  '$pull',
  '$addToSet',
  '$pop',
  '$rename',
  '$match',
  '$group',
  '$project',
  '$sort',
  '$limit',
  '$skip',
  '$lookup',
  '$unwind',
  '$count',
  '$addFields',
  '$facet',
  '$out',
  '$merge',
  '$sum',
  '$avg',
  '$min',
  '$max',
  '$first',
  '$last',
];

/** Collection methods offered after `db.<collection>.` in a Mongo console. */
export const MONGO_METHODS: readonly string[] = [
  'find',
  'findOne',
  'aggregate',
  'countDocuments',
  'distinct',
  'insertOne',
  'insertMany',
  'updateOne',
  'updateMany',
  'replaceOne',
  'deleteOne',
  'deleteMany',
  'createIndex',
  'drop',
];

export interface ConsoleContext {
  mode: 'mongo' | 'redis';
  text: string;
  pos: number;
  /** Command vocabulary of the session (`script_vocabulary`): Redis commands. */
  commands: readonly CommandHintLike[];
  /** Collection names for `db.<collection>` (Mongo). */
  collections: readonly string[];
}

/** Completions for console modes; `null` when nothing applies. */
export function consoleCompletionsAt(ctx: ConsoleContext): CompletionResult | null {
  const { text, pos } = ctx;
  let from = pos;
  while (from > 0 && /[\w$]/.test(text[from - 1]!)) from--;
  const lineStart = text.lastIndexOf('\n', pos - 1) + 1;
  const linePrefix = text.slice(lineStart, from);

  if (ctx.mode === 'redis') {
    if (linePrefix.trim() !== '') return null; // only the command name is completed
    return {
      from,
      options: ctx.commands.map((c) => ({
        label: c.name,
        type: 'function' as const,
        detail: c.syntax,
        boost: 1,
      })),
    };
  }
  // Mongo: `db.|`, `db.users.|`, `$op`.
  if (text[from - 1] === '$' || text.slice(from, pos).startsWith('$')) {
    const start = text[from - 1] === '$' ? from - 1 : from;
    return {
      from: start,
      options: MONGO_OPERATORS.map((o) => ({ label: o, type: 'keyword' as const, boost: 1 })),
    };
  }
  const m = /db\.([\w$]*)\.$/.exec(linePrefix);
  if (m) {
    return {
      from,
      options: MONGO_METHODS.map((n) => ({ label: n, type: 'function' as const, apply: `${n}(` })),
    };
  }
  if (/db\.$/.test(linePrefix)) {
    return {
      from,
      options: ctx.collections.map((c) => ({ label: c, type: 'table' as const, boost: 5 })),
    };
  }
  return null;
}
