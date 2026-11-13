import type { KeyChange, ChangeSet, KeyEntries, ObjectPath, Value } from '@/lib/types';
import { databaseOfKeyPath } from './keyDetail';
import { textValue } from './valueFormat';

/** Wrap key changes into the `ChangeSet` the backend applies (Redis is never atomic). */
export function buildKeyChangeSet(path: ObjectPath, changes: KeyChange[]): ChangeSet {
  return { model: 'keys', database: databaseOfKeyPath(path), changes };
}

export const setStringChange = (key: string, value: Value, ttlMs?: number | null): KeyChange =>
  ttlMs === undefined ? { op: 'setString', key, value } : { op: 'setString', key, value, ttlMs };
export const hashSetChange = (key: string, field: Value, value: Value): KeyChange => ({
  op: 'hashSet',
  key,
  entries: [[field, value]],
});
export const hashDeleteChange = (key: string, field: Value): KeyChange => ({
  op: 'hashDelete',
  key,
  fields: [field],
});
export const listSetChange = (key: string, index: number, value: Value): KeyChange => ({
  op: 'listSet',
  key,
  index,
  value,
});
export const listPushChange = (key: string, value: Value, head: boolean): KeyChange => ({
  op: 'listPush',
  key,
  values: [value],
  head,
});
export const listRemoveChange = (key: string, index: number): KeyChange => ({
  op: 'listRemoveAt',
  key,
  index,
});
export const setAddChange = (key: string, member: Value): KeyChange => ({
  op: 'setAdd',
  key,
  members: [member],
});
export const setRemoveChange = (key: string, member: Value): KeyChange => ({
  op: 'setRemove',
  key,
  members: [member],
});
export const zAddChange = (key: string, member: Value, score: string): KeyChange => ({
  op: 'zAdd',
  key,
  members: [[member, score.trim()]],
});
export const zRemoveChange = (key: string, member: Value): KeyChange => ({
  op: 'zRemove',
  key,
  members: [member],
});
export const streamAddChange = (
  key: string,
  id: string | null,
  fields: [Value, Value][],
): KeyChange => ({ op: 'streamAdd', key, id: id && id.trim() ? id.trim() : null, fields });
export const streamDeleteChange = (key: string, id: string): KeyChange => ({
  op: 'streamDelete',
  key,
  ids: [id],
});
export const expireChange = (key: string, ttlMs: number | null): KeyChange => ({
  op: 'expire',
  key,
  ttlMs,
});
export const renameChange = (key: string, newKey: string): KeyChange => ({
  op: 'rename',
  key,
  newKey,
});
export const deleteKeyChange = (key: string): KeyChange => ({ op: 'delete', keys: [key] });

/** Parse `field=value` lines (stream fields); `null` when a line has no `=`. */
export function parseFieldLines(text: string): [Value, Value][] | null {
  const out: [Value, Value][] = [];
  for (const raw of text.split('\n')) {
    const line = raw.trim();
    if (!line) continue;
    const eq = line.indexOf('=');
    if (eq <= 0) return null;
    out.push([textValue(line.slice(0, eq).trim()), textValue(line.slice(eq + 1))]);
  }
  return out.length > 0 ? out : null;
}

/** Append the entries of a later page to the ones already loaded. */
export function mergeEntries(prev: KeyEntries, next: KeyEntries): KeyEntries {
  if (prev.type === 'hash' && next.type === 'hash') {
    return { type: 'hash', fields: [...prev.fields, ...next.fields] };
  }
  if (prev.type === 'list' && next.type === 'list') {
    return { type: 'list', start: prev.start, items: [...prev.items, ...next.items] };
  }
  if (prev.type === 'set' && next.type === 'set') {
    return { type: 'set', members: [...prev.members, ...next.members] };
  }
  if (prev.type === 'zSet' && next.type === 'zSet') {
    return { type: 'zSet', members: [...prev.members, ...next.members] };
  }
  if (prev.type === 'stream' && next.type === 'stream') {
    return { type: 'stream', entries: [...prev.entries, ...next.entries], groups: next.groups };
  }
  return next;
}

/** Number of loaded elements (for the "n of total" counter). */
export function entryCount(entries: KeyEntries): number {
  switch (entries.type) {
    case 'hash':
      return entries.fields.length;
    case 'list':
      return entries.items.length;
    case 'set':
    case 'zSet':
      return entries.members.length;
    case 'stream':
      return entries.entries.length;
    default:
      return 0;
  }
}
