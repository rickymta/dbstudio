import { Dialog as DialogPrimitive } from 'radix-ui';
import { Search } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { openBackup } from '@/features/backup/open';
import { useConnectionsSnapshot } from '@/features/connections/queries';
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

/** Command palette (APP-08): application actions, open tabs, connections and recent objects. */
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
        run: () => useNavigation.getState().navigate(o.connectionId, o),
      }));

    return [...actions, ...tabCommands, ...connections, ...objects];
  }, [t, i18n.language, tabs, activeId, data, recent, favorites, mode]);
}

function PaletteBody({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const commands = useCommands();
  const [query, setQuery] = useState('');
  const [cursor, setCursor] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const shown = useMemo(
    () => filterCommands(commands, query).slice(0, MAX_ROWS),
    [commands, query],
  );
  const active = Math.min(cursor, Math.max(shown.length - 1, 0));

  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>('[aria-selected="true"]')
      ?.scrollIntoView?.({ block: 'nearest' });
  }, [active, shown]);

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
          {shown.length === 0 && (
            <p className="px-3 py-6 text-center text-xs text-muted-foreground">
              {t('palette.empty')}
            </p>
          )}
          {shown.map((command, index) => {
            const heading = shown[index - 1]?.group !== command.group;
            return (
              <div key={command.id}>
                {heading && (
                  <div className="px-2 pt-2 pb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
                    {t(`palette.group.${command.group}`)}
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
        </div>
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  );
}
