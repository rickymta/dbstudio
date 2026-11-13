import type { ConnectionGroup, ConnectionProfile, ConnectionsSnapshot, Uuid } from '@/lib/types';

export interface GroupNode {
  group: ConnectionGroup;
  groups: GroupNode[];
  profiles: ConnectionProfile[];
}

export interface ConnectionsTree {
  groups: GroupNode[];
  /** Profiles without a (known) group. */
  profiles: ConnectionProfile[];
}

const byOrder = <T extends { sortOrder: number; name: string }>(a: T, b: T) =>
  a.sortOrder - b.sortOrder || a.name.localeCompare(b.name);

/**
 * Nest groups and profiles. Groups whose parent is missing (or that would form a cycle) are
 * promoted to the root, profiles of unknown groups to the root list.
 */
export function buildConnectionsTree(snapshot: ConnectionsSnapshot): ConnectionsTree {
  const known = new Set(snapshot.groups.map((g) => g.id));
  const nodes = new Map<Uuid, GroupNode>(
    snapshot.groups.map((g) => [g.id, { group: g, groups: [], profiles: [] }]),
  );

  const hasCycle = (g: ConnectionGroup): boolean => {
    const seen = new Set<Uuid>([g.id]);
    let parent = g.parentId ?? null;
    while (parent && known.has(parent)) {
      if (seen.has(parent)) return true;
      seen.add(parent);
      parent = nodes.get(parent)!.group.parentId ?? null;
    }
    return false;
  };

  const roots: GroupNode[] = [];
  for (const g of [...snapshot.groups].sort(byOrder)) {
    const node = nodes.get(g.id)!;
    const parent =
      g.parentId && known.has(g.parentId) && !hasCycle(g) ? nodes.get(g.parentId) : null;
    (parent ? parent.groups : roots).push(node);
  }

  const rootProfiles: ConnectionProfile[] = [];
  for (const p of [...snapshot.profiles].sort(byOrder)) {
    const node = p.groupId ? nodes.get(p.groupId) : undefined;
    (node ? node.profiles : rootProfiles).push(p);
  }
  return { groups: roots, profiles: rootProfiles };
}

/** Keep profiles accepted by `keepProfile` and groups that match by name or contain a kept item. */
export function filterConnectionsTree(
  tree: ConnectionsTree,
  filter: string,
  keepProfile: (p: ConnectionProfile) => boolean,
): ConnectionsTree {
  const f = filter.trim().toLowerCase();
  if (!f) return tree;
  const filterGroup = (node: GroupNode, ancestorMatched: boolean): GroupNode | null => {
    const selfMatches = node.group.name.toLowerCase().includes(f);
    const all = ancestorMatched || selfMatches;
    const groups = node.groups
      .map((g) => filterGroup(g, all))
      .filter((g): g is GroupNode => g !== null);
    const profiles = node.profiles.filter((p) => all || keepProfile(p));
    return all || groups.length > 0 || profiles.length > 0 ? { ...node, groups, profiles } : null;
  };
  return {
    groups: tree.groups.map((g) => filterGroup(g, false)).filter((g): g is GroupNode => g !== null),
    profiles: tree.profiles.filter(keepProfile),
  };
}

/** Flat `Parent / Child` labels for every group, ordered as in the tree (for select options). */
export function groupPathOptions(snapshot: ConnectionsSnapshot): { id: Uuid; label: string }[] {
  const out: { id: Uuid; label: string }[] = [];
  const walk = (nodes: GroupNode[], prefix: string) => {
    for (const n of nodes) {
      const label = prefix ? `${prefix} / ${n.group.name}` : n.group.name;
      out.push({ id: n.group.id, label });
      walk(n.groups, label);
    }
  };
  walk(buildConnectionsTree({ groups: snapshot.groups, profiles: [] }).groups, '');
  return out;
}

/** Ids of every group in the snapshot. */
export const allGroupIds = (snapshot: ConnectionsSnapshot): Uuid[] =>
  snapshot.groups.map((g) => g.id);

/** A profile needs a password prompt when none is stored (CONN-08). */
export const needsPassword = (p: ConnectionProfile): boolean => !p.savePassword || !p.hasPassword;
