import { create } from 'zustand';
import { pathKey } from '@/lib/objectPath';
import type { ObjectPath } from '@/lib/types';
import { useTabsStore, tableTabId, ddlTabId, keyTabId, type TabItem } from '@/stores/tabs';
import { useSessionsStore } from '@/stores/sessions';

export interface SavedObject {
  key: string;
  connectionId: string;
  path: ObjectPath;
  kind: 'table' | 'ddl' | 'key';
  name: string;
}
const KEY = 'dbstudio.navigation';
function read(): { recent: SavedObject[]; favorites: SavedObject[] } {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? '{}');
    const valid = (items: unknown): SavedObject[] =>
      Array.isArray(items)
        ? items
            .filter(
              (x) =>
                x &&
                typeof x.key === 'string' &&
                typeof x.connectionId === 'string' &&
                typeof x.name === 'string' &&
                ['table', 'ddl', 'key'].includes(x.kind) &&
                Array.isArray(x.path?.segments) &&
                x.path.segments.every(
                  (s: { kind?: unknown; name?: unknown }) =>
                    typeof s.kind === 'string' && typeof s.name === 'string',
                ),
            )
            .slice(0, 50)
        : [];
    return { recent: valid(raw.recent), favorites: valid(raw.favorites) };
  } catch {
    return { recent: [], favorites: [] };
  }
}
export function savedObject(tab: TabItem): SavedObject | null {
  if (tab.kind !== 'table' && tab.kind !== 'ddl' && tab.kind !== 'key') return null;
  const { connectionId, path } = tab.payload;
  return {
    key: `${connectionId}:${tab.kind}:${pathKey(path)}`,
    connectionId,
    path,
    kind: tab.kind,
    name: path.segments.at(-1)?.name ?? '',
  };
}
interface Navigation {
  recent: SavedObject[];
  favorites: SavedObject[];
  request: { connectionId?: string; object?: SavedObject; create?: boolean; nonce: number } | null;
  visit: (object: SavedObject) => void;
  toggle: (object: SavedObject) => void;
  navigate: (connectionId: string, object?: SavedObject) => void;
  newConnection: () => void;
}
export const useNavigation = create<Navigation>((set) => ({
  ...read(),
  request: null,
  visit: (object) =>
    set((s) => ({
      recent: [object, ...s.recent.filter((x) => x.key !== object.key)].slice(0, 12),
    })),
  toggle: (object) =>
    set((s) => ({
      favorites: s.favorites.some((x) => x.key === object.key)
        ? s.favorites.filter((x) => x.key !== object.key)
        : [...s.favorites, object].slice(-50),
    })),
  navigate: (connectionId, object) =>
    set({ request: { connectionId, object, nonce: Date.now() + Math.random() } }),
  newConnection: () => set({ request: { create: true, nonce: Date.now() + Math.random() } }),
}));
useNavigation.subscribe((s, old) => {
  if (s.recent === old.recent && s.favorites === old.favorites) return;
  try {
    localStorage.setItem(KEY, JSON.stringify({ recent: s.recent, favorites: s.favorites }));
  } catch {
    /* storage unavailable */
  }
});
useTabsStore.subscribe((s, old) => {
  if (s.activeId === old.activeId && s.tabs === old.tabs) return;
  const tab = s.tabs.find((x) => x.id === s.activeId);
  const object = tab && savedObject(tab);
  if (object) useNavigation.getState().visit(object);
});
export function openSavedObject(object: SavedObject): boolean {
  const session = useSessionsStore.getState().sessionsByConnection[object.connectionId];
  if (!session) return false;
  const payload = {
    sessionId: session.sessionId,
    connectionId: object.connectionId,
    path: object.path,
  };
  const id = (object.kind === 'table' ? tableTabId : object.kind === 'ddl' ? ddlTabId : keyTabId)(
    session.sessionId,
    object.path,
  );
  useTabsStore
    .getState()
    .openTab({ id, kind: object.kind, title: object.name, closable: true, payload });
  return true;
}
