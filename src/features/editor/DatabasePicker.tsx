import { useQuery } from '@tanstack/react-query';
import { ChevronDown, Database } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from '@/components/ui/DropdownMenu';
import { useToastStore } from '@/components/ui/toastStore';
import { ipc, toIpcError } from '@/lib/ipc';
import type { Uuid } from '@/lib/types';
import { filterKeyDown, listDatabases, switchBlock, useSwitchBlock } from './pickerLogic';
import { getTabData, patchTabData, useTabData } from './editorStore';

/**
 * "Active catalog" of a SQL editor: the database its statements run in. Shown only when the
 * session's object tree has databases at its root (otherwise there is nothing to pick).
 */
export function DatabasePicker({
  tabStateId,
  sessionId,
  defaultDatabase,
  onAvailable,
}: {
  tabStateId: string;
  sessionId: Uuid;
  /** Database of the connection profile, shown while the tab has none of its own. */
  defaultDatabase: string | null;
  /** Told whether the picker is shown, so the toolbar does not repeat the database. */
  onAvailable?: (shown: boolean) => void;
}) {
  const { t } = useTranslation('editor');
  const data = useTabData(tabStateId);
  const blocked = useSwitchBlock(tabStateId);
  const [filter, setFilter] = useState('');
  const toast = useToastStore.getState().push;
  const { data: databases } = useQuery({
    queryKey: ['editorDatabases', sessionId],
    queryFn: () => listDatabases(sessionId),
    staleTime: 30_000,
    retry: false,
  });
  const shownPicker = !!data && !!databases && databases.length > 0;
  useEffect(() => {
    onAvailable?.(shownPicker);
    return () => onAvailable?.(false);
  }, [onAvailable, shownPicker]);
  if (!data || !databases || databases.length === 0) return null;

  const current = data.leaseState?.database ?? data.database ?? defaultDatabase;
  const needle = filter.trim().toLowerCase();
  const shown = databases.filter((d) => d.toLowerCase().includes(needle));
  const disabledHint = blocked ? t(`picker.${blocked}`) : null;

  const choose = async (database: string) => {
    // Read the tab now, not from the render that built the menu: a lease that opened since then
    // must get the new context too, or the editor would show one database and run in another.
    const live = getTabData(tabStateId);
    if (!live) return;
    if (database === (live.leaseState?.database ?? live.database ?? defaultDatabase)) return;
    const block = switchBlock(tabStateId);
    if (block) return void toast('info', t(`picker.${block}`));
    const leaseId = live.leaseId;
    try {
      if (leaseId) {
        const leaseState = await ipc.leaseSetContext(leaseId, { database, schema: null });
        patchTabData(tabStateId, { leaseState });
      }
      patchTabData(tabStateId, { database, schema: null });
    } catch (e) {
      toast('error', t('picker.databaseFailed', { message: toIpcError(e).message }));
    }
  };

  return (
    <DropdownMenu onOpenChange={(open) => open && setFilter('')}>
      <DropdownMenuTrigger
        disabled={blocked !== null}
        aria-label={t('picker.database')}
        title={disabledHint ?? t('picker.database')}
        data-testid="database-picker"
        className="flex min-w-0 items-center gap-1.5 rounded px-1.5 py-1 text-[13px] hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-60"
      >
        <Database size={13} aria-hidden className="shrink-0 text-muted-foreground" />
        <span className="max-w-48 truncate">{current ?? t('picker.databaseDefault')}</span>
        <ChevronDown size={13} aria-hidden className="shrink-0 text-muted-foreground" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-80 w-64 overflow-y-auto">
        <input
          type="search"
          autoFocus
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onKeyDown={filterKeyDown}
          aria-label={t('picker.databaseFilter')}
          placeholder={t('picker.databaseFilter')}
          className="mb-1 h-7 w-full rounded border border-border bg-surface px-2 text-[13px] outline-none focus:border-accent"
        />
        {shown.length === 0 && (
          <div className="px-2 py-1.5 text-xs text-muted-foreground">
            {t('picker.databaseNone')}
          </div>
        )}
        <DropdownMenuRadioGroup value={current ?? ''} onValueChange={(v) => void choose(v)}>
          {shown.map((d) => (
            <DropdownMenuRadioItem key={d} value={d}>
              <span className="min-w-0 truncate">{d}</span>
            </DropdownMenuRadioItem>
          ))}
        </DropdownMenuRadioGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
