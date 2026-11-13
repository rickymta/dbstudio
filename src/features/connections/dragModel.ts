import type { ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, Uuid } from '@/lib/types';

/** Something being dragged in the connections sidebar. */
export type DragItem = { type: 'profile' | 'group'; id: Uuid };

export type DropPosition = 'before' | 'into' | 'after';

/** Where an item is dropped: onto a row (with a position) or into the root/a group directly. */
export type DropTarget =
  { type: 'profile' | 'group'; id: Uuid; position: DropPosition } | { type: 'root' };

export interface MovePlan {
  profiles: ConnectionProfile[];
  groups: ConnectionGroup[];
}

/** Which part of a row the pointer is over, given what is being dragged onto what. */
export function dropPosition(
  dragged: DragItem['type'],
  rowType: 'profile' | 'group',
  offsetRatio: number,
): DropPosition | null {
  const ratio = Number.isFinite(offsetRatio) ? offsetRatio : 0.5;
  if (rowType === 'profile') {
    // Only profiles can be ordered next to profiles.
    if (dragged !== 'profile') return null;
    return ratio < 0.5 ? 'before' : 'after';
  }
  // Group rows: a profile always goes into the group; a group can also be ordered among groups.
  if (dragged === 'profile') return 'into';
  if (ratio < 0.25) return 'before';
  if (ratio > 0.75) return 'after';
  return 'into';
}

/** True when `groupId` is `ancestorId` itself or lies below it. */
export function isSameOrDescendant(
  groups: ConnectionGroup[],
  ancestorId: Uuid,
  groupId: Uuid | null,
): boolean {
  const byId = new Map(groups.map((g) => [g.id, g]));
  const seen = new Set<Uuid>();
  let current = groupId;
  while (current && !seen.has(current)) {
    if (current === ancestorId) return true;
    seen.add(current);
    current = byId.get(current)?.parentId ?? null;
  }
  return false;
}

const bySortOrder = <T extends { sortOrder: number; name: string }>(a: T, b: T) =>
  a.sortOrder - b.sortOrder || a.name.localeCompare(b.name);

/**
 * Work out which profiles/groups to save for a drop. Returns `null` for an invalid or no-op drop
 * (onto itself, a group into its own subtree, unknown ids). Only items whose container or
 * `sortOrder` changes are returned.
 */
export function planMove(
  snapshot: ConnectionsSnapshot,
  dragged: DragItem,
  target: DropTarget,
): MovePlan | null {
  const { groups, profiles } = snapshot;
  const dragProfile = dragged.type === 'profile' ? profiles.find((p) => p.id === dragged.id) : null;
  const dragGroup = dragged.type === 'group' ? groups.find((g) => g.id === dragged.id) : null;
  if (!dragProfile && !dragGroup) return null;

  // Resolve the destination container and the sibling to insert before/after.
  let container: Uuid | null;
  let anchor: { id: Uuid; after: boolean } | null = null;
  if (target.type === 'root') {
    container = null;
  } else if (target.position === 'into') {
    if (target.type !== 'group') return null;
    container = target.id;
  } else if (target.type === dragged.type) {
    if (target.id === dragged.id) return null;
    anchor = { id: target.id, after: target.position === 'after' };
    const sibling =
      target.type === 'group'
        ? groups.find((g) => g.id === target.id)?.parentId
        : profiles.find((p) => p.id === target.id)?.groupId;
    if (
      sibling === undefined &&
      !(target.type === 'group' ? groups : profiles).some((x) => x.id === target.id)
    ) {
      return null;
    }
    container = sibling ?? null;
  } else {
    return null;
  }

  if (container && !groups.some((g) => g.id === container)) return null;
  if (dragGroup && isSameOrDescendant(groups, dragGroup.id, container)) return null;

  if (dragProfile) {
    const siblings = profiles
      .filter((p) => (p.groupId ?? null) === container && p.id !== dragProfile.id)
      .sort(bySortOrder);
    const ordered = insertAt(siblings, { ...dragProfile, groupId: container }, anchor);
    const changed = ordered
      .map((p, i) => ({ ...p, sortOrder: i }))
      .filter((p) => {
        const old = profiles.find((x) => x.id === p.id)!;
        return (old.groupId ?? null) !== (p.groupId ?? null) || old.sortOrder !== p.sortOrder;
      });
    return changed.length ? { profiles: changed, groups: [] } : null;
  }

  const moving = dragGroup!;
  const siblings = groups
    .filter((g) => (g.parentId ?? null) === container && g.id !== moving.id)
    .sort(bySortOrder);
  const ordered = insertAt(siblings, { ...moving, parentId: container }, anchor);
  const changed = ordered
    .map((g, i) => ({ ...g, sortOrder: i }))
    .filter((g) => {
      const old = groups.find((x) => x.id === g.id)!;
      return (old.parentId ?? null) !== (g.parentId ?? null) || old.sortOrder !== g.sortOrder;
    });
  return changed.length ? { profiles: [], groups: changed } : null;
}

/** Insert `item` before/after the anchor sibling, or at the end without an anchor. */
function insertAt<T extends { id: Uuid }>(
  siblings: T[],
  item: T,
  anchor: { id: Uuid; after: boolean } | null,
): T[] {
  const list = [...siblings];
  if (!anchor) return [...list, item];
  const index = list.findIndex((s) => s.id === anchor.id);
  if (index < 0) return [...list, item];
  list.splice(anchor.after ? index + 1 : index, 0, item);
  return list;
}
