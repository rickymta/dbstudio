import { describe, expect, it } from 'vitest';
import type { ConnectionGroup, ConnectionProfile, ConnectionsSnapshot } from '@/lib/types';
import { initialForm, profileFromForm } from './connectionForm';
import { dropPosition, isSameOrDescendant, planMove } from './dragModel';

const g = (id: string, parentId: string | null, sortOrder: number): ConnectionGroup => ({
  id,
  name: id.toUpperCase(),
  parentId,
  sortOrder,
});

function p(id: string, groupId: string | null, sortOrder: number): ConnectionProfile {
  return {
    ...profileFromForm({ ...initialForm(undefined), name: id }, undefined, () => id),
    groupId,
    sortOrder,
  };
}

// root: groups a(0), b(1); profiles r0(0), r1(1)
// a: child group c(0); profiles a0(0), a1(1)
// c: profile c0
const snapshot: ConnectionsSnapshot = {
  groups: [g('a', null, 0), g('b', null, 1), g('c', 'a', 0)],
  profiles: [p('r0', null, 0), p('r1', null, 1), p('a0', 'a', 0), p('a1', 'a', 1), p('c0', 'c', 0)],
};

const ids = (items: { id: string; sortOrder: number }[]) =>
  items.map((i) => `${i.id}:${i.sortOrder}`).sort();

describe('dropPosition', () => {
  it('splits profile rows in halves and only accepts profiles', () => {
    expect(dropPosition('profile', 'profile', 0.2)).toBe('before');
    expect(dropPosition('profile', 'profile', 0.8)).toBe('after');
    expect(dropPosition('group', 'profile', 0.5)).toBeNull();
  });

  it('uses edges for ordering groups and the middle for nesting', () => {
    expect(dropPosition('group', 'group', 0.1)).toBe('before');
    expect(dropPosition('group', 'group', 0.5)).toBe('into');
    expect(dropPosition('group', 'group', 0.9)).toBe('after');
    expect(dropPosition('profile', 'group', 0.1)).toBe('into');
    expect(dropPosition('profile', 'group', Number.NaN)).toBe('into');
  });
});

describe('isSameOrDescendant', () => {
  it('detects subtree membership', () => {
    expect(isSameOrDescendant(snapshot.groups, 'a', 'c')).toBe(true);
    expect(isSameOrDescendant(snapshot.groups, 'a', 'a')).toBe(true);
    expect(isSameOrDescendant(snapshot.groups, 'c', 'a')).toBe(false);
    expect(isSameOrDescendant(snapshot.groups, 'a', null)).toBe(false);
  });
});

describe('planMove', () => {
  it('moves a profile into a group, appended at the end', () => {
    const plan = planMove(
      snapshot,
      { type: 'profile', id: 'r0' },
      { type: 'group', id: 'b', position: 'into' },
    )!;
    expect(plan.groups).toEqual([]);
    expect(plan.profiles.map((x) => [x.id, x.groupId, x.sortOrder])).toEqual([['r0', 'b', 0]]);
  });

  it('moves a profile out of its group to the root', () => {
    const plan = planMove(snapshot, { type: 'profile', id: 'a0' }, { type: 'root' })!;
    // Appended after r0, r1; only the moved profile changes.
    expect(plan.profiles.map((x) => [x.id, x.groupId, x.sortOrder])).toEqual([['a0', null, 2]]);
  });

  it('reorders profiles in the same container, renumbering only what changed', () => {
    const plan = planMove(
      snapshot,
      { type: 'profile', id: 'r1' },
      { type: 'profile', id: 'r0', position: 'before' },
    )!;
    expect(ids(plan.profiles)).toEqual(['r0:1', 'r1:0']);
    const after = planMove(
      snapshot,
      { type: 'profile', id: 'r0' },
      { type: 'profile', id: 'r1', position: 'after' },
    )!;
    expect(ids(after.profiles)).toEqual(['r0:1', 'r1:0']);
  });

  it('moves a profile next to a profile of another group', () => {
    const plan = planMove(
      snapshot,
      { type: 'profile', id: 'r0' },
      { type: 'profile', id: 'a1', position: 'before' },
    )!;
    expect(plan.profiles.map((x) => [x.id, x.groupId, x.sortOrder]).sort()).toEqual([
      ['a1', 'a', 2],
      ['r0', 'a', 1],
    ]);
  });

  it('is a no-op when nothing changes', () => {
    expect(
      planMove(
        snapshot,
        { type: 'profile', id: 'r0' },
        { type: 'profile', id: 'r1', position: 'before' },
      ),
    ).toBeNull();
    expect(
      planMove(
        snapshot,
        { type: 'profile', id: 'r0' },
        { type: 'profile', id: 'r0', position: 'after' },
      ),
    ).toBeNull();
    // Dropping onto its own group just sends it to the end of that group.
    const toEnd = planMove(
      snapshot,
      { type: 'profile', id: 'a0' },
      { type: 'group', id: 'a', position: 'into' },
    )!;
    expect(ids(toEnd.profiles)).toEqual(['a0:1', 'a1:0']);
  });

  it('nests and reorders groups', () => {
    const nest = planMove(
      snapshot,
      { type: 'group', id: 'b' },
      { type: 'group', id: 'a', position: 'into' },
    )!;
    expect(nest.groups.map((x) => [x.id, x.parentId, x.sortOrder])).toEqual([['b', 'a', 1]]);
    const reorder = planMove(
      snapshot,
      { type: 'group', id: 'b' },
      { type: 'group', id: 'a', position: 'before' },
    )!;
    expect(ids(reorder.groups)).toEqual(['a:1', 'b:0']);
    const out = planMove(snapshot, { type: 'group', id: 'c' }, { type: 'root' })!;
    expect(out.groups.map((x) => [x.id, x.parentId, x.sortOrder])).toEqual([['c', null, 2]]);
  });

  it('refuses to create parent-child loops', () => {
    expect(
      planMove(snapshot, { type: 'group', id: 'a' }, { type: 'group', id: 'a', position: 'into' }),
    ).toBeNull();
    expect(
      planMove(snapshot, { type: 'group', id: 'a' }, { type: 'group', id: 'c', position: 'into' }),
    ).toBeNull();
    // Ordering next to a descendant puts the group inside its own subtree as well.
    expect(
      planMove(
        snapshot,
        { type: 'group', id: 'a' },
        { type: 'group', id: 'c', position: 'before' },
      ),
    ).toBeNull();
  });

  it('refuses invalid combinations and unknown ids', () => {
    expect(
      planMove(
        snapshot,
        { type: 'group', id: 'a' },
        { type: 'profile', id: 'r0', position: 'before' },
      ),
    ).toBeNull();
    expect(
      planMove(
        snapshot,
        { type: 'profile', id: 'r0' },
        { type: 'profile', id: 'a0', position: 'into' },
      ),
    ).toBeNull();
    expect(planMove(snapshot, { type: 'profile', id: 'nope' }, { type: 'root' })).toBeNull();
    expect(
      planMove(
        snapshot,
        { type: 'profile', id: 'r0' },
        { type: 'group', id: 'nope', position: 'into' },
      ),
    ).toBeNull();
  });
});
