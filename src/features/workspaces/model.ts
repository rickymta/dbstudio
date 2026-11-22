// Workspaces: named, coloured groups of connection profiles. Each one has its own tree of folders
// (at most MAX_DEPTH levels) and, while it is active, its own set of tabs and its own list of
// connections in the sidebar. A connection not placed in any folder sits at the root ('').

import type { Uuid } from '@/lib/types';

export const MAX_DEPTH = 3;
export const ROOT = '';

export const WORKSPACE_COLORS = [
  '#38bdf8',
  '#a78bfa',
  '#f472b6',
  '#fb923c',
  '#facc15',
  '#4ade80',
  '#2dd4bf',
  '#f87171',
] as const;

export interface WorkspaceFolder {
  id: string;
  name: string;
  /** Parent folder id; [`ROOT`] for the workspace's root. */
  parent: string;
  collapsed: boolean;
}

export interface Workspace {
  id: string;
  name: string;
  /** CSS colour. */
  color: string;
  /** Connection profile ids, in the order they were added. */
  connections: Uuid[];
  folders: WorkspaceFolder[];
  /** Connection id → folder id; a connection not listed sits at the root. */
  placement: Record<Uuid, string>;
}

export interface Workspaces {
  items: Workspace[];
  /** Id of the active workspace; [`ROOT`] (empty) for none. */
  active: string;
}

/** Short random id (8 base-36 characters). */
export const newId = (): string => Math.random().toString(36).slice(2, 10).padEnd(8, '0');

const byId = (folders: WorkspaceFolder[]) => new Map(folders.map((f) => [f.id, f]));

/** 1 for a folder at the root, 2 for one inside it…; 0 for the root itself. */
export function depthOf(folders: WorkspaceFolder[], id: string): number {
  const map = byId(folders);
  let depth = 0;
  for (let f = map.get(id); f && depth <= folders.length; f = map.get(f.parent)) depth++;
  return depth;
}

/** Levels in the subtree under `id`, the folder itself included (1 for a leaf). */
export function heightOf(folders: WorkspaceFolder[], id: string): number {
  const kids = folders.filter((f) => f.parent === id);
  return 1 + Math.max(0, ...kids.map((k) => heightOf(folders, k.id)));
}

/** `id` and every folder under it. */
export function subtree(folders: WorkspaceFolder[], id: string): Set<string> {
  const out = new Set([id]);
  for (let grew = true; grew;) {
    grew = false;
    for (const f of folders)
      if (out.has(f.parent) && !out.has(f.id)) {
        out.add(f.id);
        grew = true;
      }
  }
  return out;
}

/** Whether a new folder can go inside `parent`. */
export const canNest = (folders: WorkspaceFolder[], parent: string) =>
  depthOf(folders, parent) < MAX_DEPTH;

/** Whether folder `id` can move into `parent`: not into itself, and staying within MAX_DEPTH. */
export function canMove(folders: WorkspaceFolder[], id: string, parent: string): boolean {
  if (id === parent || (parent && subtree(folders, id).has(parent))) return false;
  return depthOf(folders, parent) + heightOf(folders, id) <= MAX_DEPTH;
}

/** The folder a connection sits in ([`ROOT`] also when its folder is gone). */
export function folderOf(ws: Workspace, connectionId: Uuid): string {
  const id = ws.placement[connectionId] ?? ROOT;
  return id && ws.folders.some((f) => f.id === id) ? id : ROOT;
}

/** The connections directly in `folder`, or in it and every folder under it. */
export function connectionsIn(ws: Workspace, folder: string, deep = false): Uuid[] {
  if (deep && !folder) return ws.connections;
  const set = deep ? subtree(ws.folders, folder) : new Set([folder]);
  return ws.connections.filter((c) => set.has(folderOf(ws, c)));
}

/** Folders under `parent`, sorted by name. */
export const childrenOf = (folders: WorkspaceFolder[], parent: string) =>
  folders
    .filter((f) => f.parent === parent)
    .sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }));

/** The folders from the root down to `id`. */
export function pathTo(folders: WorkspaceFolder[], id: string): WorkspaceFolder[] {
  const map = byId(folders);
  const out: WorkspaceFolder[] = [];
  for (let f = map.get(id); f && out.length <= folders.length; f = map.get(f.parent))
    out.unshift(f);
  return out;
}

/**
 * Repairs what older or hand-edited data may hold: unknown fields get defaults, folders whose
 * parent is gone go to the root, cycles are broken, folders past MAX_DEPTH move up, duplicate
 * connections are dropped, and placements of connections no longer in the workspace (or into
 * folders that are gone) are dropped.
 */
export function sanitize(raw: Partial<Workspace> & { id: string }): Workspace {
  const str = (v: unknown, d = '') => (typeof v === 'string' ? v : d);
  const ws: Workspace = {
    id: raw.id,
    name: str(raw.name, 'Workspace'),
    color: str(raw.color, WORKSPACE_COLORS[0]),
    connections: Array.isArray(raw.connections)
      ? [...new Set(raw.connections.filter((c): c is string => typeof c === 'string'))]
      : [],
    folders: Array.isArray(raw.folders)
      ? raw.folders
          .filter((f) => f && typeof f.id === 'string' && f.id)
          .map((f) => ({
            id: f.id,
            name: str(f.name, 'Folder'),
            parent: str(f.parent),
            collapsed: f.collapsed === true,
          }))
      : [],
    placement: raw.placement && typeof raw.placement === 'object' ? { ...raw.placement } : {},
  };
  const seen = new Set<string>();
  let folders = ws.folders.filter((f) => !seen.has(f.id) && seen.add(f.id));
  const ids = new Set(folders.map((f) => f.id));
  folders = folders.map((f) =>
    f.parent && ids.has(f.parent) && f.parent !== f.id ? f : { ...f, parent: ROOT },
  );
  // Break cycles: a folder whose ancestry never reaches the root goes to the root.
  folders = folders.map((f) => {
    const map = byId(folders);
    let cur: WorkspaceFolder | undefined = f;
    for (let steps = 0; cur && cur.parent; steps++) {
      if (steps > folders.length) return { ...f, parent: ROOT };
      cur = map.get(cur.parent);
    }
    return f;
  });
  // Too deep: hang it under its ancestor at MAX_DEPTH - 1.
  folders = folders.map((f) => {
    const chain = pathTo(folders, f.id);
    return chain.length > MAX_DEPTH ? { ...f, parent: chain[MAX_DEPTH - 2]!.id } : f;
  });
  const valid = new Set(folders.map((f) => f.id));
  const members = new Set(ws.connections);
  const placement: Record<string, string> = {};
  for (const [k, v] of Object.entries(ws.placement))
    if (typeof v === 'string' && v && valid.has(v) && members.has(k)) placement[k] = v;
  return { ...ws, folders, placement };
}

// ---- edits (pure: return the new workspace) ------------------------------------------------

export function addFolder(ws: Workspace, parent: string, name: string, id: string): Workspace {
  if (!canNest(ws.folders, parent)) return ws;
  return { ...ws, folders: [...ws.folders, { id, name: name.trim(), parent, collapsed: false }] };
}

export function patchFolder(ws: Workspace, id: string, patch: Partial<WorkspaceFolder>): Workspace {
  return { ...ws, folders: ws.folders.map((f) => (f.id === id ? { ...f, ...patch, id } : f)) };
}

export function moveFolder(ws: Workspace, id: string, parent: string): Workspace {
  if (!canMove(ws.folders, id, parent)) return ws;
  return patchFolder(ws, id, { parent });
}

/** Deletes a folder; what it held (folders and connections) moves up to its parent. */
export function deleteFolder(ws: Workspace, id: string): Workspace {
  const gone = ws.folders.find((f) => f.id === id);
  if (!gone) return ws;
  const placement: Record<string, string> = {};
  for (const [k, v] of Object.entries(ws.placement))
    if (v !== id) placement[k] = v;
    else if (gone.parent) placement[k] = gone.parent;
  return {
    ...ws,
    folders: ws.folders
      .filter((f) => f.id !== id)
      .map((f) => (f.parent === id ? { ...f, parent: gone.parent } : f)),
    placement,
  };
}

export function moveConnections(ws: Workspace, ids: Uuid[], folder: string): Workspace {
  const placement = { ...ws.placement };
  for (const id of ids)
    if (folder) placement[id] = folder;
    else delete placement[id];
  return { ...ws, placement };
}

/** Adds connections not yet in the workspace, placed into `folder`. */
export function addConnections(ws: Workspace, ids: Uuid[], folder = ROOT): Workspace {
  const fresh = ids.filter((id, i) => !ws.connections.includes(id) && ids.indexOf(id) === i);
  if (fresh.length === 0) return ws;
  const placed = { ...ws, connections: [...ws.connections, ...fresh] };
  return folder && ws.folders.some((f) => f.id === folder)
    ? moveConnections(placed, fresh, folder)
    : placed;
}

export function removeConnection(ws: Workspace, id: Uuid): Workspace {
  const placement = { ...ws.placement };
  delete placement[id];
  return { ...ws, connections: ws.connections.filter((c) => c !== id), placement };
}
