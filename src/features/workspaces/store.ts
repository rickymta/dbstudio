// Workspaces store: the list (localStorage `dbstudio.workspaces`), the active one, and the glue to
// the rest of the app. Activating a workspace swaps the tab set (`useTabsStore.switchScope`); the
// connections sidebar reads `activeWorkspace()` to show only that workspace's connections.

import { create } from 'zustand';
import { toast } from '@/components/ui/toastStore';
import i18n from '@/lib/i18n';
import type { Uuid } from '@/lib/types';
import { useSessionsStore } from '@/stores/sessions';
import { NO_SCOPE, useTabsStore } from '@/stores/tabs';
import * as M from './model';

const STORAGE_KEY = 'dbstudio.workspaces';

function read(): M.Workspaces {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? 'null');
    if (!raw || typeof raw !== 'object') return { items: [], active: M.ROOT };
    const r = raw as { items?: unknown; active?: unknown };
    const items = Array.isArray(r.items)
      ? r.items
          .filter((w): w is M.Workspace => !!w && typeof (w as M.Workspace).id === 'string')
          .map((w) => M.sanitize(w))
      : [];
    const active =
      typeof r.active === 'string' && items.some((w) => w.id === r.active) ? r.active : M.ROOT;
    return { items, active };
  } catch {
    return { items: [], active: M.ROOT };
  }
}

function write(data: M.Workspaces): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
  } catch {
    // storage unavailable: the change lasts until the app closes
  }
}

interface WorkspacesState extends M.Workspaces {
  /** Folder of the active workspace shown in its overview, where new connections go. */
  folder: string;
  setFolder: (folder: string) => void;
  /** Creates a workspace and makes it active. */
  create: (name: string, color: string, connections: Uuid[]) => M.Workspace;
  update: (id: string, patch: Partial<Pick<M.Workspace, 'name' | 'color'>>) => void;
  /** Deletes a workspace; its tabs join the tabs of no workspace. Connections are not touched. */
  remove: (id: string) => void;
  /** Makes `id` active ([`M.ROOT`] for none): the sidebar and the tab set follow. */
  activate: (id: string) => void;
  /** Applies a pure edit (see `model.ts`) to workspace `id`. */
  edit: (id: string, fn: (ws: M.Workspace) => M.Workspace) => void;
  /** Adds connections to the active workspace (into `folder`, else the shown folder). */
  addToActive: (ids: Uuid[], folder?: string) => number;
}

const initial = read();

export const useWorkspaces = create<WorkspacesState>((set, get) => ({
  ...initial,
  folder: M.ROOT,
  setFolder: (folder) => set({ folder }),
  create: (name, color, connections) => {
    const ws = M.sanitize({ id: M.newId(), name: name.trim(), color, connections });
    set((s) => ({ items: [...s.items, ws] }));
    get().activate(ws.id);
    return ws;
  },
  update: (id, patch) =>
    set((s) => ({ items: s.items.map((w) => (w.id === id ? { ...w, ...patch } : w)) })),
  remove: (id) => {
    if (get().active === id) get().activate(M.ROOT);
    set((s) => ({ items: s.items.filter((w) => w.id !== id) }));
    useTabsStore.getState().dropScope(id);
  },
  activate: (id) => {
    const next = id && get().items.some((w) => w.id === id) ? id : M.ROOT;
    set({ active: next, folder: M.ROOT });
    useTabsStore.getState().switchScope(next || NO_SCOPE);
  },
  edit: (id, fn) => set((s) => ({ items: s.items.map((w) => (w.id === id ? fn(w) : w)) })),
  addToActive: (ids, folder) => {
    const { active, items } = get();
    const ws = items.find((w) => w.id === active);
    if (!ws) return 0;
    const next = M.addConnections(ws, ids, folder ?? get().folder);
    get().edit(ws.id, () => next);
    return next.connections.length - ws.connections.length;
  },
}));

/** The active workspace, or `null` for none. */
export function activeWorkspace(
  state: Pick<WorkspacesState, 'items' | 'active'>,
): M.Workspace | null {
  return state.items.find((w) => w.id === state.active) ?? null;
}

/** React hook: the active workspace, or `null`. */
export const useActiveWorkspace = (): M.Workspace | null =>
  useWorkspaces((s) => activeWorkspace(s));

useWorkspaces.subscribe((s, old) => {
  if (s.items !== old.items || s.active !== old.active) write({ items: s.items, active: s.active });
});

// The tab set on screen at start-up is the active workspace's.
if (initial.active) useTabsStore.getState().switchScope(initial.active);

// A tab set left behind by a workspace that no longer exists (e.g. restored from an older save)
// joins the tabs of no workspace instead of staying hidden for good.
useTabsStore.subscribe((s) => {
  const known = new Set(useWorkspaces.getState().items.map((w) => w.id));
  for (const scope of Object.keys(s.stashed)) {
    if (scope !== NO_SCOPE && !known.has(scope)) useTabsStore.getState().dropScope(scope);
  }
});

// A connection opened while a workspace is active (from the command palette, an editor's
// connection picker…) joins that workspace, like a repository opened in GitStudio does.
let previousSessions = new Set(Object.keys(useSessionsStore.getState().sessionsByConnection));
useSessionsStore.subscribe((s) => {
  const now = Object.keys(s.sessionsByConnection);
  const fresh = now.filter((id) => !previousSessions.has(id));
  previousSessions = new Set(now);
  const ws = activeWorkspace(useWorkspaces.getState());
  if (!ws || fresh.length === 0) return;
  const joining = fresh.filter((id) => !ws.connections.includes(id));
  if (joining.length === 0) return;
  useWorkspaces.getState().addToActive(joining);
  toast.info(i18n.t('workspaces:toast.joined', { count: joining.length, name: ws.name }));
});
