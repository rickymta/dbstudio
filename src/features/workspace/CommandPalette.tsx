import { Dialog as DialogPrimitive } from 'radix-ui';
import { Search } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { openBackup } from '@/features/backup/open';
import { Spinner } from '@/components/ui/Spinner';
import { useConnectionsSnapshot } from '@/features/connections/queries';
import {
  groupResultsByKind,
  kindGroupLabelKey,
  resultContextParts,
  SERVER_SEARCH_MIN_CHARS,
  useSearchableSessions,
} from '@/features/connections/serverSearchModel';
import { useServerSearch } from '@/features/connections/useServerSearch';
import { KindIcon } from '@/features/explorer/kindIcons';
import { openObjectTab } from '@/features/explorer/openObject';
import { useRevealStore } from '@/features/explorer/revealStore';
import { pathKey } from '@/lib/objectPath';
import { useDebounced } from '@/lib/useDebounced';
import { openEditorForActiveTab } from '@/features/editor/open';
import { formatKeys, keysOf } from '@/features/editor/shortcuts';
import { useShortcutsHelp } from '@/features/editor/shortcutsHelpStore';
import { cn } from '@/lib/cn';
import { setLanguage } from '@/lib/i18n';
import { useTabsStore } from '@/stores/tabs';
import { THEME_MODES, useThemeStore } from '@/stores/theme';
import {
  filterCommands,
  useActivityWindow,
  useCommandPalette,
  useHelpDialog,
  useSettingsDialog,
  type PaletteCommand,
} from './paletteStore';
import { useWorkspaceStore } from './layout';
import { useNavigation } from './navigation';

/** Rows rendered at most; the list is filtered by typing, not by scrolling through hundreds. */
const MAX_ROWS = 60;

/** Pause after the last key before the open connections are asked for matching objects. */
const SERVER_SEARCH_DELAY_MS = 250;

/** Objects of one kind listed at most, so that every kind with a match stays in sight. */
const OBJECTS_PER_KIND = 8;

/**
 * Command palette / Spotlight (APP-08): application actions, open tabs, connections, recent
 * objects, and the tables, views, functions... of every open connection, searched by name on
 * the server (`object_search`).
 */
export function CommandPalette() {
  const open = useCommandPalette((s) => s.open);
  const setOpen = useCommandPalette((s) => s.setOpen);
  return (
    <DialogPrimitive.Root open={open} onOpenChange={setOpen}>
      {open && <PaletteBody onClose={() => setOpen(false)} />}
    </DialogPrimitive.Root>
  );
}

function useCommands(): PaletteCommand[] {
  const { t, i18n } = useTranslation();
  const tabs = useTabsStore((s) => s.tabs);
  const activeId = useTabsStore((s) => s.activeId);
  const { data } = useConnectionsSnapshot();
  const recent = useNavigation((s) => s.recent);
  const favorites = useNavigation((s) => s.favorites);
  const mode = useThemeStore((s) => s.mode);

  return useMemo(() => {
    const language = i18n.language.startsWith('vi') ? 'vi' : 'en';
    const other = language === 'vi' ? 'en' : 'vi';
    const profiles = data?.profiles ?? [];
    const profileName = (id: string) => profiles.find((p) => p.id === id)?.name;

    const actions: PaletteCommand[] = [
      {
        id: 'newQuery',
        group: 'action',
        label: t('workspace.newQuery'),
        keys: formatKeys(keysOf('newTab')),
        run: () => void openEditorForActiveTab(),
      },
      {
        id: 'newConnection',
        group: 'action',
        label: t('palette.newConnection'),
        run: () => {
          useWorkspaceStore.getState().showSidebar();
          useNavigation.getState().newConnection();
        },
      },
      {
        id: 'settings',
        group: 'action',
        label: t('settings:open'),
        keys: formatKeys('Mod-,'),
        run: () => useSettingsDialog.getState().setOpen(true),
      },
      {
        id: 'backup',
        group: 'action',
        label: t('palette.backup'),
        run: openBackup,
      },
      {
        id: 'toggleSidebar',
        group: 'action',
        label: t('workspace.toggleSidebar'),
        keys: formatKeys(keysOf('toggleSidebar')),
        run: () => useWorkspaceStore.getState().toggleSidebar(),
      },
      {
        id: 'shortcuts',
        group: 'action',
        label: t('palette.shortcuts'),
        keys: formatKeys(keysOf('help')),
        run: () => useShortcutsHelp.getState().setOpen(true),
      },
      {
        id: 'activity',
        group: 'action',
        label: t('palette.activity'),
        keys: formatKeys(keysOf('activity')),
        run: () => useActivityWindow.getState().setOpen(true),
      },
      {
        id: 'help',
        group: 'action',
        label: t('palette.help'),
        run: () => useHelpDialog.getState().setOpen(true),
      },
      ...THEME_MODES.filter((m) => m !== mode).map((m): PaletteCommand => ({
        id: `theme:${m}`,
        group: 'action',
        label: `${t('theme.label')}: ${t(`theme.${m}`)}`,
        run: () => useThemeStore.getState().setMode(m),
      })),
      {
        id: 'language',
        group: 'action',
        label: `${t('language.label')}: ${t(`language.${other}`)}`,
        run: () => setLanguage(other),
      },
    ];

    const tabCommands = tabs
      .filter((tab) => tab.id !== activeId)
      .map((tab): PaletteCommand => {
        const title =
          typeof tab.title === 'string'
            ? tab.title
            : t(tab.title.key, { ns: tab.title.ns ?? 'common' });
        return {
          id: `tab:${tab.id}`,
          group: 'tab',
          label: title,
          detail: tab.payload?.connectionId ? profileName(tab.payload.connectionId) : undefined,
          run: () => useTabsStore.getState().activate(tab.id),
        };
      });

    const connections = profiles.map((p): PaletteCommand => ({
      id: `connection:${p.id}`,
      group: 'connection',
      label: p.name,
      detail: p.environment ? t(`connections:environment.${p.environment}`) : undefined,
      run: () => {
        useWorkspaceStore.getState().showSidebar();
        useNavigation.getState().navigate(p.id);
      },
    }));

    const seen = new Set<string>();
    const objects = [...favorites, ...recent]
      .filter((o) => {
        if (seen.has(o.key) || !profileName(o.connectionId)) return false;
        seen.add(o.key);
        return true;
      })
      .map((o): PaletteCommand => ({
        id: `object:${o.key}`,
        group: 'object',
        label: o.name,
        detail: [
          profileName(o.connectionId),
          ...o.path.segments
            .slice(0, -1)
            .filter((s) => s.kind !== 'folder')
            .map((s) => s.name),
        ].join(' › '),
        objectKey: `${o.connectionId}:${pathKey(o.path)}`,
        run: () => useNavigation.getState().navigate(o.connectionId, o),
      }));

    return [...actions, ...tabCommands, ...connections, ...objects];
  }, [t, i18n.language, tabs, activeId, data, recent, favorites, mode]);
}

/**
 * Objects of the open connections whose name contains `query`, found on the server: the object
 * tree is loaded lazily, so a table or a function that was never expanded is only known there.
 */
function useServerObjects(query: string): { commands: PaletteCommand[]; fetching: boolean } {
  const { t } = useTranslation();
  const sessions = useSearchableSessions();
  const { data } = useConnectionsSnapshot();
  const typed = query.trim();
  const text = useDebounced(typed, SERVER_SEARCH_DELAY_MS);
  const enabled = text.length >= SERVER_SEARCH_MIN_CHARS;
  const results = useServerSearch(sessions, text, enabled);
  // Typed but not sent yet: for the user that is still "searching".
  const fetching =
    sessions.length > 0 &&
    typed.length >= SERVER_SEARCH_MIN_CHARS &&
    (text !== typed || results.some((r) => r.fetching));

  // Every connection together, split by kind: tables, views, functions, procedures...
  const found = sessions.flatMap((session, i) =>
    (results[i]?.data ?? []).map((node) => ({ node, session })),
  );
  const sessionOf = new Map(found.map((f) => [f.node, f.session]));
  const commands = groupResultsByKind(found.map((f) => f.node)).flatMap((group) => {
    const heading = t('palette.serverKind', { kind: t(kindGroupLabelKey(group.kind)) });
    return group.nodes.map((node): PaletteCommand => {
      const session = sessionOf.get(node)!;
      const connection =
        data?.profiles.find((p) => p.id === session.connectionId)?.name ??
        session.serverInfo.product;
      return {
        id: `server:${session.sessionId}:${pathKey(node.path)}`,
        group: 'server',
        heading,
        label: node.name,
        detail: [connection, ...resultContextParts(node)].join(' › '),
        kind: node.kind,
        objectKey: `${session.connectionId}:${pathKey(node.path)}`,
        run: () => {
          if (openObjectTab(node, session)) return;
          // No tab for this kind: show where it is in the tree instead.
          useWorkspaceStore.getState().showSidebar();
          useRevealStore.getState().reveal(session.connectionId, node.path);
        },
      };
    });
  });
  return { commands, fetching };
}

function PaletteBody({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const commands = useCommands();
  const [query, setQuery] = useState('');
  const [cursor, setCursor] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const server = useServerObjects(query);

  const local = filterCommands(commands, query);
  const known = new Set(local.map((c) => c.objectKey));
  // The answer may belong to the text of a moment ago: keep only what still matches, and skip
  // objects already listed as favorite or recent.
  const perKind = new Map<string, number>();
  const found = server.commands.filter((c) => {
    if (known.has(c.objectKey) || filterCommands([c], query).length === 0) return false;
    const n = (perKind.get(c.kind ?? '') ?? 0) + 1;
    perKind.set(c.kind ?? '', n);
    return n <= OBJECTS_PER_KIND;
  });
  const shown = [...local, ...found].slice(0, MAX_ROWS);
  const active = Math.min(cursor, Math.max(shown.length - 1, 0));

  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView?.({ block: 'nearest' });
  }, [active, shown.length, query]);

  const run = (command: PaletteCommand | undefined) => {
    if (!command) return;
    onClose();
    command.run();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setCursor(shown.length ? (active + 1) % shown.length : 0);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setCursor(shown.length ? (active - 1 + shown.length) % shown.length : 0);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      run(shown[active]);
    }
  };

  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Overlay className="fixed inset-0 z-40 bg-black/40" />
      <DialogPrimitive.Content
        aria-describedby={undefined}
        className="fixed top-[12vh] left-1/2 z-50 flex max-h-[70vh] w-[min(560px,92vw)] -translate-x-1/2 flex-col overflow-hidden rounded-lg border border-border bg-surface text-foreground shadow-xl"
        onKeyDown={onKeyDown}
      >
        <DialogPrimitive.Title className="sr-only">{t('palette.title')}</DialogPrimitive.Title>
        <div className="flex items-center gap-2 border-b border-border px-3">
          <Search size={14} className="shrink-0 text-muted-foreground" aria-hidden />
          <input
            autoFocus
            role="combobox"
            aria-expanded
            aria-controls="command-palette-list"
            aria-activedescendant={shown[active] ? `palette-${active}` : undefined}
            aria-label={t('palette.title')}
            placeholder={t('palette.placeholder')}
            // The box is the whole dialog: no focus ring of its own around the text.
            style={{ outline: 'none' }}
            className="h-10 min-w-0 flex-1 bg-transparent text-[13px] outline-none placeholder:text-muted-foreground"
            spellCheck={false}
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setCursor(0);
            }}
          />
        </div>
        <div
          ref={listRef}
          id="command-palette-list"
          role="listbox"
          aria-label={t('palette.title')}
          className="min-h-0 flex-1 overflow-y-auto p-1"
        >
          {shown.length === 0 && !server.fetching && (
            <p className="px-3 py-6 text-center text-xs text-muted-foreground">
              {t('palette.empty')}
            </p>
          )}
          {shown.map((command, index) => {
            const title = command.heading ?? t(`palette.group.${command.group}`);
            const previous = shown[index - 1];
            const heading =
              !previous || (previous.heading ?? t(`palette.group.${previous.group}`)) !== title;
            return (
              <div key={command.id}>
                {heading && (
                  <div className="px-2 pt-2 pb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
                    {title}
                  </div>
                )}
                <div
                  id={`palette-${index}`}
                  role="option"
                  aria-selected={index === active}
                  className={cn(
                    'flex min-h-8 cursor-default items-center gap-2 rounded px-2 text-[13px]',
                    index === active && 'bg-surface-muted',
                  )}
                  onMouseMove={() => setCursor(index)}
                  onClick={() => run(command)}
                >
                  {command.kind && (
                    <KindIcon kind={command.kind} className="shrink-0 text-muted-foreground" />
                  )}
                  <span className="min-w-0 flex-1 truncate">{command.label}</span>
                  {command.detail && (
                    <span className="max-w-[45%] truncate text-xs text-muted-foreground">
                      {command.detail}
                    </span>
                  )}
                  {command.keys && (
                    <kbd className="shrink-0 rounded border border-border bg-surface-muted px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
                      {command.keys}
                    </kbd>
                  )}
                </div>
              </div>
            );
          })}
          {server.fetching && (
            <p
              role="status"
              className="flex items-center justify-center gap-2 px-3 py-2 text-xs text-muted-foreground"
            >
              <Spinner size={11} />
              {t('palette.searching')}
            </p>
          )}
        </div>
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  );
}
