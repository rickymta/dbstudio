// Mock data editing. Tables of the mock PostgreSQL/MySQL servers with a primary key are
// editable; `changes_preview` renders ANSI SQL and `changes_apply` validates the change set but
// stores nothing (the mock rows are a pure function of the row index). A text value `__fail__`
// makes the change that carries it fail, like a constraint violation would.

import type {
  ApplyResult,
  CellEdit,
  ChangeSet,
  EditValue,
  Editability,
  ObjectPath,
  RowChange,
  Uuid,
  Value,
} from '../types';
import { lastSegment } from '../objectPath';
import { findRelation } from './catalog';
import { mockSessionKind } from './sessions';
import { ipcError, sleep } from './util';

const NOT_EDITABLE = (reason: Editability['reason']): Editability => ({
  editable: false,
  keyColumns: [],
  readOnlyColumns: [],
  reason,
  atomic: true,
});

const quote = (name: string) => `"${name.replace(/"/g, '""')}"`;
const text = (s: string) => `'${s.replace(/'/g, "''")}'`;

function literal(v: Value): string {
  switch (v.t) {
    case 'null':
      return 'NULL';
    case 'bool':
      return v.v ? 'true' : 'false';
    case 'int':
    case 'uInt':
    case 'float':
      return String(v.v);
    case 'decimal':
      return v.v;
    case 'array':
    case 'document':
      return 'NULL';
    case 'ext':
      return text(v.v.text);
    default:
      return text(String(v.v));
  }
}

function editLiteral(e: EditValue): string {
  return e.kind === 'default' ? 'DEFAULT' : e.kind === 'text' ? text(e.v) : literal(e.v);
}

function where(keyColumns: string[], key: Value[]): string {
  return keyColumns
    .map((c, i) => {
      const v = key[i];
      return v && v.t !== 'null' ? `${quote(c)} = ${literal(v)}` : `${quote(c)} IS NULL`;
    })
    .join(' AND ');
}

/** Deletes, then updates, then inserts: the order the server applies them. */
function ordered(changes: RowChange[]): { change: RowChange; index: number }[] {
  const all = changes.map((change, index) => ({ change, index }));
  return (['delete', 'update', 'insert'] as const).flatMap((op) =>
    all.filter((x) => x.change.op === op),
  );
}

function render(table: string, keyColumns: string[], change: RowChange): string {
  switch (change.op) {
    case 'delete':
      return `DELETE FROM ${table} WHERE ${where(keyColumns, change.key)};`;
    case 'update': {
      const set = change.set.map((c: CellEdit) => `${quote(c.column)} = ${editLiteral(c.value)}`);
      return `UPDATE ${table} SET ${set.join(', ')} WHERE ${where(keyColumns, change.key)};`;
    }
    case 'insert': {
      if (change.values.length === 0) return `INSERT INTO ${table} DEFAULT VALUES;`;
      const cols = change.values.map((c) => quote(c.column)).join(', ');
      const vals = change.values.map((c) => editLiteral(c.value)).join(', ');
      return `INSERT INTO ${table} (${cols}) VALUES (${vals});`;
    }
  }
}

function carriesFailMarker(change: RowChange): boolean {
  const cells = change.op === 'update' ? change.set : change.op === 'insert' ? change.values : [];
  return cells.some((c) => c.value.kind === 'text' && c.value.v === '__fail__');
}

export const changesMock = {
  async tableEditability(sessionId: Uuid, path: ObjectPath): Promise<Editability> {
    const kind = mockSessionKind(sessionId);
    await sleep(20, 60);
    const rel = findRelation(kind, path);
    if (!rel) return NOT_EDITABLE('unsupported');
    if (rel.table.kind !== 'table') return NOT_EDITABLE('notATable');
    const keyColumns = rel.table.columns.filter((c) => c.pk).map((c) => c.name);
    if (keyColumns.length === 0) return NOT_EDITABLE('noUniqueKey');
    return {
      editable: true,
      keyColumns,
      readOnlyColumns: rel.table.columns
        .filter((c) => c.extra?.includes('generated') || c.extra?.includes('identity'))
        .map((c) => c.name),
      reason: null,
      atomic: true,
    };
  },

  async changesPreview(sessionId: Uuid, changes: ChangeSet): Promise<string[]> {
    mockSessionKind(sessionId);
    if (changes.model !== 'rows') throw ipcError('unsupported', 'data_editing');
    const name = lastSegment(changes.table)?.name ?? 'table';
    const table = quote(name);
    return ordered(changes.changes).map(({ change }) => render(table, changes.keyColumns, change));
  },

  async changesApply(sessionId: Uuid, changes: ChangeSet): Promise<ApplyResult> {
    mockSessionKind(sessionId);
    if (changes.model !== 'rows') throw ipcError('unsupported', 'data_editing');
    // Like the real drivers: a table that cannot be edited is refused.
    const ed = await changesMock.tableEditability(sessionId, changes.table);
    if (!ed.editable) throw ipcError('unsupported', 'data_editing', { capability: 'data_editing' });
    await sleep(60, 160);
    const steps = ordered(changes.changes);
    const bad = steps.findIndex(({ change }) => carriesFailMarker(change));
    if (bad >= 0) {
      throw ipcError('changeFailed', 'constraint violation (mock)', {
        index: steps[bad]!.index,
        applied: 0,
      });
    }
    return { applied: steps.length, affected: steps.map(() => 1), atomic: true };
  },
};
