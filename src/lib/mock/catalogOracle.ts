// Tiny fake Oracle tree for the browser mock (names only; no table data).

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

/** Children of `path`: schemas -> table/package/synonym/type folders -> leaves. */
export function oracleChildren(path: ObjectPath): ObjectNode[] {
  const last = lastSegment(path);
  if (!last) return ['HR', 'SCOTT'].map((s) => node(ROOT_PATH, 'schema', s, true));
  switch (last.kind) {
    case 'schema':
      return ['tables', 'packages', 'synonyms', 'types'].map((f) => node(path, 'folder', f, true));
    case 'folder':
      return last.name === 'packages' ? [node(path, 'package', 'PKG_HR', false)] : [];
    default:
      return [];
  }
}

export function oracleDetails(path: ObjectPath): TableDetails {
  void path;
  throw ipcError('unsupported', 'table details are not available in the browser mock');
}

export function oracleDdl(path: ObjectPath): string {
  return `-- DDL is not available for ${lastSegment(path)?.name ?? ''}`;
}
