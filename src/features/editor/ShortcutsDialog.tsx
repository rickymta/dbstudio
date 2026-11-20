import { Search } from 'lucide-react';
import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { BigWindow } from '@/components/ui/BigWindow';
import { useDebugStore } from '@/features/debugger/debugStore';
import { DEBUG_COMMANDS, bindingsOf, formatCombo } from '@/features/debugger/keymap';
import { SHORTCUT_GROUPS, SHORTCUTS, formatKeys, keysOf } from './shortcuts';
import { useShortcutsHelp } from './shortcutsHelpStore';

interface Row {
  id: string;
  label: string;
  /** Alternatives, already formatted for this platform. */
  combos: string[];
}

interface Group {
  id: string;
  title: string;
  rows: Row[];
}

/** Keyboard shortcuts (APP-04): grouped, searchable; the debugger group follows its keymap. */
export function ShortcutsDialog() {
  const { open, setOpen } = useShortcutsHelp();
  return open ? <ShortcutsWindow onOpenChange={setOpen} /> : null;
}

function ShortcutsWindow({ onOpenChange }: { onOpenChange: (open: boolean) => void }) {
  const { t } = useTranslation('editor');
  const keymap = useDebugStore((s) => s.keymap);
  const [query, setQuery] = useState('');

  const groups = useMemo<Group[]>(() => {
    const all: Group[] = [
      ...SHORTCUT_GROUPS.map((group) => ({
        id: group,
        title: t(`shortcuts.group.${group}`),
        rows: SHORTCUTS.filter((s) => s.group === group).map((s) => ({
          id: s.id,
          label: t(`shortcuts.action.${s.id}`),
          combos: [s.keys, ...(s.alt ?? [])].map((k) => formatKeys(k)),
        })),
      })),
      {
        id: 'debugger',
        title: t('shortcuts.group.debugger', { keymap: t(`shortcuts.keymap.${keymap}`) }),
        rows: DEBUG_COMMANDS.map((command) => ({
          id: command,
          label: t(`debugger:command.${command}`),
          combos: bindingsOf(command, keymap).map((c) => formatCombo(c)),
        })).filter((r) => r.combos.length > 0),
      },
    ];
    const needle = query.trim().toLowerCase();
    if (!needle) return all;
    return all
      .map((g) => ({
        ...g,
        rows: g.rows.filter(
          (r) =>
            r.label.toLowerCase().includes(needle) ||
            r.combos.some((c) => c.toLowerCase().includes(needle)),
        ),
      }))
      .filter((g) => g.rows.length > 0);
  }, [t, keymap, query]);

  return (
    <BigWindow
      open
      onOpenChange={onOpenChange}
      width="min(720px, calc(100vw - 32px))"
      height="min(680px, calc(100vh - 64px))"
      title={t('shortcuts.title')}
      headerExtra={
        <div className="relative flex items-center gap-3">
          <Keys combo={formatKeys(keysOf('help'))} />
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('shortcuts.filter')}
            aria-label={t('shortcuts.filter')}
            className="h-7 w-52 rounded border border-border bg-background pr-7 pl-2 text-xs outline-none focus:border-accent"
          />
          <Search
            size={13}
            aria-hidden
            className="pointer-events-none absolute top-1.5 right-2 text-muted-foreground"
          />
        </div>
      }
    >
      <div className="min-h-0 flex-1 overflow-auto px-6 py-4">
        {groups.length === 0 && (
          <p className="py-10 text-center text-muted-foreground">{t('shortcuts.noMatch')}</p>
        )}
        {groups.map((g) => (
          <section key={g.id} className="mb-5" aria-label={g.title}>
            <h3 className="mb-1 border-b border-border pb-1.5 text-[13px] font-semibold">
              {g.title}
            </h3>
            {g.rows.map((r) => (
              <div
                key={r.id}
                className="flex min-h-8 items-center gap-3 border-b border-border/50 text-[13px] last:border-0"
                data-testid="shortcut-row"
              >
                <span className="min-w-0 flex-1">{r.label}</span>
                <span className="flex shrink-0 items-center gap-1.5">
                  {r.combos.map((combo, n) => (
                    <span key={combo} className="flex items-center gap-1.5">
                      {n > 0 && (
                        <span className="text-xs text-muted-foreground">{t('shortcuts.or')}</span>
                      )}
                      <Keys combo={combo} />
                    </span>
                  ))}
                </span>
              </div>
            ))}
          </section>
        ))}
      </div>
    </BigWindow>
  );
}

function Keys({ combo }: { combo: string }) {
  return (
    <kbd className="rounded border border-border bg-surface-muted px-1.5 py-0.5 font-mono text-xs font-normal">
      {combo}
    </kbd>
  );
}
