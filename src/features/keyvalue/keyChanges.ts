import type { KeyChange, ChangeSet, KeyEntries, ObjectPath, Value } from '@/lib/types';
import { databaseOfKeyPath } from './keyDetail';
import { textValue } from './valueFormat';

/**
 * Order list edits that address elements by index. Every index refers to the list as it was
 * loaded, but the backend applies changes one after the other: removing index 2 and then index 3
 * would remove the wrong element. So: value replacements first, then removals from the highest
 * index down (each removed index once, a replaced-then-removed index only removed, an index
 * replaced twice written once), then pushes
 * (a head push shifts every index). Other changes keep their relative order.
 */
export function normalizeListChanges(changes: KeyChange[]): KeyChange[] {
  const other: KeyChange[] = [];
  const sets: Extract<KeyChange, { op: 'listSet' }>[] = [];
  const removes: Extract<KeyChange, { op: 'listRemoveAt' }>[] = [];
  const pushes: KeyChange[] = [];
  for (const c of changes) {
    if (c.op === 'listSet') sets.push(c);
    else if (c.op === 'listRemoveAt') removes.push(c);
    else if (c.op === 'listPush') pushes.push(c);
    else other.push(c);
  }
  const uniqueRemoves = removes
    .filter((r, i) => removes.findIndex((x) => x.key === r.key && x.index === r.index) === i)
    .sort((a, b) => (a.key === b.key ? b.index - a.index : a.key < b.key ? -1 : 1));
  // An element edited twice is written once: the last value, checked against what was loaded
  // (the `expected` of the first edit), since the second edit would otherwise see the first.
  const mergedSets: typeof sets = [];
  for (const s of sets) {
    const at = mergedSets.findIndex((x) => x.key === s.key && x.index === s.index);
    if (at < 0) mergedSets.push(s);
    else mergedSets[at] = { ...s, expected: mergedSets[at]!.expected };
  }
  const keptSets = mergedSets.filter(
    (s) => !uniqueRemoves.some((r) => r.key === s.key && r.index === s.index),
  );
  return [...other, ...keptSets, ...uniqueRemoves, ...pushes];
}

/** Wrap key changes into the `ChangeSet` the backend applies (Redis is never atomic). */
export function buildKeyChangeSet(path: ObjectPath, changes: KeyChange[]): ChangeSet {
  return {
    model: 'keys',
    database: databaseOfKeyPath(path),
    changes: normalizeListChanges(changes),
  };
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
/**
 * `expected` is the element the UI showed at `index`: the backend refuses the change when the
 * list holds something else there by now (another client pushed, popped or edited it).
 */
export const listSetChange = (
  key: string,
  index: number,
  value: Value,
  expected?: Value,
): KeyChange => ({
  op: 'listSet',
  key,
  index,
  value,
  ...(expected === undefined ? {} : { expected }),
});
export const listPushChange = (key: string, value: Value, head: boolean): KeyChange => ({
  op: 'listPush',
  key,
  values: [value],
  head,
});
export const listRemoveChange = (key: string, index: number, expected?: Value): KeyChange => ({
  op: 'listRemoveAt',
  key,
  index,
  ...(expected === undefined ? {} : { expected }),
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

/** Append `next` to `prev`, dropping items whose identity was already loaded (SCAN may repeat). */
function appendUnique<T>(prev: T[], next: T[], identity: (item: T) => string): T[] {
  const seen = new Set(prev.map(identity));
  const added = next.filter((item) => {
    const id = identity(item);
    if (seen.has(id)) return false;
    seen.add(id);
    return true;
  });
  return [...prev, ...added];
}

const valueId = (v: Value): string => JSON.stringify(v);

/**
 * Append the entries of a later page to the ones already loaded. HSCAN/SSCAN/ZSCAN can return
 * the same element on several pages, so hash fields, set and sorted-set members are deduplicated.
 */
export function mergeEntries(prev: KeyEntries, next: KeyEntries): KeyEntries {
  if (prev.type === 'hash' && next.type === 'hash') {
    return { type: 'hash', fields: appendUnique(prev.fields, next.fields, ([f]) => valueId(f)) };
  }
  if (prev.type === 'list' && next.type === 'list') {
    return { type: 'list', start: prev.start, items: [...prev.items, ...next.items] };
  }
  if (prev.type === 'set' && next.type === 'set') {
    return { type: 'set', members: appendUnique(prev.members, next.members, valueId) };
  }
  if (prev.type === 'zSet' && next.type === 'zSet') {
    return { type: 'zSet', members: appendUnique(prev.members, next.members, ([m]) => valueId(m)) };
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
