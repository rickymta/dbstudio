// Pure logic of the export wizard: form state <-> `ExportSpec`, output file naming, validation.

import type {
  BinaryFormat,
  CsvOptions,
  ExportContent,
  ExportFormat,
  ExportSource,
  ExportSpec,
  FileFilter,
  JsonOptions,
  ObjectPath,
  ScriptDialect,
  SqlOptions,
  Uuid,
  XlsxOptions,
} from '@/lib/types';

export type FormatKind = ExportFormat['kind'];
export const FORMAT_KINDS: readonly FormatKind[] = ['csv', 'xlsx', 'json', 'sql'];

/** Everything the wizard edits. */
export interface ExportForm {
  source: ExportSource;
  content: ExportContent;
  format: FormatKind;
  sql: SqlOptions;
  xlsx: XlsxOptions;
  csv: CsvOptions;
  json: JsonOptions;
  binary: BinaryFormat;
  /** Output file chosen in the save dialog. */
  path: string;
}

export function defaultForm(source: ExportSource): ExportForm {
  return {
    source,
    content: source.kind === 'query' ? 'data' : 'both',
    format: source.kind === 'query' ? 'csv' : 'sql',
    sql: {
      ddl: { qualifySchema: true },
      insertBatchRows: 100,
      wrapInTransaction: true,
      disableFkChecks: false,
      includeColumnNames: true,
    },
    xlsx: { dataDictionary: false, header: true, freezeHeader: true },
    csv: {
      delimiter: ',',
      quote: '"',
      quoteAll: false,
      header: true,
      encoding: 'utf8Bom',
      lineEnding: 'crlf',
      nullText: '',
    },
    json: { layout: 'array', extendedJson: 'off', pretty: false },
    binary: 'base64',
    path: '',
  };
}

/** Source that follows from the node the wizard was opened on. */
export function sourceFromPath(path: ObjectPath | undefined, query?: string): ExportSource {
  if (!path) return { kind: 'query', text: query ?? '', database: null };
  const last = path.segments[path.segments.length - 1];
  switch (last?.kind) {
    case 'database':
      return { kind: 'database', path };
    case 'schema':
      return { kind: 'schema', path };
    default:
      return { kind: 'tables', paths: [path] };
  }
}

/** SQL scripts only make sense for SQL databases. */
export function formatsFor(dialect: ScriptDialect): FormatKind[] {
  const sql = ['postgres', 'mySql', 'msSql', 'oracle'].includes(dialect.name);
  return FORMAT_KINDS.filter((f) => f !== 'sql' || sql);
}

export function buildFormat(form: ExportForm): ExportFormat {
  switch (form.format) {
    case 'sql':
      return { kind: 'sql', ...form.sql };
    case 'xlsx':
      return { kind: 'xlsx', ...form.xlsx };
    case 'csv':
      return { kind: 'csv', ...form.csv };
    case 'json':
      return { kind: 'json', ...form.json };
  }
}

/** The spec to send. `overwrite` is true: the save dialog already asked about replacing. */
export function buildSpec(connectionId: Uuid, form: ExportForm): ExportSpec {
  return {
    connectionId,
    source: form.source,
    content: form.source.kind === 'query' ? 'data' : form.content,
    format: buildFormat(form),
    destination: { path: form.path, overwrite: true },
    values: { binary: form.binary },
  };
}

/** Form restored from a saved preset's spec (`null` when it is not an export spec). */
export function formFromSpec(spec: unknown, base: ExportForm): ExportForm | null {
  if (typeof spec !== 'object' || spec === null) return null;
  const s = spec as Partial<ExportSpec>;
  if (!s.format || !s.content) return null;
  const next: ExportForm = { ...base, content: s.content, format: s.format.kind };
  const { kind, ...options } = s.format;
  (next as unknown as Record<string, unknown>)[kind] = {
    ...(base[kind] as object),
    ...options,
  };
  if (s.values?.binary) next.binary = s.values.binary;
  return next;
}

/** How many tables the export covers; `null` when it depends on the database content. */
export function tableCount(source: ExportSource): number | null {
  if (source.kind === 'tables') return source.paths.length;
  if (source.kind === 'query') return 1;
  return null;
}

/** Several files are produced (CSV / JSON): the output is a zip archive. */
export function needsZip(form: ExportForm): boolean {
  if (form.format !== 'csv' && form.format !== 'json') return false;
  if (form.source.kind === 'query') return false;
  const tables = tableCount(form.source);
  const data = form.content === 'schema' ? 0 : (tables ?? 2);
  return data + (form.content === 'data' ? 0 : 1) > 1;
}

export function extensionFor(form: ExportForm): string {
  if (needsZip(form)) return 'zip';
  if (form.format === 'json') return form.json.layout === 'ndjson' ? 'ndjson' : 'json';
  return form.format;
}

/** File name offered in the save dialog. */
export function suggestedName(form: ExportForm, label: string): string {
  const safe = label.replace(/[\\/:*?"<>|]+/g, '_').trim() || 'export';
  const suffix = form.content === 'schema' && form.format === 'sql' ? '-schema' : '';
  return `${safe}${suffix}.${extensionFor(form)}`;
}

export function saveFilters(
  form: ExportForm,
  names: { zip: string; [k: string]: string },
): FileFilter[] {
  const ext = extensionFor(form);
  const name = ext === 'zip' ? names.zip : (names[form.format] ?? ext.toUpperCase());
  return [{ name, extensions: [ext] }];
}

export type FormIssue =
  'noTables' | 'noQuery' | 'noPath' | 'delimiterLength' | 'quoteLength' | 'delimiterEqualsQuote';

/** Problems that block starting the export. */
export function validate(form: ExportForm): FormIssue[] {
  const issues: FormIssue[] = [];
  if (form.source.kind === 'tables' && form.source.paths.length === 0) issues.push('noTables');
  if (form.source.kind === 'query' && form.source.text.trim() === '') issues.push('noQuery');
  if (form.format === 'csv') {
    const { delimiter = ',', quote = '"' } = form.csv;
    if ([...delimiter].length !== 1) issues.push('delimiterLength');
    if ([...quote].length !== 1) issues.push('quoteLength');
    if (delimiter === quote) issues.push('delimiterEqualsQuote');
  }
  if (form.path === '') issues.push('noPath');
  return issues;
}

/** Message of whatever a failed IPC call threw. */
export function ipcMessage(e: unknown): string {
  if (typeof e === 'object' && e !== null && 'message' in e)
    return String((e as { message: unknown }).message);
  return String(e);
}
