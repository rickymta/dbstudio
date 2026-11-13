// Tiny fake SQL Server tree for the browser mock (names only; no table data).

import { childPath, lastSegment, ROOT_PATH } from '../objectPath';
import type { ObjectKind, ObjectNode, ObjectPath, TableDetails } from '../types';
import { ipcError } from './util';

const node = (
  parent: ObjectPath,
  kind: ObjectKind,
  name: string,
  hasChildren: boolean,
  detail?: string,
): ObjectNode => ({
  path: childPath(parent, kind, name),
  kind,
  name,
  hasChildren,
  detail: detail ?? null,
});

/** Children of `path`: databases -> schemas -> table/synonym folders -> leaves. */
export function mssqlChildren(path: ObjectPath): ObjectNode[] {
  const last = lastSegment(path);
  if (!last) return ['master', 'app'].map((d) => node(ROOT_PATH, 'database', d, true));
  switch (last.kind) {
    case 'database':
      return [node(path, 'folder', 'schemas', true)];
    case 'folder':
      return last.name === 'schemas' ? [node(path, 'schema', 'dbo', true)] : [];
    case 'schema':
      return [node(path, 'folder', 'tables', true), node(path, 'folder', 'synonyms', true)];
    default:
      return [];
  }
}

export function mssqlDetails(path: ObjectPath): TableDetails {
  void path;
  throw ipcError('unsupported', 'table details are not available in the browser mock');
}

export function mssqlDdl(path: ObjectPath): string {
  return `-- DDL is not available for ${lastSegment(path)?.name ?? ''}`;
}
