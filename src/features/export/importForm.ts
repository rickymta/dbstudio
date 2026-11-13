// Pure logic of the import wizard: file kind, type suggestions, mapping, `ImportSpec`.

import type {
  ColumnMapping,
  ImportFormat,
  ImportPreview,
  ImportSpec,
  NewColumn,
  ObjectPath,
  OnConflict,
  ScriptDialect,
  TextEncoding,
  Transform,
  Uuid,
  ValueKind,
} from '@/lib/types';

export type ImportKind = 'csv' | 'xlsx' | 'json' | 'sql';
export type TargetMode = 'existing' | 'new' | 'script';

/** A file column and where it goes (`null` target = skipped). */
export interface MappingRow {
  source: string;
  target: string | null;
  transform: Transform;
}

export interface ImportForm {
  file: string;
  kind: ImportKind;
  csv: { delimiter: string; quote: string; header: boolean; encoding: TextEncoding };
  xlsx: { sheet: string; header: boolean };
  json: { layout: 'array' | 'ndjson' };
  sql: { continueOnError: boolean };
  targetMode: TargetMode;
  table: ObjectPath | null;
  newName: string;
  newColumns: NewColumn[];
  /** Per file column; used by `existing`. */
  mapping: MappingRow[];
  onConflict: OnConflict | null;
  mode: 'batches' | 'single';
  batchRows: number;
  /** 0 = stop at the first error. */
  maxErrors: number;
  database: string | null;
}

export function defaultImportForm(target: ObjectPath | undefined): ImportForm {
  return {
    file: '',
    kind: 'csv',
    csv: { delimiter: ',', quote: '"', header: true, encoding: 'utf8' },
    xlsx: { sheet: '', header: true },
    json: { layout: 'array' },
    sql: { continueOnError: false },
    targetMode: target ? 'existing' : 'new',
    table: target ?? null,
    newName: '',
    newColumns: [],
    mapping: [],
    onConflict: null,
    mode: 'batches',
    batchRows: 5000,
    maxErrors: 0,
    database: null,
  };
}

/** Kind of file from its extension (CSV when unknown). */
export function kindFromFile(path: string): ImportKind {
  const ext = path.split('.').pop()?.toLowerCase() ?? '';
  if (ext === 'xlsx' || ext === 'xlsm' || ext === 'xls') return 'xlsx';
  if (ext === 'json' || ext === 'ndjson' || ext === 'jsonl') return 'json';
  if (ext === 'sql') return 'sql';
  return 'csv';
}

/** JSON layout implied by the extension. */
export function layoutFromFile(path: string): 'array' | 'ndjson' {
  return /\.(ndjson|jsonl)$/i.test(path) ? 'ndjson' : 'array';
}

export function importFormat(form: ImportForm): ImportFormat {
  switch (form.kind) {
    case 'csv':
      return { kind: 'csv', ...form.csv };
    case 'xlsx':
      return { kind: 'xlsx', ...form.xlsx };
    case 'json':
      return { kind: 'json', ...form.json };
    case 'sql':
      return { kind: 'sql', ...form.sql };
  }
}

const TYPES: Record<string, Partial<Record<ValueKind | 'default', string>>> = {
  postgres: {
    int: 'bigint',
    uInt: 'bigint',
    float: 'double precision',
    decimal: 'numeric',
    bool: 'boolean',
    date: 'date',
    dateTime: 'timestamp',
    timestamp: 'timestamptz',
    uuid: 'uuid',
    json: 'jsonb',
    bytes: 'bytea',
    default: 'text',
  },
  mySql: {
    int: 'bigint',
    uInt: 'bigint',
    float: 'double',
    decimal: 'decimal(38,10)',
    bool: 'tinyint(1)',
    date: 'date',
    dateTime: 'datetime(6)',
    timestamp: 'datetime(6)',
    uuid: 'char(36)',
    json: 'json',
    bytes: 'longblob',
    default: 'longtext',
  },
  msSql: {
    int: 'bigint',
    uInt: 'bigint',
    float: 'float',
    decimal: 'decimal(38,10)',
    bool: 'bit',
    date: 'date',
    dateTime: 'datetime2',
    timestamp: 'datetimeoffset',
    uuid: 'uniqueidentifier',
    json: 'nvarchar(max)',
    bytes: 'varbinary(max)',
    default: 'nvarchar(max)',
  },
  oracle: {
    int: 'number(19)',
    uInt: 'number(19)',
    float: 'binary_double',
    decimal: 'number',
    bool: 'number(1)',
    date: 'date',
    dateTime: 'timestamp',
    timestamp: 'timestamp with time zone',
    uuid: 'varchar2(36)',
    json: 'clob',
    bytes: 'blob',
    default: 'clob',
  },
};

/** Column type to create for a column whose values look like `kind`. */
export function typeForKind(kind: ValueKind, dialect: ScriptDialect): string {
  const table = TYPES[dialect.name] ?? TYPES.postgres!;
  return table[kind] ?? table.default ?? 'text';
}

/** Identifier-friendly column name from a header (`Unit price` -> `unit_price`). */
export function columnName(header: string, index: number): string {
  const cleaned = header
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, '_')
    .replace(/^_+|_+$/g, '');
  const name = cleaned === '' || /^\d/.test(cleaned) ? `col_${index + 1}` : cleaned;
  return name;
}

/** Columns of a new table proposed from the preview (names made unique). */
export function newColumnsFromPreview(preview: ImportPreview, dialect: ScriptDialect): NewColumn[] {
  const used = new Set<string>();
  return preview.columns.map((header, i) => {
    let name = columnName(header, i);
    while (used.has(name)) name = `${name}_${i + 1}`;
    used.add(name);
    return {
      name,
      typeName: typeForKind(preview.suggestedKinds[i] ?? 'text', dialect),
      nullable: true,
      primaryKey: false,
    };
  });
}

/** Map file columns to table columns with the same name (case-insensitive); others are skipped. */
export function autoMapping(fileColumns: string[], tableColumns: string[]): MappingRow[] {
  const byName = new Map(tableColumns.map((c) => [c.toLowerCase(), c]));
  const taken = new Set<string>();
  return fileColumns.map((source) => {
    const hit = byName.get(source.trim().toLowerCase());
    const target = hit && !taken.has(hit) ? hit : null;
    if (target) taken.add(target);
    return { source, target, transform: 'none' as Transform };
  });
}

/** Column mappings of the spec for the current target. */
export function mappingFor(form: ImportForm, fileColumns: string[]): ColumnMapping[] {
  if (form.targetMode === 'new') {
    return form.newColumns.flatMap((c, i) => {
      const source = fileColumns[i];
      return source === undefined || c.name === '' ? [] : [{ source, target: c.name }];
    });
  }
  return form.mapping
    .filter((m) => m.target !== null)
    .map((m) => ({ source: m.source, target: m.target as string, transform: m.transform }));
}

export function buildImportSpec(
  connectionId: Uuid,
  form: ImportForm,
  fileColumns: string[],
  parent: ObjectPath | null,
): ImportSpec {
  const base = {
    connectionId,
    file: form.file,
    format: importFormat(form),
    mode:
      form.mode === 'single'
        ? ({ kind: 'singleTransaction' } as const)
        : ({ kind: 'batches', rows: Math.max(1, form.batchRows) } as const),
    maxErrors: form.maxErrors,
  };
  if (form.kind === 'sql') {
    return {
      ...base,
      target: { kind: 'script', database: form.database },
      mode: { kind: 'batches', rows: 1000 },
    };
  }
  const target =
    form.targetMode === 'new'
      ? {
          kind: 'new' as const,
          parent: parent as ObjectPath,
          name: form.newName.trim(),
          columns: form.newColumns,
        }
      : { kind: 'existing' as const, table: form.table as ObjectPath };
  return {
    ...base,
    target,
    mapping: mappingFor(form, fileColumns),
    onConflict: form.targetMode === 'existing' ? form.onConflict : null,
  };
}

export type ImportIssue =
  'noFile' | 'noTable' | 'noName' | 'noColumns' | 'noMapping' | 'duplicateTarget';

/** Problems that block starting the import. */
export function validateImport(form: ImportForm, parent: ObjectPath | null): ImportIssue[] {
  const issues: ImportIssue[] = [];
  if (form.file === '') issues.push('noFile');
  if (form.kind === 'sql') return issues;
  if (form.targetMode === 'existing') {
    if (!form.table) issues.push('noTable');
    const targets = form.mapping.map((m) => m.target).filter((t): t is string => t !== null);
    if (targets.length === 0) issues.push('noMapping');
    if (new Set(targets).size !== targets.length) issues.push('duplicateTarget');
  } else {
    if (!parent) issues.push('noTable');
    if (form.newName.trim() === '') issues.push('noName');
    if (form.newColumns.length === 0) issues.push('noColumns');
    const names = form.newColumns.map((c) => c.name);
    if (names.some((n) => n === '') || new Set(names).size !== names.length) {
      issues.push('duplicateTarget');
    }
  }
  return issues;
}
